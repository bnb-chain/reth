use super::{DatabaseProviderRO, ProviderFactory, ProviderNodeTypes};
use crate::{
    providers::{StaticFileProvider, StaticFileProviderRWRefMut},
    to_range, BlockHashReader, BlockIdReader, BlockNumReader, BlockReader, BlockReaderIdExt,
    BlockSource, ChainSpecProvider, ChangeSetReader, HeaderProvider, ProviderError,
    PruneCheckpointReader, ReceiptProvider, ReceiptProviderIdExt, StageCheckpointReader,
    StaticFileProviderFactory, TransactionVariant, TransactionsProvider,
};
use alloy_consensus::{
    transaction::{TransactionMeta, TxHashRef},
    BlockHeader, Sealable,
};
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag, HashOrNumber};
use alloy_primitives::{Address, BlockHash, BlockNumber, TxHash, TxNumber, B256, U256};
use reth_chain_state::{BlockState, CanonicalInMemoryState};
use reth_chainspec::{ChainInfo, EthChainSpec};
use reth_db_api::{
    models::{AccountBeforeTx, BlockNumberAddress, StoredBlockBodyIndices},
    tables,
    transaction::DbTx,
};
use reth_execution_types::RecoveredBlockAndExecutionOutput;
use reth_node_types::{BlockTy, HeaderTy, ReceiptTy, TxTy};
use reth_primitives_traits::{
    Block as _, BlockBody, RecoveredBlock, SealedHeader, SealedOrRecoveredBlock, StorageEntry,
};
use reth_prune_types::{PruneCheckpoint, PruneSegment};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_static_file_types::StaticFileSegment;
use reth_storage_api::{
    BlockBodyIndicesProvider, DatabaseProviderFactory, NodePrimitivesProvider,
    StorageChangeSetReader,
};
use reth_storage_errors::provider::ProviderResult;
use revm::database::states::PlainStorageRevert;
use std::{
    ops::{Add, Bound, RangeBounds, RangeInclusive, Sub},
    sync::Arc,
};

/// Includes the initial memory search, ancestor visits and durable-row reads.
const MAX_EXACT_TD_STEPS: usize = 4096;

/// Type that interacts with a snapshot view of the blockchain (storage and in-memory) at time of
/// instantiation, EXCEPT for pending, safe and finalized block which might change while holding
/// this provider.
///
/// CAUTION: Avoid holding this provider for too long or the inner database transaction will
/// time-out.
#[derive(Debug)]
#[doc(hidden)] // triggers ICE for `cargo docs`
pub struct ConsistentProvider<N: ProviderNodeTypes> {
    /// Storage provider.
    storage_provider: <ProviderFactory<N> as DatabaseProviderFactory>::Provider,
    /// Head block at time of [`Self`] creation
    head_block: Option<Arc<BlockState<N::Primitives>>>,
    /// In-memory canonical state. This is not a snapshot, and can change! Use with caution.
    canonical_in_memory_state: CanonicalInMemoryState<N::Primitives>,
}

impl<N: ProviderNodeTypes> ConsistentProvider<N> {
    /// Create a new provider using [`ProviderFactory`] and [`CanonicalInMemoryState`],
    ///
    /// Underneath it will take a snapshot by fetching [`CanonicalInMemoryState::head_state`] and
    /// [`ProviderFactory::database_provider_ro`] effectively maintaining one single snapshotted
    /// view of memory and database.
    pub fn new(
        storage_provider_factory: ProviderFactory<N>,
        state: CanonicalInMemoryState<N::Primitives>,
    ) -> ProviderResult<Self> {
        // Each one provides a snapshot at the time of instantiation, but its order matters.
        //
        // If we acquire first the database provider, it's possible that before the in-memory chain
        // snapshot is instantiated, it will flush blocks to disk. This would
        // mean that our database provider would not have access to the flushed blocks (since it's
        // working under an older view), while the in-memory state may have deleted them
        // entirely. Resulting in gaps on the range.
        let head_block = state.head_state();
        let storage_provider = storage_provider_factory.database_provider_ro()?;
        Ok(Self { storage_provider, head_block, canonical_in_memory_state: state })
    }

    /// Consumes this consistent view and returns its database provider snapshot.
    pub(crate) fn into_database_provider(
        self,
    ) -> <ProviderFactory<N> as DatabaseProviderFactory>::Provider {
        self.storage_provider
    }

    /// Traverses the captured chain once; no height-only TD can terminate the walk.
    fn block_with_td_by_hash_budget(
        &self,
        hash: B256,
        remaining: &mut usize,
    ) -> ProviderResult<Option<(BlockTy<N>, U256)>> {
        let mut chain = self.head_block.as_deref().into_iter().flat_map(BlockState::chain);
        let mut lowest_memory = None;
        let mut target = None;
        for state in chain.by_ref() {
            let Some(next) = remaining.checked_sub(1) else { return Ok(None) };
            *remaining = next;
            lowest_memory = Some(state.number());
            if state.hash() == hash {
                target = Some(state);
                break
            }
        }

        let number = if let Some(state) = target {
            state.number()
        } else {
            let Some(next) = remaining.checked_sub(1) else { return Ok(None) };
            *remaining = next;
            let Some(number) = self.storage_provider.block_number(hash)? else { return Ok(None) };
            // A durable block covered by a different captured memory branch is not canonical in
            // this view. Do not turn an old hash-to-number mapping into a canonical result.
            if lowest_memory.is_some_and(|lowest| number >= lowest) {
                return Ok(None)
            }
            number
        };

        let mut expected = BlockNumHash::new(number, hash);
        let mut memory = target;
        let mut suffix = U256::ZERO;
        let chain_spec = self.chain_spec();
        let td = loop {
            // A configured genesis hash can be an alias for a different header. Only a
            // self-consistent declaration proves the header's difficulty without a stored row.
            if expected.number == chain_spec.genesis_header().number() &&
                expected.hash == chain_spec.genesis_hash() &&
                expected.hash == chain_spec.genesis_header().hash_slow()
            {
                let Some(td) = suffix.checked_add(chain_spec.genesis_header().difficulty()) else {
                    return Ok(None)
                };
                break td
            }

            let Some(next) = remaining.checked_sub(1) else { return Ok(None) };
            *remaining = next;
            let durable = self
                .storage_provider
                .static_file_provider()
                .header_with_td(expected.number)?
                .filter(|(header, _)| header.hash_slow() == expected.hash);
            if let Some((_, stored_td)) = &durable {
                let mut td = *stored_td;
                // Legacy zero placeholders are useful only with a canonical hash binding in
                // this same MDBX snapshot. HeaderNumbers alone does not prove that binding.
                if td.is_zero() &&
                    self.storage_provider
                        .tx_ref()
                        .get::<tables::CanonicalHeaders>(expected.number)? ==
                        Some(expected.hash)
                {
                    td = self
                        .storage_provider
                        .tx_ref()
                        .get::<tables::HeaderTerminalDifficulties>(expected.number)?
                        .map_or(U256::ZERO, |td| td.0);
                }
                if !td.is_zero() {
                    let Some(td) = suffix.checked_add(td) else { return Ok(None) };
                    break td
                }
            }

            if expected.number == chain_spec.genesis_header().number() {
                return Ok(None)
            }

            let header = if let Some(state) = memory {
                if state.hash() != expected.hash || state.number() != expected.number {
                    return Ok(None)
                }
                state.block_ref().recovered_block().header()
            } else if let Some((header, _)) = &durable {
                header
            } else {
                return Ok(None)
            };
            let Some(next_suffix) = suffix.checked_add(header.difficulty()) else {
                return Ok(None)
            };
            suffix = next_suffix;
            let Some(parent_number) = expected.number.checked_sub(1) else { return Ok(None) };
            expected = BlockNumHash::new(parent_number, header.parent_hash());
            memory = chain.next();
            if memory.is_some() {
                let Some(next) = remaining.checked_sub(1) else { return Ok(None) };
                *remaining = next;
            }
        };

        let block = if let Some(state) = target {
            state.block_ref().recovered_block().clone_block()
        } else {
            let Some(block) = self.storage_provider.block_by_hash(hash)? else { return Ok(None) };
            if block.header().hash_slow() != hash || block.header().number() != number {
                return Ok(None)
            }
            block
        };
        Ok(Some((block, td)))
    }

    // Helper function to convert range bounds
    fn convert_range_bounds<T>(
        &self,
        range: impl RangeBounds<T>,
        end_unbounded: impl FnOnce() -> T,
    ) -> (T, T)
    where
        T: Copy + Add<Output = T> + Sub<Output = T> + From<u8>,
    {
        let start = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + T::from(1u8),
            Bound::Unbounded => T::from(0u8),
        };

        let end = match range.end_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n - T::from(1u8),
            Bound::Unbounded => end_unbounded(),
        };

        (start, end)
    }

    /// Fetches a range of data from both in-memory state and persistent storage while a predicate
    /// is met.
    ///
    /// Creates a snapshot of the in-memory chain state and database provider to prevent
    /// inconsistencies. Splits the range into in-memory and storage sections, prioritizing
    /// recent in-memory blocks in case of overlaps.
    ///
    /// * `fetch_db_range` function (`F`) provides access to the database provider, allowing the
    ///   user to retrieve the required items from the database using [`RangeInclusive`].
    /// * `map_block_state_item` function (`G`) provides each block of the range in the in-memory
    ///   state, allowing for selection or filtering for the desired data.
    fn get_in_memory_or_storage_by_block_range_while<T, F, G, P>(
        &self,
        range: impl RangeBounds<BlockNumber>,
        fetch_db_range: F,
        map_block_state_item: G,
        mut predicate: P,
    ) -> ProviderResult<Vec<T>>
    where
        F: FnOnce(
            &DatabaseProviderRO<N::DB, N>,
            RangeInclusive<BlockNumber>,
            &mut P,
        ) -> ProviderResult<Vec<T>>,
        G: Fn(&BlockState<N::Primitives>, &mut P) -> Option<T>,
        P: FnMut(&T) -> bool,
    {
        // Each one provides a snapshot at the time of instantiation, but its order matters.
        //
        // If we acquire first the database provider, it's possible that before the in-memory chain
        // snapshot is instantiated, it will flush blocks to disk. This would
        // mean that our database provider would not have access to the flushed blocks (since it's
        // working under an older view), while the in-memory state may have deleted them
        // entirely. Resulting in gaps on the range.
        let mut in_memory_chain =
            self.head_block.as_ref().map(|b| b.chain().collect::<Vec<_>>()).unwrap_or_default();
        let db_provider = &self.storage_provider;

        let (start, end) = self.convert_range_bounds(range, || {
            // the first block is the highest one.
            in_memory_chain
                .first()
                .map(|b| b.number())
                .unwrap_or_else(|| db_provider.last_block_number().unwrap_or_default())
        });

        if start > end {
            return Ok(vec![])
        }

        // Split range into storage_range and in-memory range. If the in-memory range is not
        // necessary drop it early.
        //
        // The last block of `in_memory_chain` is the lowest block number.
        let (in_memory, storage_range) = match in_memory_chain.last().as_ref().map(|b| b.number()) {
            Some(lowest_memory_block) if lowest_memory_block <= end => {
                let highest_memory_block =
                    in_memory_chain.first().as_ref().map(|b| b.number()).expect("qed");

                // Database will for a time overlap with in-memory-chain blocks. In
                // case of a re-org, it can mean that the database blocks are of a forked chain, and
                // so, we should prioritize the in-memory overlapped blocks.
                let in_memory_range =
                    lowest_memory_block.max(start)..=end.min(highest_memory_block);

                // If requested range is in the middle of the in-memory range, remove the necessary
                // lowest blocks
                in_memory_chain.truncate(
                    in_memory_chain
                        .len()
                        .saturating_sub(start.saturating_sub(lowest_memory_block) as usize),
                );

                let storage_range =
                    (lowest_memory_block > start).then(|| start..=lowest_memory_block - 1);

                (Some((in_memory_chain, in_memory_range)), storage_range)
            }
            _ => {
                // Drop the in-memory chain so we don't hold blocks in memory.
                drop(in_memory_chain);

                (None, Some(start..=end))
            }
        };

        let mut items = Vec::with_capacity((end - start + 1) as usize);

        if let Some(storage_range) = storage_range {
            let mut db_items = fetch_db_range(db_provider, storage_range.clone(), &mut predicate)?;
            items.append(&mut db_items);

            // The predicate was not met, if the number of items differs from the expected. So, we
            // return what we have.
            if items.len() as u64 != storage_range.end() - storage_range.start() + 1 {
                return Ok(items)
            }
        }

        if let Some((in_memory_chain, in_memory_range)) = in_memory {
            for (num, block) in in_memory_range.zip(in_memory_chain.into_iter().rev()) {
                debug_assert!(num == block.number());
                if let Some(item) = map_block_state_item(block, &mut predicate) {
                    items.push(item);
                } else {
                    break
                }
            }
        }

        Ok(items)
    }

    /// Fetches data from either in-memory state or persistent storage for a range of transactions.
    ///
    /// * `fetch_from_db`: has a `DatabaseProviderRO` and the storage specific range.
    /// * `fetch_from_block_state`: has a [`RangeInclusive`] of elements that should be fetched from
    ///   [`BlockState`]. [`RangeInclusive`] is necessary to handle partial look-ups of a block.
    fn get_in_memory_or_storage_by_tx_range<S, M, R>(
        &self,
        range: impl RangeBounds<BlockNumber>,
        fetch_from_db: S,
        fetch_from_block_state: M,
    ) -> ProviderResult<Vec<R>>
    where
        S: FnOnce(
            &DatabaseProviderRO<N::DB, N>,
            RangeInclusive<TxNumber>,
        ) -> ProviderResult<Vec<R>>,
        M: Fn(RangeInclusive<usize>, &BlockState<N::Primitives>) -> ProviderResult<Vec<R>>,
    {
        let in_mem_chain = self.head_block.iter().flat_map(|b| b.chain()).collect::<Vec<_>>();
        let provider = &self.storage_provider;

        // Get the last block number stored in the storage which does NOT overlap with in-memory
        // chain.
        let last_database_block_number = in_mem_chain
            .last()
            .map(|b| Ok(b.anchor().number))
            .unwrap_or_else(|| provider.last_block_number())?;

        // Get the next tx number for the last block stored in the storage, which marks the start of
        // the in-memory state.
        let last_block_body_index = provider
            .block_body_indices(last_database_block_number)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(last_database_block_number))?;
        let mut in_memory_tx_num = last_block_body_index.next_tx_num();

        let (start, end) = self.convert_range_bounds(range, || {
            in_mem_chain
                .iter()
                .map(|b| b.block_ref().recovered_block().body().transactions().len() as u64)
                .sum::<u64>() +
                last_block_body_index.last_tx_num()
        });

        if start > end {
            return Ok(vec![])
        }

        let mut tx_range = start..=end;

        // If the range is entirely before the first in-memory transaction number, fetch from
        // storage
        if *tx_range.end() < in_memory_tx_num {
            return fetch_from_db(provider, tx_range);
        }

        let mut items = Vec::with_capacity((tx_range.end() - tx_range.start() + 1) as usize);

        // If the range spans storage and memory, get elements from storage first.
        if *tx_range.start() < in_memory_tx_num {
            // Determine the range that needs to be fetched from storage.
            let db_range = *tx_range.start()..=in_memory_tx_num.saturating_sub(1);

            // Set the remaining transaction range for in-memory
            tx_range = in_memory_tx_num..=*tx_range.end();

            items.extend(fetch_from_db(provider, db_range)?);
        }

        // Iterate from the lowest block to the highest in-memory chain
        for block_state in in_mem_chain.iter().rev() {
            let block_tx_count =
                block_state.block_ref().recovered_block().body().transactions().len();
            let remaining = (tx_range.end() - tx_range.start() + 1) as usize;

            // If the transaction range start is equal or higher than the next block first
            // transaction, advance
            if *tx_range.start() >= in_memory_tx_num + block_tx_count as u64 {
                in_memory_tx_num += block_tx_count as u64;
                continue
            }

            // This should only be more than 0 once, in case of a partial range inside a block.
            let skip = (tx_range.start() - in_memory_tx_num) as usize;

            items.extend(fetch_from_block_state(
                skip..=skip + (remaining.min(block_tx_count - skip) - 1),
                block_state,
            )?);

            in_memory_tx_num += block_tx_count as u64;

            // Break if the range has been fully processed
            if in_memory_tx_num > *tx_range.end() {
                break
            }

            // Set updated range
            tx_range = in_memory_tx_num..=*tx_range.end();
        }

        Ok(items)
    }

    /// Fetches data from either in-memory state or persistent storage by transaction
    /// [`HashOrNumber`].
    fn get_in_memory_or_storage_by_tx<S, M, R>(
        &self,
        id: HashOrNumber,
        fetch_from_db: S,
        fetch_from_block_state: M,
    ) -> ProviderResult<Option<R>>
    where
        S: FnOnce(&DatabaseProviderRO<N::DB, N>) -> ProviderResult<Option<R>>,
        M: Fn(usize, TxNumber, &BlockState<N::Primitives>) -> ProviderResult<Option<R>>,
    {
        let in_mem_chain = self.head_block.iter().flat_map(|b| b.chain()).collect::<Vec<_>>();
        let provider = &self.storage_provider;

        // Get the last block number stored in the database which does NOT overlap with in-memory
        // chain.
        let last_database_block_number = in_mem_chain
            .last()
            .map(|b| Ok(b.anchor().number))
            .unwrap_or_else(|| provider.last_block_number())?;

        // Get the next tx number for the last block stored in the database and consider it the
        // first tx number of the in-memory state
        let last_block_body_index = provider
            .block_body_indices(last_database_block_number)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(last_database_block_number))?;
        let mut in_memory_tx_num = last_block_body_index.next_tx_num();

        // If the transaction number is less than the first in-memory transaction number, make a
        // database lookup
        if let HashOrNumber::Number(id) = id &&
            id < in_memory_tx_num
        {
            return fetch_from_db(provider)
        }

        // Iterate from the lowest block to the highest
        for block_state in in_mem_chain.iter().rev() {
            let executed_block = block_state.block_ref();
            let block = executed_block.recovered_block();

            for tx_index in 0..block.body().transactions().len() {
                match id {
                    HashOrNumber::Hash(tx_hash) => {
                        if tx_hash == *block.body().transactions()[tx_index].tx_hash() {
                            return fetch_from_block_state(tx_index, in_memory_tx_num, block_state)
                        }
                    }
                    HashOrNumber::Number(id) => {
                        if id == in_memory_tx_num {
                            return fetch_from_block_state(tx_index, in_memory_tx_num, block_state)
                        }
                    }
                }

                in_memory_tx_num += 1;
            }
        }

        // Not found in-memory, so check database.
        if let HashOrNumber::Hash(_) = id {
            return fetch_from_db(provider)
        }

        Ok(None)
    }

    /// Fetches data from either in-memory state or persistent storage by [`BlockHashOrNumber`].
    pub(crate) fn get_in_memory_or_storage_by_block<S, M, R>(
        &self,
        id: BlockHashOrNumber,
        fetch_from_db: S,
        fetch_from_block_state: M,
    ) -> ProviderResult<R>
    where
        S: FnOnce(&DatabaseProviderRO<N::DB, N>) -> ProviderResult<R>,
        M: Fn(&BlockState<N::Primitives>) -> ProviderResult<R>,
    {
        if let Some(Some(block_state)) = self.head_block.as_ref().map(|b| b.block_on_chain(id)) {
            return fetch_from_block_state(block_state)
        }
        fetch_from_db(&self.storage_provider)
    }
}

impl<N: ProviderNodeTypes> NodePrimitivesProvider for ConsistentProvider<N> {
    type Primitives = N::Primitives;
}

impl<N: ProviderNodeTypes> StaticFileProviderFactory for ConsistentProvider<N> {
    fn static_file_provider(&self) -> StaticFileProvider<N::Primitives> {
        self.storage_provider.static_file_provider()
    }

    fn get_static_file_writer(
        &self,
        block: BlockNumber,
        segment: StaticFileSegment,
    ) -> ProviderResult<StaticFileProviderRWRefMut<'_, Self::Primitives>> {
        self.storage_provider.get_static_file_writer(block, segment)
    }
}

impl<N: ProviderNodeTypes> HeaderProvider for ConsistentProvider<N> {
    type Header = HeaderTy<N>;

    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        self.get_in_memory_or_storage_by_block(
            block_hash.into(),
            |db_provider| db_provider.header(block_hash),
            |block_state| Ok(Some(block_state.block_ref().recovered_block().clone_header())),
        )
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Self::Header>> {
        self.get_in_memory_or_storage_by_block(
            num.into(),
            |db_provider| db_provider.header_by_number(num),
            |block_state| Ok(Some(block_state.block_ref().recovered_block().clone_header())),
        )
    }

    fn header_td(&self, hash: &BlockHash) -> ProviderResult<Option<alloy_primitives::U256>> {
        if let Some(num) = self.block_number(*hash)? {
            self.header_td_by_number(num)
        } else {
            // Hash not (yet) known to this provider — TD is unknown; the caller falls back.
            Ok(None)
        }
    }

    fn header_td_by_number(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<alloy_primitives::U256>> {
        // BSC parlia TD. Persisted TD comes from static files or legacy MDBX entries; blocks
        // above the on-disk tip are only in memory. Walk down from `number`, summing difficulties,
        // until we reach a block whose TD is already known (persisted, or the genesis
        // fallback), then add the running sum. This short-circuits at the on-disk tip in
        // the common case (so it walks only the small in-memory tail and stays O(1) on a
        // cold restart at a deep head), while tolerating missing historical TD entries.
        let mut suffix = alloy_primitives::U256::ZERO;
        let mut cursor = number;
        loop {
            if let Some(base) = self.storage_provider.header_td_by_number(cursor)? {
                return Ok(Some(base + suffix));
            }
            // `cursor`'s TD isn't persisted: fold in its difficulty and descend to the parent.
            let Some(header) = self.header_by_number(cursor)? else { return Ok(None) };
            suffix += header.difficulty();
            if cursor == 0 {
                // Genesis with no persisted TD — its difficulty is already folded into `suffix`.
                return Ok(Some(suffix));
            }
            cursor -= 1;
        }
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, _| db_provider.headers_range(range),
            |block_state, _| Some(block_state.block_ref().recovered_block().header().clone()),
            |_| true,
        )
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.get_in_memory_or_storage_by_block(
            number.into(),
            |db_provider| db_provider.sealed_header(number),
            |block_state| Ok(Some(block_state.block_ref().recovered_block().clone_sealed_header())),
        )
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, _| db_provider.sealed_headers_range(range),
            |block_state, _| Some(block_state.block_ref().recovered_block().clone_sealed_header()),
            |_| true,
        )
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, predicate| db_provider.sealed_headers_while(range, predicate),
            |block_state, predicate| {
                let header = block_state.block_ref().recovered_block().sealed_header();
                predicate(header).then(|| header.clone())
            },
            predicate,
        )
    }
}

impl<N: ProviderNodeTypes> BlockHashReader for ConsistentProvider<N> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.get_in_memory_or_storage_by_block(
            number.into(),
            |db_provider| db_provider.block_hash(number),
            |block_state| Ok(Some(block_state.hash())),
        )
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.get_in_memory_or_storage_by_block_range_while(
            start..end,
            |db_provider, inclusive_range, _| {
                db_provider
                    .canonical_hashes_range(*inclusive_range.start(), *inclusive_range.end() + 1)
            },
            |block_state, _| Some(block_state.hash()),
            |_| true,
        )
    }
}

impl<N: ProviderNodeTypes> BlockNumReader for ConsistentProvider<N> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        let best_number = self.best_block_number()?;
        Ok(ChainInfo { best_hash: self.block_hash(best_number)?.unwrap_or_default(), best_number })
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.head_block.as_ref().map(|b| Ok(b.number())).unwrap_or_else(|| self.last_block_number())
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.storage_provider.last_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        self.get_in_memory_or_storage_by_block(
            hash.into(),
            |db_provider| db_provider.block_number(hash),
            |block_state| Ok(Some(block_state.number())),
        )
    }
}

impl<N: ProviderNodeTypes> BlockIdReader for ConsistentProvider<N> {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(self.canonical_in_memory_state.pending_block_num_hash())
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(self.canonical_in_memory_state.get_safe_num_hash())
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(self.canonical_in_memory_state.get_finalized_num_hash())
    }
}

impl<N: ProviderNodeTypes> BlockReader for ConsistentProvider<N> {
    type Block = BlockTy<N>;

    fn find_block_by_hash(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        if source.is_canonical() &&
            let Some(block) = self.get_in_memory_or_storage_by_block(
                hash.into(),
                |db_provider| db_provider.find_block_by_hash(hash, BlockSource::Canonical),
                |block_state| Ok(Some(block_state.block_ref().recovered_block().clone_block())),
            )?
        {
            return Ok(Some(block))
        }

        if source.is_pending() {
            return Ok(self
                .canonical_in_memory_state
                .pending_block()
                .filter(|b| b.hash() == hash)
                .map(|b| b.into_block()))
        }

        Ok(None)
    }

    fn find_sealed_or_recovered_block(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<SealedOrRecoveredBlock<Self::Block>>> {
        if source.is_canonical() &&
            let Some(block) = self.get_in_memory_or_storage_by_block(
                hash.into(),
                |db_provider| {
                    db_provider.find_sealed_or_recovered_block(hash, BlockSource::Canonical)
                },
                |block_state| {
                    Ok(Some(SealedOrRecoveredBlock::recovered_arc(Arc::clone(
                        &block_state.block_ref().recovered_block,
                    ))))
                },
            )?
        {
            return Ok(Some(block))
        }

        if source.is_pending() &&
            let Some(block_state) = self.canonical_in_memory_state.pending_state()
        {
            let recovered_block = Arc::clone(&block_state.block_ref().recovered_block);
            if recovered_block.hash() == hash {
                return Ok(Some(SealedOrRecoveredBlock::recovered_arc(recovered_block)))
            }
        }

        Ok(None)
    }

    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        self.get_in_memory_or_storage_by_block(
            id,
            |db_provider| db_provider.block(id),
            |block_state| Ok(Some(block_state.block_ref().recovered_block().clone_block())),
        )
    }

    fn block_with_td_by_hash(&self, hash: B256) -> ProviderResult<Option<(Self::Block, U256)>> {
        let mut remaining = MAX_EXACT_TD_STEPS;
        self.block_with_td_by_hash_budget(hash, &mut remaining)
    }

    fn pending_block(&self) -> ProviderResult<Option<Arc<RecoveredBlock<Self::Block>>>> {
        Ok(self.canonical_in_memory_state.pending_recovered_block())
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<RecoveredBlockAndExecutionOutput<Self::Block, Self::Receipt>>> {
        Ok(self.canonical_in_memory_state.pending_block_and_receipts())
    }

    /// Returns the block with senders with matching number or hash from database.
    ///
    /// **NOTE: If [`TransactionVariant::NoHash`] is provided then the transactions have invalid
    /// hashes, since they would need to be calculated on the spot, and we want fast querying.**
    ///
    /// Returns `None` if block is not found.
    fn recovered_block(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.get_in_memory_or_storage_by_block(
            id,
            |db_provider| db_provider.recovered_block(id, transaction_kind),
            |block_state| Ok(Some(block_state.block().recovered_block().clone())),
        )
    }

    fn sealed_block_with_senders(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.get_in_memory_or_storage_by_block(
            id,
            |db_provider| db_provider.sealed_block_with_senders(id, transaction_kind),
            |block_state| Ok(Some(block_state.block().recovered_block().clone())),
        )
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, _| db_provider.block_range(range),
            |block_state, _| Some(block_state.block_ref().recovered_block().clone_block()),
            |_| true,
        )
    }

    fn block_with_senders_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, _| db_provider.block_with_senders_range(range),
            |block_state, _| Some(block_state.block().recovered_block().clone()),
            |_| true,
        )
    }

    fn recovered_block_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, _| db_provider.recovered_block_range(range),
            |block_state, _| Some(block_state.block().recovered_block().clone()),
            |_| true,
        )
    }

    fn block_by_transaction_id(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        self.get_in_memory_or_storage_by_tx(
            id.into(),
            |db_provider| db_provider.block_by_transaction_id(id),
            |_, _, block_state| Ok(Some(block_state.number())),
        )
    }
}

impl<N: ProviderNodeTypes> TransactionsProvider for ConsistentProvider<N> {
    type Transaction = TxTy<N>;

    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        self.get_in_memory_or_storage_by_tx(
            tx_hash.into(),
            |db_provider| db_provider.transaction_id(tx_hash),
            |_, tx_number, _| Ok(Some(tx_number)),
        )
    }

    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        self.get_in_memory_or_storage_by_tx(
            id.into(),
            |provider| provider.transaction_by_id(id),
            |tx_index, _, block_state| {
                Ok(block_state
                    .block_ref()
                    .recovered_block()
                    .body()
                    .transactions()
                    .get(tx_index)
                    .cloned())
            },
        )
    }

    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.get_in_memory_or_storage_by_tx(
            id.into(),
            |provider| provider.transaction_by_id_unhashed(id),
            |tx_index, _, block_state| {
                Ok(block_state
                    .block_ref()
                    .recovered_block()
                    .body()
                    .transactions()
                    .get(tx_index)
                    .cloned())
            },
        )
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        if let Some(tx) = self.head_block.as_ref().and_then(|b| b.transaction_on_chain(hash)) {
            return Ok(Some(tx))
        }

        self.storage_provider.transaction_by_hash(hash)
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        if let Some((tx, meta)) =
            self.head_block.as_ref().and_then(|b| b.transaction_meta_on_chain(tx_hash))
        {
            return Ok(Some((tx, meta)))
        }

        self.storage_provider.transaction_by_hash_with_meta(tx_hash)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        self.get_in_memory_or_storage_by_block(
            id,
            |provider| provider.transactions_by_block(id),
            |block_state| {
                Ok(Some(block_state.block_ref().recovered_block().body().transactions().to_vec()))
            },
        )
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        self.get_in_memory_or_storage_by_block_range_while(
            range,
            |db_provider, range, _| db_provider.transactions_by_block_range(range),
            |block_state, _| {
                Some(block_state.block_ref().recovered_block().body().transactions().to_vec())
            },
            |_| true,
        )
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        self.get_in_memory_or_storage_by_tx_range(
            range,
            |db_provider, db_range| db_provider.transactions_by_tx_range(db_range),
            |index_range, block_state| {
                Ok(block_state.block_ref().recovered_block().body().transactions()[index_range]
                    .to_vec())
            },
        )
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        self.get_in_memory_or_storage_by_tx_range(
            range,
            |db_provider, db_range| db_provider.senders_by_tx_range(db_range),
            |index_range, block_state| {
                Ok(block_state.block_ref().recovered_block.senders()[index_range].to_vec())
            },
        )
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        self.get_in_memory_or_storage_by_tx(
            id.into(),
            |provider| provider.transaction_sender(id),
            |tx_index, _, block_state| {
                Ok(block_state.block_ref().recovered_block.senders().get(tx_index).copied())
            },
        )
    }
}

impl<N: ProviderNodeTypes> ReceiptProvider for ConsistentProvider<N> {
    type Receipt = ReceiptTy<N>;

    fn receipt(&self, id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        self.get_in_memory_or_storage_by_tx(
            id.into(),
            |provider| provider.receipt(id),
            |tx_index, _, block_state| {
                Ok(block_state.executed_block_receipts_ref().get(tx_index).cloned())
            },
        )
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        for block_state in self.head_block.iter().flat_map(|b| b.chain()) {
            let executed_block = block_state.block_ref();
            let block = executed_block.recovered_block();
            let receipts = block_state.executed_block_receipts_ref();

            // assuming 1:1 correspondence between transactions and receipts
            debug_assert_eq!(
                block.body().transactions().len(),
                receipts.len(),
                "Mismatch between transaction and receipt count"
            );

            if let Some(tx_index) =
                block.body().transactions_iter().position(|tx| *tx.tx_hash() == hash)
            {
                // safe to use tx_index for receipts due to 1:1 correspondence
                return Ok(receipts.get(tx_index).cloned());
            }
        }

        self.storage_provider.receipt_by_hash(hash)
    }

    fn receipts_by_block(
        &self,
        block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        self.get_in_memory_or_storage_by_block(
            block,
            |db_provider| db_provider.receipts_by_block(block),
            |block_state| Ok(Some(block_state.executed_block_receipts())),
        )
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        self.get_in_memory_or_storage_by_tx_range(
            range,
            |db_provider, db_range| db_provider.receipts_by_tx_range(db_range),
            |index_range, block_state| {
                Ok(block_state.executed_block_receipts_ref()[index_range].to_vec())
            },
        )
    }

    fn receipts_by_block_range(
        &self,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        self.storage_provider.receipts_by_block_range(block_range)
    }
}

impl<N: ProviderNodeTypes> ReceiptProviderIdExt for ConsistentProvider<N> {
    fn receipts_by_block_id(&self, block: BlockId) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        match block {
            BlockId::Hash(rpc_block_hash) => {
                let mut receipts = self.receipts_by_block(rpc_block_hash.block_hash.into())?;
                if receipts.is_none() &&
                    !rpc_block_hash.require_canonical.unwrap_or(false) &&
                    let Some(state) = self
                        .head_block
                        .as_ref()
                        .and_then(|b| b.block_on_chain(rpc_block_hash.block_hash.into()))
                {
                    receipts = Some(state.executed_block_receipts());
                }
                Ok(receipts)
            }
            BlockId::Number(num_tag) => match num_tag {
                BlockNumberOrTag::Pending => Ok(self
                    .canonical_in_memory_state
                    .pending_state()
                    .map(|block_state| block_state.executed_block_receipts())),
                _ => {
                    if let Some(num) = self.convert_block_number(num_tag)? {
                        self.receipts_by_block(num.into())
                    } else {
                        Ok(None)
                    }
                }
            },
        }
    }
}

impl<N: ProviderNodeTypes> BlockBodyIndicesProvider for ConsistentProvider<N> {
    fn block_body_indices(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        self.get_in_memory_or_storage_by_block(
            number.into(),
            |db_provider| db_provider.block_body_indices(number),
            |block_state| {
                // Find the last block indices on database
                let last_storage_block_number = block_state.anchor().number;
                let mut stored_indices = self
                    .storage_provider
                    .block_body_indices(last_storage_block_number)?
                    .ok_or(ProviderError::BlockBodyIndicesNotFound(last_storage_block_number))?;

                // Prepare our block indices
                stored_indices.first_tx_num = stored_indices.next_tx_num();
                stored_indices.tx_count = 0;

                // Iterate from the lowest block in memory until our target block
                for state in block_state.chain().collect::<Vec<_>>().into_iter().rev() {
                    let block_tx_count =
                        state.block_ref().recovered_block().body().transactions().len() as u64;
                    if state.block_ref().recovered_block().number() == number {
                        stored_indices.tx_count = block_tx_count;
                    } else {
                        stored_indices.first_tx_num += block_tx_count;
                    }
                }

                Ok(Some(stored_indices))
            },
        )
    }

    fn block_body_indices_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        range.map_while(|b| self.block_body_indices(b).transpose()).collect()
    }
}

impl<N: ProviderNodeTypes> StageCheckpointReader for ConsistentProvider<N> {
    fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        self.storage_provider.get_stage_checkpoint(id)
    }

    fn get_stage_checkpoint_progress(&self, id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        self.storage_provider.get_stage_checkpoint_progress(id)
    }

    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        self.storage_provider.get_all_checkpoints()
    }
}

impl<N: ProviderNodeTypes> PruneCheckpointReader for ConsistentProvider<N> {
    fn get_prune_checkpoint(
        &self,
        segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>> {
        self.storage_provider.get_prune_checkpoint(segment)
    }

    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
        self.storage_provider.get_prune_checkpoints()
    }
}

impl<N: ProviderNodeTypes> ChainSpecProvider for ConsistentProvider<N> {
    type ChainSpec = N::ChainSpec;

    fn chain_spec(&self) -> Arc<N::ChainSpec> {
        ChainSpecProvider::chain_spec(&self.storage_provider)
    }
}

impl<N: ProviderNodeTypes> BlockReaderIdExt for ConsistentProvider<N> {
    fn block_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Block>> {
        match id {
            BlockId::Number(num) => self.block_by_number_or_tag(num),
            BlockId::Hash(hash) => {
                // TODO: should we only apply this for the RPCs that are listed in EIP-1898?
                // so not at the provider level?
                // if we decide to do this at a higher level, then we can make this an automatic
                // trait impl
                if Some(true) == hash.require_canonical {
                    // check the database, canonical blocks are only stored in the database
                    self.find_block_by_hash(hash.block_hash, BlockSource::Canonical)
                } else {
                    self.block_by_hash(hash.block_hash)
                }
            }
        }
    }

    fn header_by_number_or_tag(&self, id: BlockNumberOrTag) -> ProviderResult<Option<HeaderTy<N>>> {
        Ok(match id {
            BlockNumberOrTag::Latest => {
                Some(self.canonical_in_memory_state.get_canonical_head().unseal())
            }
            BlockNumberOrTag::Finalized => {
                self.canonical_in_memory_state.get_finalized_header().map(|h| h.unseal())
            }
            BlockNumberOrTag::Safe => {
                self.canonical_in_memory_state.get_safe_header().map(|h| h.unseal())
            }
            BlockNumberOrTag::Earliest => self.header_by_number(self.earliest_block_number()?)?,
            BlockNumberOrTag::Pending => self.canonical_in_memory_state.pending_header(),

            BlockNumberOrTag::Number(num) => self.header_by_number(num)?,
        })
    }

    fn sealed_header_by_number_or_tag(
        &self,
        id: BlockNumberOrTag,
    ) -> ProviderResult<Option<SealedHeader<HeaderTy<N>>>> {
        match id {
            BlockNumberOrTag::Latest => {
                Ok(Some(self.canonical_in_memory_state.get_canonical_head()))
            }
            BlockNumberOrTag::Finalized => {
                Ok(self.canonical_in_memory_state.get_finalized_header())
            }
            BlockNumberOrTag::Safe => Ok(self.canonical_in_memory_state.get_safe_header()),
            BlockNumberOrTag::Earliest => self
                .header_by_number(self.earliest_block_number()?)?
                .map_or_else(|| Ok(None), |h| Ok(Some(SealedHeader::seal_slow(h)))),
            BlockNumberOrTag::Pending => Ok(self.canonical_in_memory_state.pending_sealed_header()),
            BlockNumberOrTag::Number(num) => self
                .header_by_number(num)?
                .map_or_else(|| Ok(None), |h| Ok(Some(SealedHeader::seal_slow(h)))),
        }
    }

    fn sealed_header_by_id(
        &self,
        id: BlockId,
    ) -> ProviderResult<Option<SealedHeader<HeaderTy<N>>>> {
        Ok(match id {
            BlockId::Number(num) => self.sealed_header_by_number_or_tag(num)?,
            BlockId::Hash(hash) => self
                .header(hash.block_hash)?
                .map(|header| SealedHeader::new(header, hash.block_hash)),
        })
    }

    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<HeaderTy<N>>> {
        Ok(match id {
            BlockId::Number(num) => self.header_by_number_or_tag(num)?,
            BlockId::Hash(hash) => self.header(hash.block_hash)?,
        })
    }
}

impl<N: ProviderNodeTypes> StorageChangeSetReader for ConsistentProvider<N> {
    fn storage_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        if let Some(state) =
            self.head_block.as_ref().and_then(|b| b.block_on_chain(block_number.into()))
        {
            let changesets = state
                .block()
                .execution_output
                .state
                .reverts
                .to_plain_state_reverts()
                .storage
                .into_iter()
                .flatten()
                .flat_map(|revert: PlainStorageRevert| {
                    revert.storage_revert.into_iter().map(move |(key, value)| {
                        (
                            BlockNumberAddress((block_number, revert.address)),
                            StorageEntry { key: key.into(), value: value.to_previous_value() },
                        )
                    })
                })
                .collect();
            Ok(changesets)
        } else {
            // Perform checks on whether or not changesets exist for the block.

            // No prune checkpoint means history should exist and we should `unwrap_or(true)`
            let storage_history_exists = self
                .storage_provider
                .get_prune_checkpoint(PruneSegment::StorageHistory)?
                .and_then(|checkpoint| {
                    // return true if the block number is ahead of the prune checkpoint.
                    //
                    // The checkpoint stores the highest pruned block number, so we should make
                    // sure the block_number is strictly greater.
                    checkpoint.block_number.map(|checkpoint| block_number > checkpoint)
                })
                .unwrap_or(true);

            if !storage_history_exists {
                return Err(ProviderError::StateAtBlockPruned(block_number))
            }

            self.storage_provider.storage_changeset(block_number)
        }
    }

    fn get_storage_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
        storage_key: B256,
    ) -> ProviderResult<Option<StorageEntry>> {
        if let Some(state) =
            self.head_block.as_ref().and_then(|b| b.block_on_chain(block_number.into()))
        {
            let changeset = state
                .block_ref()
                .execution_output
                .state
                .reverts
                .to_plain_state_reverts()
                .storage
                .into_iter()
                .flatten()
                .find_map(|revert: PlainStorageRevert| {
                    if revert.address != address {
                        return None
                    }
                    revert.storage_revert.into_iter().find_map(|(key, value)| {
                        let plain_key = B256::from(key);
                        (plain_key == storage_key).then(|| StorageEntry {
                            key: plain_key,
                            value: value.to_previous_value(),
                        })
                    })
                });
            Ok(changeset)
        } else {
            let storage_history_exists = self
                .storage_provider
                .get_prune_checkpoint(PruneSegment::StorageHistory)?
                .and_then(|checkpoint| {
                    checkpoint.block_number.map(|checkpoint| block_number > checkpoint)
                })
                .unwrap_or(true);

            if !storage_history_exists {
                return Err(ProviderError::StateAtBlockPruned(block_number))
            }

            self.storage_provider.get_storage_before_block(block_number, address, storage_key)
        }
    }

    fn storage_changesets_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        let range = to_range(range);
        let mut changesets = Vec::new();
        let database_start = range.start;
        let mut database_end = range.end;

        if let Some(head_block) = &self.head_block {
            database_end = head_block.anchor().number;

            for state in head_block.chain() {
                let block_changesets = state
                    .block_ref()
                    .execution_output
                    .state
                    .reverts
                    .to_plain_state_reverts()
                    .storage
                    .into_iter()
                    .flatten()
                    .flat_map(|revert: PlainStorageRevert| {
                        revert.storage_revert.into_iter().map(move |(key, value)| {
                            (
                                BlockNumberAddress((state.number(), revert.address)),
                                StorageEntry { key: key.into(), value: value.to_previous_value() },
                            )
                        })
                    });

                changesets.extend(block_changesets);
            }
        }

        if database_start < database_end {
            let storage_history_exists = self
                .storage_provider
                .get_prune_checkpoint(PruneSegment::StorageHistory)?
                .and_then(|checkpoint| {
                    checkpoint.block_number.map(|checkpoint| database_start > checkpoint)
                })
                .unwrap_or(true);

            if !storage_history_exists {
                return Err(ProviderError::StateAtBlockPruned(database_start))
            }

            let db_changesets = self
                .storage_provider
                .storage_changesets_range(database_start..=database_end - 1)?;
            changesets.extend(db_changesets);
        }

        changesets.sort_by_key(|(block_address, _)| block_address.block_number());

        Ok(changesets)
    }
}

impl<N: ProviderNodeTypes> ChangeSetReader for ConsistentProvider<N> {
    fn account_block_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<AccountBeforeTx>> {
        if let Some(state) =
            self.head_block.as_ref().and_then(|b| b.block_on_chain(block_number.into()))
        {
            let changesets = state
                .block_ref()
                .execution_output
                .state
                .reverts
                .to_plain_state_reverts()
                .accounts
                .into_iter()
                .flatten()
                .map(|(address, info)| AccountBeforeTx { address, info: info.map(Into::into) })
                .collect();
            Ok(changesets)
        } else {
            // Perform checks on whether or not changesets exist for the block.

            // No prune checkpoint means history should exist and we should `unwrap_or(true)`
            let account_history_exists = self
                .storage_provider
                .get_prune_checkpoint(PruneSegment::AccountHistory)?
                .and_then(|checkpoint| {
                    // return true if the block number is ahead of the prune checkpoint.
                    //
                    // The checkpoint stores the highest pruned block number, so we should make
                    // sure the block_number is strictly greater.
                    checkpoint.block_number.map(|checkpoint| block_number > checkpoint)
                })
                .unwrap_or(true);

            if !account_history_exists {
                return Err(ProviderError::StateAtBlockPruned(block_number))
            }

            self.storage_provider.account_block_changeset(block_number)
        }
    }

    fn get_account_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
    ) -> ProviderResult<Option<AccountBeforeTx>> {
        if let Some(state) =
            self.head_block.as_ref().and_then(|b| b.block_on_chain(block_number.into()))
        {
            // Search in-memory state for the account changeset
            let changeset = state
                .block_ref()
                .execution_output
                .state
                .reverts
                .to_plain_state_reverts()
                .accounts
                .into_iter()
                .flatten()
                .find(|(addr, _)| addr == &address)
                .map(|(address, info)| AccountBeforeTx { address, info: info.map(Into::into) });
            Ok(changeset)
        } else {
            // Perform checks on whether or not changesets exist for the block.
            // No prune checkpoint means history should exist and we should `unwrap_or(true)`
            let account_history_exists = self
                .storage_provider
                .get_prune_checkpoint(PruneSegment::AccountHistory)?
                .and_then(|checkpoint| {
                    // return true if the block number is ahead of the prune checkpoint.
                    //
                    // The checkpoint stores the highest pruned block number, so we should make
                    // sure the block_number is strictly greater.
                    checkpoint.block_number.map(|checkpoint| block_number > checkpoint)
                })
                .unwrap_or(true);

            if !account_history_exists {
                return Err(ProviderError::StateAtBlockPruned(block_number))
            }

            // Delegate to the storage provider for database lookups
            self.storage_provider.get_account_before_block(block_number, address)
        }
    }

    fn account_changesets_range(
        &self,
        range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumber, AccountBeforeTx)>> {
        let range = to_range(range);
        let mut changesets = Vec::new();
        let database_start = range.start;
        let mut database_end = range.end;

        // Check which blocks in the range are in memory
        if let Some(head_block) = &self.head_block {
            // the anchor is the end of the db range
            database_end = head_block.anchor().number;

            for state in head_block.chain() {
                // found block in memory, collect its changesets
                let block_changesets = state
                    .block_ref()
                    .execution_output
                    .state
                    .reverts
                    .to_plain_state_reverts()
                    .accounts
                    .into_iter()
                    .flatten()
                    .map(|(address, info)| AccountBeforeTx { address, info: info.map(Into::into) });

                for changeset in block_changesets {
                    changesets.push((state.number(), changeset));
                }
            }
        }

        // get changesets from database for remaining blocks
        if database_start < database_end {
            // check if account history is pruned for these blocks
            let account_history_exists = self
                .storage_provider
                .get_prune_checkpoint(PruneSegment::AccountHistory)?
                .and_then(|checkpoint| {
                    checkpoint.block_number.map(|checkpoint| database_start > checkpoint)
                })
                .unwrap_or(true);

            if !account_history_exists {
                return Err(ProviderError::StateAtBlockPruned(database_start))
            }

            let db_changesets =
                self.storage_provider.account_changesets_range(database_start..database_end)?;
            changesets.extend(db_changesets);
        }

        changesets.sort_by_key(|(block_num, _)| *block_num);

        Ok(changesets)
    }
}

#[cfg(test)]
#[allow(clippy::clone_on_copy)]
mod tests {
    use super::*;
    use crate::{
        providers::{blockchain_provider::BlockchainProvider, StaticFileWriter},
        test_utils::{
            create_test_provider_factory, create_test_provider_factory_with_chain_spec,
            MockNodeTypesWithDB,
        },
        BlockWriter,
    };
    use alloy_consensus::Header;
    use alloy_eips::BlockHashOrNumber;
    use alloy_primitives::B256;
    use itertools::Itertools;
    use rand::Rng;
    use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
    use reth_chainspec::MAINNET;
    use reth_db_api::{models::AccountBeforeTx, transaction::DbTxMut};
    use reth_ethereum_primitives::Block;
    use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult, ExecutionOutcome};
    use reth_primitives_traits::{RecoveredBlock, SealedBlock};
    use reth_storage_api::{BlockReader, BlockSource, ChangeSetReader, StateReader};
    use reth_testing_utils::generators::{
        self, random_block_range, random_changeset_range, random_eoa_accounts, BlockRangeParams,
    };
    use revm::database::BundleState;
    use std::{
        ops::{Bound, Range, RangeBounds},
        sync::Arc,
    };

    const TEST_BLOCKS_COUNT: usize = 5;

    fn td_child(parent: &SealedBlock<Block>, difficulty: u64) -> SealedBlock<Block> {
        Block {
            header: Header {
                number: parent.number() + 1,
                parent_hash: parent.hash(),
                difficulty: U256::from(difficulty),
                ..Default::default()
            },
            ..Default::default()
        }
        .seal_slow()
    }

    fn td_executed(block: &SealedBlock<Block>) -> ExecutedBlock {
        ExecutedBlock {
            recovered_block: Arc::new(block.clone().try_recover().unwrap()),
            ..Default::default()
        }
    }

    struct TdFixture {
        factory: ProviderFactory<MockNodeTypesWithDB>,
        provider: BlockchainProvider<MockNodeTypesWithDB>,
        common: SealedBlock<Block>,
        old: Vec<SealedBlock<Block>>,
        new: Vec<SealedBlock<Block>>,
    }

    fn td_fixture() -> eyre::Result<TdFixture> {
        let factory = create_test_provider_factory();
        let genesis =
            Block { header: factory.chain_spec().genesis_header().clone(), ..Default::default() }
                .seal_slow();
        let common = td_child(&genesis, 2);
        let old_one = td_child(&common, 3);
        let old_two = td_child(&old_one, 5);
        let new_one = td_child(&common, 7);
        let new_two = td_child(&new_one, 11);
        let writer = factory.provider_rw()?;
        for block in [&genesis, &common, &old_one, &old_two] {
            writer.insert_block(&block.clone().try_recover().unwrap())?;
        }
        writer.commit()?;
        let provider = BlockchainProvider::new(factory.clone())?;
        Ok(TdFixture {
            factory,
            provider,
            common,
            old: vec![old_one, old_two],
            new: vec![new_one, new_two],
        })
    }

    #[test]
    fn exact_td_follows_memory_fork_to_common_durable_anchor() -> eyre::Result<()> {
        let fixture = td_fixture()?;
        fixture.provider.canonical_in_memory_state().update_chain(NewCanonicalChain::Commit {
            new: fixture.new.iter().map(td_executed).collect(),
        });
        let view = fixture.provider.consistent_provider()?;
        let anchor_td = fixture.factory.provider()?.header_td_by_number(1)?.unwrap();
        let expected = anchor_td + U256::from(18);
        assert_eq!(view.block_hash(3)?, Some(fixture.new[1].hash()));
        assert_eq!(view.header_td_by_number(3)?, Some(anchor_td + U256::from(8)));
        let (block, td) = fixture
            .provider
            .block_with_td_by_hash(fixture.new[1].hash())?
            .expect("new branch must remain propagatable before durable unwind");
        assert_eq!(block, fixture.new[1].clone().into_block());
        assert_eq!(td, expected);
        assert!(view.block_with_td_by_hash(fixture.old[1].hash())?.is_none());
        // Wrapper implementations must not silently fall back to the default unavailable method.
        let wrapped = Arc::new(fixture.provider);
        assert_eq!(wrapped.block_with_td_by_hash(fixture.new[1].hash())?.unwrap().1, expected);
        assert_eq!(
            BlockReader::block_with_td_by_hash(&wrapped.as_ref(), fixture.new[1].hash())?
                .unwrap()
                .1,
            expected
        );
        Ok(())
    }

    #[test]
    fn exact_td_captured_view_survives_live_a_b_a_reorg() -> eyre::Result<()> {
        let fixture = td_fixture()?;
        let state = fixture.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit {
            new: fixture.new.iter().map(td_executed).collect(),
        });
        let view = fixture.provider.consistent_provider()?;
        let expected =
            fixture.factory.provider()?.header_td_by_number(1)?.unwrap() + U256::from(18);
        state.update_chain(NewCanonicalChain::Reorg {
            old: fixture.new.iter().map(td_executed).collect(),
            new: fixture.old.iter().map(td_executed).collect(),
        });
        assert_eq!(fixture.provider.block_hash(3)?, Some(fixture.old[1].hash()));
        assert_eq!(view.block_with_td_by_hash(fixture.new[1].hash())?.unwrap().1, expected);
        state.update_chain(NewCanonicalChain::Reorg {
            old: fixture.old.iter().map(td_executed).collect(),
            new: fixture.new.iter().map(td_executed).collect(),
        });
        assert_eq!(view.block_with_td_by_hash(fixture.new[1].hash())?.unwrap().1, expected);
        assert_eq!(
            fixture.provider.block_with_td_by_hash(fixture.new[1].hash())?.unwrap().1,
            expected
        );
        Ok(())
    }

    #[test]
    fn exact_td_budget_includes_search_and_anchor_without_quadratic_walks() -> eyre::Result<()> {
        let fixture = td_fixture()?;
        let mut blocks = fixture.new;
        for _ in 0..62 {
            blocks.push(td_child(blocks.last().unwrap(), 2));
        }
        fixture.provider.canonical_in_memory_state().update_chain(NewCanonicalChain::Commit {
            new: blocks.iter().map(td_executed).collect(),
        });
        let view = fixture.provider.consistent_provider()?;
        for index in [0, 31, 63] {
            let steps = 64 + index + 2;
            let mut remaining = steps;
            let result = view.block_with_td_by_hash_budget(blocks[index].hash(), &mut remaining)?;
            assert!(result.is_some());
            assert_eq!(remaining, 0, "search + each memory ancestor + durable attempts");
            assert!(view
                .block_with_td_by_hash_budget(blocks[index].hash(), &mut (steps - 1))?
                .is_none());
        }
        let mut remaining = 10;
        assert!(view
            .block_with_td_by_hash_budget(fixture.common.hash(), &mut remaining)?
            .is_none());
        assert_eq!(remaining, 0, "initial target search also consumes the limit");
        Ok(())
    }

    #[test]
    fn exact_td_writer_contention_is_unavailable_and_retry_succeeds() -> eyre::Result<()> {
        let fixture = td_fixture()?;
        let view = fixture.provider.consistent_provider()?;
        let static_files = fixture.factory.static_file_provider();
        let writer = static_files.latest_writer(StaticFileSegment::Headers)?;
        assert!(view.block_with_td_by_hash(fixture.old[1].hash())?.is_none());
        drop(writer);
        let (block, td) = view.block_with_td_by_hash(fixture.old[1].hash())?.unwrap();
        assert_eq!(block, fixture.old[1].clone().into_block());
        assert_eq!(td, fixture.factory.provider()?.header_td_by_number(3)?.unwrap());
        Ok(())
    }

    #[test]
    fn exact_td_rejects_wrong_parent_and_overflow_without_poisoning_later_reads() -> eyre::Result<()>
    {
        let fixture = td_fixture()?;
        let mut view = fixture.provider.consistent_provider()?;
        let child = td_child(&fixture.new[0], 1);
        view.head_block = Some(Arc::new(BlockState::with_parent(
            td_executed(&child),
            Some(Arc::new(BlockState::new(td_executed(&fixture.old[0])))),
        )));
        assert!(view.block_with_td_by_hash(child.hash())?.is_none());
        view.head_block = Some(Arc::new(BlockState::new(td_executed(&child))));
        assert!(
            view.block_with_td_by_hash(child.hash())?.is_none(),
            "missing parent is not a height fallback"
        );
        let mut overflow = td_child(&fixture.common, 1).into_block();
        overflow.header.difficulty = U256::MAX;
        let overflow = overflow.seal_slow();
        view.head_block = Some(Arc::new(BlockState::new(td_executed(&overflow))));
        assert!(view.block_with_td_by_hash(overflow.hash())?.is_none());
        view.head_block = None;
        assert!(view.block_with_td_by_hash(fixture.old[1].hash())?.is_some());
        Ok(())
    }

    #[test]
    fn exact_td_legacy_anchor_requires_same_ro_canonical_hash_binding() -> eyre::Result<()> {
        let fixture = td_fixture()?;
        let genesis = fixture.factory.chain_spec().genesis_header().clone();
        let expected_parent = fixture.factory.provider()?.header_td_by_number(3)?.unwrap();
        let writer = fixture.factory.provider_rw()?;
        {
            let static_files = fixture.factory.static_file_provider();
            let mut headers = static_files.get_writer(0, StaticFileSegment::Headers)?;
            headers.prune_headers(4)?;
            headers.commit()?;
            headers.append_header(&genesis, &genesis.hash_slow())?;
            for block in [&fixture.common, &fixture.old[0], &fixture.old[1]] {
                headers.append_header(block.header(), &block.hash())?;
            }
        }
        writer.tx_ref().put::<tables::HeaderTerminalDifficulties>(3, expected_parent.into())?;
        writer.commit()?;
        let child = td_child(&fixture.old[1], 7);
        fixture
            .provider
            .canonical_in_memory_state()
            .update_chain(NewCanonicalChain::Commit { new: vec![td_executed(&child)] });
        let old_view = fixture.provider.consistent_provider()?;
        assert_eq!(old_view.storage_provider.block_number(fixture.old[1].hash())?, Some(3));
        assert!(
            old_view.block_with_td_by_hash_budget(child.hash(), &mut 3)?.is_none(),
            "HeaderNumbers is not a canonical proof for legacy TD"
        );
        let writer = fixture.factory.provider_rw()?;
        writer.tx_ref().put::<tables::CanonicalHeaders>(3, fixture.old[1].hash())?;
        writer.commit()?;
        assert!(
            old_view.block_with_td_by_hash_budget(child.hash(), &mut 3)?.is_none(),
            "existing RO cannot borrow a newer binding"
        );
        let view = fixture.provider.consistent_provider()?;
        assert_eq!(
            view.block_with_td_by_hash_budget(child.hash(), &mut 3)?.unwrap().1,
            expected_parent + U256::from(7)
        );
        let writer = fixture.factory.provider_rw()?;
        writer.tx_ref().put::<tables::CanonicalHeaders>(3, B256::repeat_byte(9))?;
        writer.commit()?;
        assert!(fixture
            .provider
            .consistent_provider()?
            .block_with_td_by_hash_budget(child.hash(), &mut 3)?
            .is_none());
        Ok(())
    }

    #[test]
    fn exact_td_genesis_alias_requires_real_header_or_stored_pair() -> eyre::Result<()> {
        let mut spec = MAINNET.as_ref().clone();
        let genesis =
            Block { header: spec.genesis_header().clone(), ..Default::default() }.seal_slow();
        let alias = B256::repeat_byte(9);
        assert_ne!(alias, genesis.hash());
        spec.genesis_header = SealedHeader::new(genesis.header().clone(), alias);
        let factory = create_test_provider_factory_with_chain_spec(Arc::new(spec));
        let writer = factory.provider_rw()?;
        writer.insert_block(&genesis.clone().try_recover().unwrap())?;
        writer.commit()?;
        let provider = BlockchainProvider::new(factory)?;
        let real_child = td_child(&genesis, 2);
        let mut aliased = real_child.clone().into_block();
        aliased.header.parent_hash = alias;
        let aliased = aliased.seal_slow();
        provider
            .canonical_in_memory_state()
            .update_chain(NewCanonicalChain::Commit { new: vec![td_executed(&aliased)] });
        assert!(
            provider.block_with_td_by_hash(aliased.hash())?.is_none(),
            "a declared alias must not borrow an unrelated genesis header's difficulty"
        );
        provider.canonical_in_memory_state().update_chain(NewCanonicalChain::Reorg {
            old: vec![td_executed(&aliased)],
            new: vec![td_executed(&real_child)],
        });
        let view = provider.consistent_provider()?;
        // The mismatched declaration does not invalidate a real hash-bound stored genesis row.
        assert_eq!(view.block_with_td_by_hash(genesis.hash())?.unwrap().1, genesis.difficulty);
        let mut remaining = 3;
        assert_eq!(
            view.block_with_td_by_hash_budget(real_child.hash(), &mut remaining)?.unwrap().1,
            genesis.difficulty + U256::from(2)
        );
        assert_eq!(remaining, 0, "the fallback genesis row read consumes the shared budget");
        assert!(view.block_with_td_by_hash_budget(real_child.hash(), &mut 2)?.is_none());
        Ok(())
    }

    fn random_blocks(
        rng: &mut impl Rng,
        database_blocks: usize,
        in_memory_blocks: usize,
        requests_count: Option<Range<u8>>,
        withdrawals_count: Option<Range<u8>>,
        tx_count: impl RangeBounds<u8>,
    ) -> (Vec<SealedBlock<Block>>, Vec<SealedBlock<Block>>) {
        let block_range = (database_blocks + in_memory_blocks - 1) as u64;

        let tx_start = match tx_count.start_bound() {
            Bound::Included(&n) | Bound::Excluded(&n) => n,
            Bound::Unbounded => u8::MIN,
        };
        let tx_end = match tx_count.end_bound() {
            Bound::Included(&n) | Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => u8::MAX,
        };

        let blocks = random_block_range(
            rng,
            0..=block_range,
            BlockRangeParams {
                parent: Some(B256::ZERO),
                tx_count: tx_start..tx_end,
                requests_count,
                withdrawals_count,
            },
        );
        let (database_blocks, in_memory_blocks) = blocks.split_at(database_blocks);
        (database_blocks.to_vec(), in_memory_blocks.to_vec())
    }

    #[test]
    fn test_block_reader_find_block_by_hash() -> eyre::Result<()> {
        // Initialize random number generator and provider factory
        let mut rng = generators::rng();
        let factory = create_test_provider_factory();

        // Generate 10 random blocks and split into database and in-memory blocks
        let blocks = random_block_range(
            &mut rng,
            0..=10,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 0..1, ..Default::default() },
        );
        let (database_blocks, in_memory_blocks) = blocks.split_at(5);

        // Insert first 5 blocks into the database
        let provider_rw = factory.provider_rw()?;
        for block in database_blocks {
            provider_rw.insert_block(
                &block.clone().try_recover().expect("failed to seal block with senders"),
            )?;
        }
        provider_rw.commit()?;

        // Create a new provider
        let provider = BlockchainProvider::new(factory)?;
        let consistent_provider = provider.consistent_provider()?;

        // Useful blocks
        let first_db_block = database_blocks.first().unwrap();
        let first_in_mem_block = in_memory_blocks.first().unwrap();
        let last_in_mem_block = in_memory_blocks.last().unwrap();

        // No block in memory before setting in memory state
        assert_eq!(
            consistent_provider.find_block_by_hash(first_in_mem_block.hash(), BlockSource::Any)?,
            None
        );
        assert!(consistent_provider
            .find_sealed_or_recovered_block(first_in_mem_block.hash(), BlockSource::Any)?
            .is_none());
        assert_eq!(
            consistent_provider
                .find_block_by_hash(first_in_mem_block.hash(), BlockSource::Canonical)?,
            None
        );
        // No pending block in memory
        assert_eq!(
            consistent_provider
                .find_block_by_hash(first_in_mem_block.hash(), BlockSource::Pending)?,
            None
        );

        // Insert first block into the in-memory state
        let in_memory_block_senders =
            first_in_mem_block.senders().expect("failed to recover senders");
        let chain = NewCanonicalChain::Commit {
            new: vec![ExecutedBlock {
                recovered_block: Arc::new(RecoveredBlock::new_sealed(
                    first_in_mem_block.clone(),
                    in_memory_block_senders,
                )),
                ..Default::default()
            }],
        };
        consistent_provider.canonical_in_memory_state.update_chain(chain);
        let consistent_provider = provider.consistent_provider()?;

        // Now the block should be found in memory
        assert_eq!(
            consistent_provider.find_block_by_hash(first_in_mem_block.hash(), BlockSource::Any)?,
            Some(first_in_mem_block.clone().into_block())
        );
        let block = consistent_provider
            .find_sealed_or_recovered_block(first_in_mem_block.hash(), BlockSource::Any)?
            .expect("in-memory block should be found");
        assert_eq!(block.sealed_block(), first_in_mem_block);
        assert!(block.recovered_block().is_some());

        assert_eq!(
            consistent_provider
                .find_block_by_hash(first_in_mem_block.hash(), BlockSource::Canonical)?,
            Some(first_in_mem_block.clone().into_block())
        );
        let block = consistent_provider
            .find_sealed_or_recovered_block(first_in_mem_block.hash(), BlockSource::Canonical)?
            .expect("canonical in-memory block should be found");
        assert_eq!(block.sealed_block(), first_in_mem_block);
        assert!(block.recovered_block().is_some());

        // Find the first block in database by hash
        assert_eq!(
            consistent_provider.find_block_by_hash(first_db_block.hash(), BlockSource::Any)?,
            Some(first_db_block.clone().into_block())
        );
        let block = consistent_provider
            .find_sealed_or_recovered_block(first_db_block.hash(), BlockSource::Any)?
            .expect("database block should be found");
        assert_eq!(block.sealed_block(), first_db_block);
        assert!(block.recovered_block().is_none());

        assert_eq!(
            consistent_provider
                .find_block_by_hash(first_db_block.hash(), BlockSource::Canonical)?,
            Some(first_db_block.clone().into_block())
        );
        let block = consistent_provider
            .find_sealed_or_recovered_block(first_db_block.hash(), BlockSource::Canonical)?
            .expect("canonical database block should be found");
        assert_eq!(block.sealed_block(), first_db_block);
        assert!(block.recovered_block().is_none());

        // No pending block in database
        assert_eq!(
            consistent_provider.find_block_by_hash(first_db_block.hash(), BlockSource::Pending)?,
            None
        );
        assert!(consistent_provider
            .find_sealed_or_recovered_block(first_db_block.hash(), BlockSource::Pending)?
            .is_none());

        // Insert the last block into the pending state
        provider.canonical_in_memory_state.set_pending_block(ExecutedBlock {
            recovered_block: Arc::new(RecoveredBlock::new_sealed(
                last_in_mem_block.clone(),
                Default::default(),
            )),
            ..Default::default()
        });

        // Now the last block should be found in memory
        assert_eq!(
            consistent_provider
                .find_block_by_hash(last_in_mem_block.hash(), BlockSource::Pending)?,
            Some(last_in_mem_block.clone_block())
        );
        let block = consistent_provider
            .find_sealed_or_recovered_block(last_in_mem_block.hash(), BlockSource::Pending)?
            .expect("pending block should be found");
        assert_eq!(block.sealed_block(), last_in_mem_block);
        assert!(block.recovered_block().is_some());

        Ok(())
    }

    #[test]
    fn test_block_reader_block() -> eyre::Result<()> {
        // Initialize random number generator and provider factory
        let mut rng = generators::rng();
        let factory = create_test_provider_factory();

        // Generate 10 random blocks and split into database and in-memory blocks
        let blocks = random_block_range(
            &mut rng,
            0..=10,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 0..1, ..Default::default() },
        );
        let (database_blocks, in_memory_blocks) = blocks.split_at(5);

        // Insert first 5 blocks into the database
        let provider_rw = factory.provider_rw()?;
        for block in database_blocks {
            provider_rw.insert_block(
                &block.clone().try_recover().expect("failed to seal block with senders"),
            )?;
        }
        provider_rw.commit()?;

        // Create a new provider
        let provider = BlockchainProvider::new(factory)?;
        let consistent_provider = provider.consistent_provider()?;

        // First in memory block
        let first_in_mem_block = in_memory_blocks.first().unwrap();
        // First database block
        let first_db_block = database_blocks.first().unwrap();

        // First in memory block should not be found yet as not integrated to the in-memory state
        assert_eq!(
            consistent_provider.block(BlockHashOrNumber::Hash(first_in_mem_block.hash()))?,
            None
        );
        assert_eq!(
            consistent_provider.block(BlockHashOrNumber::Number(first_in_mem_block.number))?,
            None
        );

        // Insert first block into the in-memory state
        let in_memory_block_senders =
            first_in_mem_block.senders().expect("failed to recover senders");
        let chain = NewCanonicalChain::Commit {
            new: vec![ExecutedBlock {
                recovered_block: Arc::new(RecoveredBlock::new_sealed(
                    first_in_mem_block.clone(),
                    in_memory_block_senders,
                )),
                ..Default::default()
            }],
        };
        consistent_provider.canonical_in_memory_state.update_chain(chain);

        let consistent_provider = provider.consistent_provider()?;

        // First in memory block should be found
        assert_eq!(
            consistent_provider.block(BlockHashOrNumber::Hash(first_in_mem_block.hash()))?,
            Some(first_in_mem_block.clone().into_block())
        );
        assert_eq!(
            consistent_provider.block(BlockHashOrNumber::Number(first_in_mem_block.number))?,
            Some(first_in_mem_block.clone().into_block())
        );

        // First database block should be found
        assert_eq!(
            consistent_provider.block(BlockHashOrNumber::Hash(first_db_block.hash()))?,
            Some(first_db_block.clone().into_block())
        );
        assert_eq!(
            consistent_provider.block(BlockHashOrNumber::Number(first_db_block.number))?,
            Some(first_db_block.clone().into_block())
        );

        Ok(())
    }

    #[test]
    fn test_changeset_reader() -> eyre::Result<()> {
        let mut rng = generators::rng();

        let (database_blocks, in_memory_blocks) =
            random_blocks(&mut rng, TEST_BLOCKS_COUNT, 1, None, None, 0..1);

        let first_database_block = database_blocks.first().map(|block| block.number).unwrap();
        let last_database_block = database_blocks.last().map(|block| block.number).unwrap();
        let first_in_memory_block = in_memory_blocks.first().map(|block| block.number).unwrap();

        let accounts = random_eoa_accounts(&mut rng, 2);

        let (database_changesets, database_state) = random_changeset_range(
            &mut rng,
            &database_blocks,
            accounts.into_iter().map(|(address, account)| (address, (account, Vec::new()))),
            0..0,
            0..0,
        );
        let (in_memory_changesets, in_memory_state) = random_changeset_range(
            &mut rng,
            &in_memory_blocks,
            database_state.iter().map(|(address, (account, storage))| {
                (*address, (account.clone(), storage.clone()))
            }),
            0..0,
            0..0,
        );

        let factory = create_test_provider_factory();

        let provider_rw = factory.provider_rw()?;
        provider_rw.append_blocks_with_state(
            database_blocks
                .into_iter()
                .map(|b| b.try_recover().expect("failed to seal block with senders"))
                .collect(),
            &ExecutionOutcome {
                bundle: BundleState::new(
                    database_state.into_iter().map(|(address, (account, _))| {
                        (address, None, Some(account.into()), Default::default())
                    }),
                    database_changesets.iter().map(|block_changesets| {
                        block_changesets.iter().map(|(address, account, _)| {
                            (*address, Some(Some((account.clone()).into())), [])
                        })
                    }),
                    Vec::new(),
                ),
                first_block: first_database_block,
                ..Default::default()
            },
            Default::default(),
        )?;
        provider_rw.commit()?;

        let provider = BlockchainProvider::new(factory)?;

        let in_memory_changesets = in_memory_changesets.into_iter().next().unwrap();
        let chain = NewCanonicalChain::Commit {
            new: vec![in_memory_blocks
                .first()
                .map(|block| {
                    let senders = block.senders().expect("failed to recover senders");
                    ExecutedBlock {
                        recovered_block: Arc::new(RecoveredBlock::new_sealed(
                            block.clone(),
                            senders,
                        )),
                        execution_output: Arc::new(BlockExecutionOutput {
                            state: BundleState::new(
                                in_memory_state.into_iter().map(|(address, (account, _))| {
                                    (address, None, Some(account.into()), Default::default())
                                }),
                                [in_memory_changesets.iter().map(|(address, account, _)| {
                                    (*address, Some(Some((account.clone()).into())), Vec::new())
                                })],
                                [],
                            ),
                            result: BlockExecutionResult {
                                receipts: Default::default(),
                                requests: Default::default(),
                                gas_used: 0,
                                blob_gas_used: 0,
                            },
                        }),
                        ..Default::default()
                    }
                })
                .unwrap()],
        };
        provider.canonical_in_memory_state.update_chain(chain);

        let consistent_provider = provider.consistent_provider()?;

        assert_eq!(
            consistent_provider.account_block_changeset(last_database_block).unwrap(),
            database_changesets
                .into_iter()
                .next_back()
                .unwrap()
                .into_iter()
                .sorted_by_key(|(address, _, _)| *address)
                .map(|(address, account, _)| AccountBeforeTx { address, info: Some(account) })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            consistent_provider.account_block_changeset(first_in_memory_block).unwrap(),
            in_memory_changesets
                .into_iter()
                .sorted_by_key(|(address, _, _)| *address)
                .map(|(address, account, _)| AccountBeforeTx { address, info: Some(account) })
                .collect::<Vec<_>>()
        );

        Ok(())
    }
    #[test]
    fn test_get_state_storage_value_plain_state() -> eyre::Result<()> {
        use alloy_primitives::U256;
        use reth_db_api::{models::StorageSettings, tables, transaction::DbTxMut};
        use reth_primitives_traits::StorageEntry;
        use reth_storage_api::StorageSettingsCache;
        use std::collections::HashMap;

        let address = alloy_primitives::Address::with_last_byte(1);
        let account = reth_primitives_traits::Account {
            nonce: 1,
            balance: U256::from(1000),
            ..Default::default()
        };
        let slot = U256::from(0x42);
        let slot_b256 = B256::from(slot);

        let mut rng = generators::rng();
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v1());

        let blocks = random_block_range(
            &mut rng,
            0..=1,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 0..1, ..Default::default() },
        );

        let provider_rw = factory.provider_rw()?;
        provider_rw.append_blocks_with_state(
            blocks
                .into_iter()
                .map(|b| b.try_recover().expect("failed to seal block with senders"))
                .collect(),
            &ExecutionOutcome {
                bundle: BundleState::new(
                    [(address, None, Some(account.clone().into()), {
                        let mut s = HashMap::default();
                        s.insert(slot, (U256::ZERO, U256::from(100)));
                        s
                    })],
                    [
                        Vec::new(),
                        vec![(
                            address,
                            Some(Some(account.clone().into())),
                            vec![(slot, U256::ZERO)],
                        )],
                    ],
                    [],
                ),
                first_block: 0,
                ..Default::default()
            },
            Default::default(),
        )?;

        provider_rw.tx_ref().put::<tables::PlainStorageState>(
            address,
            StorageEntry { key: slot_b256, value: U256::from(100) },
        )?;
        provider_rw.tx_ref().put::<tables::PlainAccountState>(address, account)?;

        provider_rw.commit()?;

        let provider = BlockchainProvider::new(factory)?;
        let outcome = provider.get_state(1)?.expect("should return execution outcome");

        let state = &outcome.bundle.state;
        let account_state = state.get(&address).expect("should have account in bundle state");
        let storage = &account_state.storage;

        let storage_slot = storage.get(&slot).expect("should have the slot in storage");

        assert_eq!(
            storage_slot.present_value,
            U256::from(100),
            "present_value should be 100 (the actual value in PlainStorageState)"
        );

        Ok(())
    }

    #[test]
    fn test_storage_changeset_consistent_keys_plain_state() -> eyre::Result<()> {
        use alloy_primitives::U256;
        use reth_db_api::models::StorageSettings;
        use reth_storage_api::{StorageChangeSetReader, StorageSettingsCache};
        use std::collections::HashMap;

        let mut rng = generators::rng();
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v1());

        let (database_blocks, in_memory_blocks) = random_blocks(&mut rng, 1, 1, None, None, 0..1);

        let address = alloy_primitives::Address::with_last_byte(1);
        let account = reth_primitives_traits::Account {
            nonce: 1,
            balance: U256::from(1000),
            ..Default::default()
        };
        let slot = U256::from(0x42);

        let provider_rw = factory.provider_rw()?;
        provider_rw.append_blocks_with_state(
            database_blocks
                .into_iter()
                .map(|b| b.try_recover().expect("failed to seal block with senders"))
                .collect(),
            &ExecutionOutcome {
                bundle: BundleState::new(
                    [(address, None, Some(account.clone().into()), {
                        let mut s = HashMap::default();
                        s.insert(slot, (U256::ZERO, U256::from(100)));
                        s
                    })],
                    [[(address, Some(Some(account.clone().into())), vec![(slot, U256::ZERO)])]],
                    [],
                ),
                first_block: 0,
                ..Default::default()
            },
            Default::default(),
        )?;
        provider_rw.commit()?;

        let provider = BlockchainProvider::new(factory)?;

        let in_mem_block = in_memory_blocks.first().unwrap();
        let senders = in_mem_block.senders().expect("failed to recover senders");
        let chain = NewCanonicalChain::Commit {
            new: vec![ExecutedBlock {
                recovered_block: Arc::new(RecoveredBlock::new_sealed(
                    in_mem_block.clone(),
                    senders,
                )),
                execution_output: Arc::new(BlockExecutionOutput {
                    state: BundleState::new(
                        [(address, None, Some(account.clone().into()), {
                            let mut s = HashMap::default();
                            s.insert(slot, (U256::from(100), U256::from(200)));
                            s
                        })],
                        [[(address, Some(Some(account.into())), vec![(slot, U256::from(100))])]],
                        [],
                    ),
                    result: BlockExecutionResult {
                        receipts: Default::default(),
                        requests: Default::default(),
                        gas_used: 0,
                        blob_gas_used: 0,
                    },
                }),
                ..Default::default()
            }],
        };
        provider.canonical_in_memory_state.update_chain(chain);

        let consistent_provider = provider.consistent_provider()?;

        let db_changeset = consistent_provider.storage_changeset(0)?;
        let mem_changeset = consistent_provider.storage_changeset(1)?;

        let slot_b256 = B256::from(slot);

        assert_eq!(db_changeset.len(), 1);
        assert_eq!(mem_changeset.len(), 1);

        let db_key = db_changeset[0].1.key;
        let mem_key = mem_changeset[0].1.key;

        assert_eq!(db_key, slot_b256, "DB changeset should use plain (unhashed) key");
        assert_eq!(mem_key, slot_b256, "In-memory changeset should use plain (unhashed) key");
        assert_eq!(
            db_key, mem_key,
            "DB and in-memory changesets should return the same key format (plain) for the same logical slot"
        );

        Ok(())
    }

    #[test]
    fn test_storage_changesets_range_consistent_keys_plain_state() -> eyre::Result<()> {
        use alloy_primitives::U256;
        use reth_db_api::models::StorageSettings;
        use reth_storage_api::{StorageChangeSetReader, StorageSettingsCache};
        use std::collections::HashMap;

        let mut rng = generators::rng();
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v1());

        let (database_blocks, in_memory_blocks) = random_blocks(&mut rng, 2, 1, None, None, 0..1);

        let address = alloy_primitives::Address::with_last_byte(1);
        let account = reth_primitives_traits::Account {
            nonce: 1,
            balance: U256::from(1000),
            ..Default::default()
        };
        let slot = U256::from(0x42);

        let provider_rw = factory.provider_rw()?;
        provider_rw.append_blocks_with_state(
            database_blocks
                .into_iter()
                .map(|b| b.try_recover().expect("failed to seal block with senders"))
                .collect(),
            &ExecutionOutcome {
                bundle: BundleState::new(
                    [(address, None, Some(account.clone().into()), {
                        let mut s = HashMap::default();
                        s.insert(slot, (U256::ZERO, U256::from(100)));
                        s
                    })],
                    vec![
                        vec![(
                            address,
                            Some(Some(account.clone().into())),
                            vec![(slot, U256::ZERO)],
                        )],
                        vec![],
                    ],
                    [],
                ),
                first_block: 0,
                ..Default::default()
            },
            Default::default(),
        )?;
        provider_rw.commit()?;

        let provider = BlockchainProvider::new(factory)?;

        let in_mem_block = in_memory_blocks.first().unwrap();
        let senders = in_mem_block.senders().expect("failed to recover senders");
        let chain = NewCanonicalChain::Commit {
            new: vec![ExecutedBlock {
                recovered_block: Arc::new(RecoveredBlock::new_sealed(
                    in_mem_block.clone(),
                    senders,
                )),
                execution_output: Arc::new(BlockExecutionOutput {
                    state: BundleState::new(
                        [(address, None, Some(account.clone().into()), {
                            let mut s = HashMap::default();
                            s.insert(slot, (U256::from(100), U256::from(200)));
                            s
                        })],
                        [[(address, Some(Some(account.into())), vec![(slot, U256::from(100))])]],
                        [],
                    ),
                    result: BlockExecutionResult {
                        receipts: Default::default(),
                        requests: Default::default(),
                        gas_used: 0,
                        blob_gas_used: 0,
                    },
                }),
                ..Default::default()
            }],
        };
        provider.canonical_in_memory_state.update_chain(chain);

        let consistent_provider = provider.consistent_provider()?;

        let all_changesets = consistent_provider.storage_changesets_range(0..=2)?;

        assert_eq!(all_changesets.len(), 2, "should have one changeset entry per block");

        let slot_b256 = B256::from(slot);
        let keys: Vec<B256> = all_changesets.iter().map(|(_, entry)| entry.key).collect();

        assert_eq!(
            keys[0], keys[1],
            "same logical slot should produce identical keys whether from DB or memory"
        );
        assert_eq!(
            keys[0], slot_b256,
            "keys should be plain/unhashed when use_hashed_state is false"
        );

        Ok(())
    }
}
