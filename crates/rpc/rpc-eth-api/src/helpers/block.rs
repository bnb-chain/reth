//! Database access for `eth_` block RPC methods. Loads block and receipt data w.r.t. network.

use super::{LoadPendingBlock, LoadReceipt, SpawnBlocking};
use crate::{
    node::RpcNodeCoreExt, EthApiTypes, FromEthApiError, FullEthApiTypes, RpcBlock, RpcNodeCore,
    RpcReceipt,
};
use alloy_consensus::{transaction::TxHashRef, TxReceipt};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::U256;
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{Block, BlockTransactions, Index};
use futures::Future;
use reth_node_api::BlockBody;
use reth_primitives_traits::{AlloyBlockHeader, RecoveredBlock, SealedHeader, TransactionMeta};
use reth_rpc_convert::{transaction::ConvertReceiptInput, RpcConvert, RpcHeader};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::{
    BlockIdReader, BlockReader, BlockReaderIdExt, HeaderProvider, ProviderHeader, ProviderReceipt,
    ProviderTx,
};
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::{collections::HashSet, sync::Arc};

/// Max blocks to walk back when deriving the probabilistic finalized height for
/// `eth_getFinalizedBlock`/`eth_getFinalizedHeader`.
const MAX_VALIDATOR_LOOKBACK: usize = 1000;

/// Resolves the distinct-validator threshold for a `verified_validator_num` request value, per
/// go-bsc semantics: `-1` = ceil(N/2), `-2` = ceil(2N/3), `-3` = N, `>=1` = explicit threshold.
fn resolved_validators_threshold(
    verified_validator_num: i64,
    active_validator_count: Option<usize>,
) -> Result<usize, EthApiError> {
    let missing_validator_count = || -> EthApiError {
        EthApiError::InvalidParams(format!(
            "Unable to derive validator-count from request value {verified_validator_num} without chain validator set"
        ))
    };

    match verified_validator_num {
        -3..=-1 => {
            let active_validator_count =
                active_validator_count.ok_or_else(missing_validator_count)?;
            match verified_validator_num {
                -1 => Ok(active_validator_count.div_ceil(2)),
                -2 => Ok((active_validator_count * 2).div_ceil(3)),
                _ => Ok(active_validator_count),
            }
        }
        value if value < 1 => Err(EthApiError::InvalidParams(format!(
            "{value} neither within the range [1,{}] nor the range [-3,-1]",
            active_validator_count.unwrap_or(0)
        ))),
        value
            if active_validator_count
                .is_some_and(|validator_count| value > validator_count as i64) =>
        {
            Err(EthApiError::InvalidParams(format!(
                "{value} neither within the range [1,{}] nor the range [-3,-1]",
                active_validator_count.unwrap_or(0)
            )))
        }
        value => Ok(value as usize),
    }
}

/// Result type of the fetched block receipts.
pub type BlockReceiptsResult<N, E> = Result<Option<Vec<RpcReceipt<N>>>, E>;
/// Result type of the fetched block and its receipts.
pub type BlockAndReceiptsResult<Eth> = Result<
    Option<(
        Arc<RecoveredBlock<<<Eth as RpcNodeCore>::Provider as BlockReader>::Block>>,
        Arc<Vec<ProviderReceipt<<Eth as RpcNodeCore>::Provider>>>,
    )>,
    <Eth as EthApiTypes>::Error,
>;

/// Block related functions for the [`EthApiServer`](crate::EthApiServer) trait in the
/// `eth_` namespace.
pub trait EthBlocks: LoadBlock<RpcConvert: RpcConvert<Primitives = Self::Primitives>> {
    /// Returns the block header for the given block id.
    fn rpc_block_header(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<RpcHeader<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
    {
        async move {
            let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
            let sealed_header = block.clone_sealed_header();
            let td = self.total_difficulty_for(&sealed_header);
            let header = self.converter().convert_header(sealed_header, block.rlp_length(), td)?;
            Ok(Some(header))
        }
    }

    /// Returns the total difficulty at `header`'s height (go-bsc attaches it to block/header
    /// responses). Falls back to `parent_td + difficulty` for blocks not yet in the DB (pending).
    fn total_difficulty_for(
        &self,
        header: &SealedHeader<ProviderHeader<Self::Provider>>,
    ) -> Option<U256> {
        let block_number = header.number();
        match self.provider().header_td_by_number(block_number) {
            Ok(Some(td)) => Some(td),
            _ => {
                let parent_td =
                    self.provider().header_td_by_number(block_number.saturating_sub(1)).ok()??;
                Some(parent_td.saturating_add(header.difficulty()))
            }
        }
    }

    /// Returns the populated rpc block object for the given block id.
    ///
    /// If `full` is true, the block object will contain all transaction objects, otherwise it will
    /// only contain the transaction hashes.
    fn rpc_block(
        &self,
        block_id: BlockId,
        full: bool,
    ) -> impl Future<Output = Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
    {
        async move {
            let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };

            let block = block.clone_into_rpc_block(
                full.into(),
                |tx, tx_info| self.converter().fill(tx, tx_info),
                |header, size| {
                    let td = self.total_difficulty_for(&header);
                    self.converter().convert_header(header, size, td)
                },
            )?;
            Ok(Some(block))
        }
    }

    /// Derives the probabilistic finalized block number for BSC parlia fast finality.
    ///
    /// Walks back from the tip counting distinct block signers until `threshold` distinct
    /// validators are seen (a block is probabilistically final once that many validators have
    /// built on it), and returns the max of that and the fast-finalized height.
    /// `verified_validator_num` selects the threshold; see [`resolved_validators_threshold`].
    fn finalized_block_number(
        &self,
        verified_validator_num: i64,
    ) -> impl Future<Output = Result<Option<u64>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
        Self::Provider: BlockReaderIdExt,
    {
        async move {
            let latest_header = self
                .provider()
                .sealed_header_by_id(BlockNumberOrTag::Latest.into())
                .map_err(Self::Error::from_eth_err)?
                .ok_or_else(|| {
                    Self::Error::from_eth_err(EthApiError::HeaderNotFound(
                        BlockNumberOrTag::Latest.into(),
                    ))
                })?;

            let fast_finalized_header = self
                .provider()
                .sealed_header_by_id(BlockNumberOrTag::Finalized.into())
                .map_err(Self::Error::from_eth_err)?
                .ok_or_else(|| {
                    Self::Error::from_eth_err(EthApiError::HeaderNotFound(
                        BlockNumberOrTag::Finalized.into(),
                    ))
                })?;

            let lower_bound = fast_finalized_header.number().max(1);
            let active_validator_count = self.current_validators_len();
            let threshold =
                resolved_validators_threshold(verified_validator_num, active_validator_count)
                    .map_err(Self::Error::from_eth_err)?;
            if threshold == 0 {
                return Ok(Some(fast_finalized_header.number()));
            }

            let mut cursor = latest_header;
            let mut seen_signers = HashSet::with_capacity(threshold.max(1));
            let mut probabilistic_finalized = fast_finalized_header.number();
            for i in 0..=MAX_VALIDATOR_LOOKBACK {
                seen_signers.insert(cursor.beneficiary());
                probabilistic_finalized = cursor.number();

                if seen_signers.len() >= threshold {
                    break;
                }

                let parent_hash = cursor.parent_hash();
                if cursor.number() <= lower_bound {
                    break;
                }

                if i == MAX_VALIDATOR_LOOKBACK {
                    break;
                }
                cursor = self
                    .provider()
                    .sealed_header_by_hash(parent_hash)
                    .map_err(Self::Error::from_eth_err)?
                    .ok_or_else(|| {
                        Self::Error::from_eth_err(EthApiError::HeaderNotFound(parent_hash.into()))
                    })?;
            }

            Ok(Some(std::cmp::max(fast_finalized_header.number(), probabilistic_finalized)))
        }
    }

    /// Returns the finalized block header. Backs `eth_getFinalizedHeader`.
    fn rpc_finalized_header(
        &self,
        verified_validator_num: i64,
    ) -> impl Future<Output = Result<Option<RpcHeader<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
        Self::Provider: BlockReaderIdExt,
    {
        async move {
            let Some(finalized_block_number) =
                self.finalized_block_number(verified_validator_num).await?
            else {
                return Ok(None);
            };
            self.rpc_block_header(BlockNumberOrTag::Number(finalized_block_number).into()).await
        }
    }

    /// Returns the finalized block. Backs `eth_getFinalizedBlock`.
    fn rpc_finalized_block(
        &self,
        verified_validator_num: i64,
        full: bool,
    ) -> impl Future<Output = Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
        Self::Provider: BlockReaderIdExt,
    {
        async move {
            let Some(finalized_block_number) =
                self.finalized_block_number(verified_validator_num).await?
            else {
                return Ok(None);
            };
            self.rpc_block(BlockNumberOrTag::Number(finalized_block_number).into(), full).await
        }
    }

    /// Returns the number transactions in the given block.
    ///
    /// Returns `None` if the block does not exist
    fn block_transaction_count(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<usize>, Self::Error>> + Send {
        async move { Ok(self.recovered_block(block_id).await?.map(|b| b.body().transaction_count())) }
    }

    /// Helper function for `eth_getBlockReceipts`.
    ///
    /// Returns all transaction receipts in block, or `None` if block wasn't found.
    fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = BlockReceiptsResult<Self::NetworkTypes, Self::Error>> + Send
    where
        Self: LoadReceipt,
    {
        async move {
            if let Some((block, receipts)) = self.load_block_and_receipts(block_id).await? {
                let block_number = block.number();
                let base_fee = block.base_fee_per_gas();
                let block_hash = block.hash();
                let excess_blob_gas = block.excess_blob_gas();
                let timestamp = block.timestamp();
                let mut gas_used = 0;
                let mut next_log_index = 0;

                let inputs = block
                    .transactions_recovered()
                    .zip(Arc::unwrap_or_clone(receipts))
                    .enumerate()
                    .map(|(idx, (tx, receipt))| {
                        let meta = TransactionMeta {
                            tx_hash: *tx.tx_hash(),
                            index: idx as u64,
                            block_hash,
                            block_number,
                            base_fee,
                            excess_blob_gas,
                            timestamp,
                        };

                        let cumulative_gas_used = receipt.cumulative_gas_used();
                        let logs_len = receipt.logs().len();

                        let input = ConvertReceiptInput {
                            tx,
                            gas_used: cumulative_gas_used - gas_used,
                            next_log_index,
                            meta,
                            receipt,
                        };

                        gas_used = cumulative_gas_used;
                        next_log_index += logs_len;

                        input
                    })
                    .collect::<Vec<_>>();

                return Ok(self
                    .converter()
                    .convert_receipts_with_block(inputs, block.sealed_block())
                    .map(Some)?)
            }

            Ok(None)
        }
    }

    /// Helper method that loads a block and all its receipts.
    fn load_block_and_receipts(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = BlockAndReceiptsResult<Self>> + Send
    where
        Self: LoadReceipt,
        Self::Pool:
            TransactionPool<Transaction: PoolTransaction<Consensus = ProviderTx<Self::Provider>>>,
    {
        async move {
            if block_id.is_pending() {
                if self.pending_block_kind().is_none() {
                    return Ok(None);
                }

                // First, try to get the pending block from the provider, in case we already
                // received the actual pending block from the CL.
                if let Some((block, receipts)) = self
                    .provider()
                    .pending_block_and_receipts()
                    .map_err(Self::Error::from_eth_err)?
                {
                    return Ok(Some((Arc::new(block), Arc::new(receipts))));
                }

                // If no pending block from provider, build the pending block locally.
                if let Some(pending) = self.local_pending_block().await? {
                    return Ok(Some((pending.block, pending.receipts)));
                }
            }

            if let Some(block_hash) =
                self.provider().block_hash_for_id(block_id).map_err(Self::Error::from_eth_err)? &&
                let Some((block, receipts)) = self
                    .cache()
                    .get_block_and_receipts(block_hash)
                    .await
                    .map_err(Self::Error::from_eth_err)?
            {
                return Ok(Some((block, receipts)));
            }

            Ok(None)
        }
    }

    /// Returns uncle headers of given block.
    ///
    /// Returns an empty vec if there are none.
    #[expect(clippy::type_complexity)]
    fn ommers(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<Vec<ProviderHeader<Self::Provider>>>, Self::Error>> + Send
    {
        async move {
            if let Some(block) = self.recovered_block(block_id).await? {
                Ok(block.body().ommers().map(|o| o.to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// Returns uncle block at given index in given block.
    ///
    /// Returns `None` if index out of range.
    fn ommer_by_block_and_index(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> impl Future<Output = Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>> + Send
    {
        async move {
            let uncles = self
                .recovered_block(block_id)
                .await?
                .map(|block| block.body().ommers().map(|o| o.to_vec()).unwrap_or_default())
                .unwrap_or_default();

            uncles
                .into_iter()
                .nth(index.into())
                .map(|header| {
                    let block =
                        alloy_consensus::Block::<alloy_consensus::TxEnvelope, _>::uncle(header);
                    let size = block.length();
                    let header = self.converter().convert_header(
                        SealedHeader::new_unhashed(block.header),
                        size,
                        None,
                    )?;
                    Ok(Block {
                        uncles: vec![],
                        header,
                        transactions: BlockTransactions::Uncle,
                        withdrawals: None,
                    })
                })
                .transpose()
        }
    }
}

/// Loads a block from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` blocks RPC methods.
pub trait LoadBlock: LoadPendingBlock + SpawnBlocking + RpcNodeCoreExt {
    /// Returns the block object for the given block id.
    #[expect(clippy::type_complexity)]
    fn recovered_block(
        &self,
        block_id: BlockId,
    ) -> impl Future<
        Output = Result<
            Option<Arc<RecoveredBlock<<Self::Provider as BlockReader>::Block>>>,
            Self::Error,
        >,
    > + Send {
        async move {
            if block_id.is_pending() {
                if self.pending_block_kind().is_none() {
                    return Ok(None);
                }

                // Pending block can be fetched directly without need for caching
                if let Some(pending_block) =
                    self.provider().pending_block().map_err(Self::Error::from_eth_err)?
                {
                    return Ok(Some(Arc::new(pending_block)));
                }

                // If no pending block from provider, try to get local pending block
                return match self.local_pending_block().await? {
                    Some(pending) => Ok(Some(pending.block)),
                    None => Ok(None),
                };
            }

            let block_hash = match self
                .provider()
                .block_hash_for_id(block_id)
                .map_err(Self::Error::from_eth_err)?
            {
                Some(block_hash) => block_hash,
                None => return Ok(None),
            };

            self.cache().get_recovered_block(block_hash).await.map_err(Self::Error::from_eth_err)
        }
    }
}
