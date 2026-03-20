//! Database access for `eth_` block RPC methods. Loads block and receipt data w.r.t. network.

use super::{LoadPendingBlock, LoadReceipt, SpawnBlocking};
use crate::{
    node::RpcNodeCoreExt, EthApiTypes, FromEthApiError, FullEthApiTypes, RpcBlock, RpcNodeCore,
    RpcReceipt,
};
use alloy_consensus::{transaction::TxHashRef, TxReceipt};
use alloy_eips::{BlockId, BlockNumberOrTag};
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
use tracing::{trace, warn};

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

const MAX_VALIDATOR_LOOKBACK: usize = 1000;

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
        async move { Ok(self.rpc_block(block_id, false).await?.map(|block| block.header)) }
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
                    let block_number = header.number();
                    let td = match self.provider().header_td_by_number(block_number) {
                        Ok(Some(td)) => {
                            Some(td)
                        }
                        Ok(None) | Err(_) => {
                            // Block not in database yet (e.g., pending block)
                            // Calculate TD = parent_td + current_difficulty
                            trace!(target: "rpc::eth", ?block_id, block_number, "Block not in DB, calculating TD from parent");

                            let parent_number = block_number.saturating_sub(1);
                            match self.provider().header_td_by_number(parent_number) {
                                Ok(Some(parent_td)) => {
                                    let current_difficulty = header.difficulty();
                                    let calculated_td = parent_td.saturating_add(current_difficulty);

                                    trace!(
                                        target: "rpc::eth", 
                                        ?block_id,
                                        block_number,
                                        parent_number,
                                        ?parent_td,
                                        ?current_difficulty,
                                        ?calculated_td,
                                        "Calculated TD from parent"
                                    );

                                    Some(calculated_td)
                                }
                                _ => {
                                    warn!(
                                        target: "rpc::eth",
                                        ?block_id,
                                        block_number,
                                        parent_number,
                                        "Parent TD not found, returning None"
                                    );
                                    None
                                }
                            }
                        }
                    };
                    self.converter().convert_header(header, size, td)
                },
            )?;
            Ok(Some(block))
        }
    }

    /// Returns the finalized block header.
    ///
    /// BSC compatibility:
    /// - `verified_validator_num` is required to determine the probabilistic finalized height.
    ///   Accepted values are:
    ///   - `-1`: `ceil(number_of_validators / 2)`
    ///   - `-2`: `ceil(number_of_validators * 2 / 3)`
    ///   - `-3`: `number_of_validators`
    ///   - or `>=1`: explicit validator threshold.
    fn rpc_finalized_header(
        &self,
        verified_validator_num: i64,
    ) -> impl Future<Output = Result<Option<RpcHeader<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
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

    /// Returns the finalized block.
    ///
    /// BSC compatibility:
    /// - `verified_validator_num` is required to determine the probabilistic finalized height.
    ///   Accepted values are:
    ///   - `-1`: `ceil(number_of_validators / 2)`
    ///   - `-2`: `ceil(number_of_validators * 2 / 3)`
    ///   - `-3`: `number_of_validators`
    ///   - or `>=1`: explicit validator threshold.
    ///
    /// If `full` is true, the block object will contain all transaction objects, otherwise it will
    /// only contain the transaction hashes.
    fn rpc_finalized_block(
        &self,
        verified_validator_num: i64,
        full: bool,
    ) -> impl Future<Output = Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
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

    /// Returns the best-effort finalized block number according to `verified_validator_num`.
    ///
    /// BSC compatibility:
    /// - `verified_validator_num` is required to determine the probabilistic finalized height.
    ///   Accepted values are:
    ///   - `-1`: `ceil(number_of_validators / 2)`
    ///   - `-2`: `ceil(number_of_validators * 2 / 3)`
    ///   - `-3`: `number_of_validators`
    ///   - or `>=1`: explicit validator threshold.
    fn finalized_block_number(
        &self,
        verified_validator_num: i64,
    ) -> impl Future<Output = Result<Option<u64>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
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
                resolved_validators_threshold(verified_validator_num, active_validator_count)?;
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

    /// Returns the number transactions in the given block.
    ///
    /// Returns `None` if the block does not exist
    fn block_transaction_count(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<usize>, Self::Error>> + Send {
        async move {
            // If no pending block from provider, build the pending block locally.
            if block_id.is_pending() {
                if let Some(block) =
                    self.provider().pending_block().map_err(Self::Error::from_eth_err)?
                {
                    return Ok(Some(block.body().transaction_count()));
                }

                // If no pending block from provider, try to get local pending block
                if let Some(pending) = self.local_pending_block().await? {
                    return Ok(Some(pending.block.body().transaction_count()));
                }

                return Ok(None);
            }

            let block_hash = match self
                .provider()
                .block_hash_for_id(block_id)
                .map_err(Self::Error::from_eth_err)?
            {
                Some(block_hash) => block_hash,
                None => return Ok(None),
            };

            Ok(self
                .cache()
                .get_recovered_block(block_hash)
                .await
                .map_err(Self::Error::from_eth_err)?
                .map(|b| b.body().transaction_count()))
        }
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
            let uncles = if block_id.is_pending() {
                // Pending block can be fetched directly without need for caching
                self.provider()
                    .pending_block()
                    .map_err(Self::Error::from_eth_err)?
                    .and_then(|block| block.body().ommers().map(|o| o.to_vec()))
            } else {
                self.recovered_block(block_id)
                    .await?
                    .map(|block| block.body().ommers().map(|o| o.to_vec()).unwrap_or_default())
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn resolved_finalized_block_number(
        fast_finalized_number: u64,
        headers: &[(u64, Address)],
        verified_validator_num: i64,
        active_validator_count: Option<usize>,
    ) -> Result<u64, EthApiError> {
        if headers.is_empty() {
            return Ok(fast_finalized_number);
        }

        let threshold =
            resolved_validators_threshold(verified_validator_num, active_validator_count)?;
        if threshold == 0 {
            return Ok(fast_finalized_number);
        }

        let mut seen = HashSet::with_capacity(threshold.max(1));
        let mut probabilistic_finalized = None;
        for (number, beneficiary) in headers {
            seen.insert(*beneficiary);
            if seen.len() >= threshold {
                probabilistic_finalized = Some(*number);
                break;
            }
        }

        Ok(probabilistic_finalized.map_or(fast_finalized_number, |p| {
            std::cmp::max(fast_finalized_number, p)
        }))
    }

    #[test]
    fn resolves_validator_threshold_special_cases() {
        let validator_count = 9usize;
        assert_eq!(resolved_validators_threshold(-1, Some(validator_count)).unwrap(), 5);
        assert_eq!(resolved_validators_threshold(-2, Some(validator_count)).unwrap(), 6);
        assert_eq!(resolved_validators_threshold(-3, Some(validator_count)).unwrap(), 9);
        assert_eq!(resolved_validators_threshold(4, Some(validator_count)).unwrap(), 4);
        assert!(resolved_validators_threshold(10, Some(validator_count)).is_err());
        assert!(resolved_validators_threshold(-4, Some(validator_count)).is_err());
    }

    fn header(number: u64, beneficiary: u8) -> (u64, Address) {
        (number, Address::repeat_byte(beneficiary))
    }

    #[test]
    fn finalized_block_number_uses_ceil_half_for_negative_one() {
        let headers = vec![header(12, 0x11), header(11, 0x22), header(10, 0x33), header(9, 0x11)];

        // 3 validators => threshold ceil(3/2) = 2, satisfied at header #11.
        assert_eq!(resolved_finalized_block_number(8, &headers, -1, Some(3)).unwrap(), 11);
    }

    #[test]
    fn finalized_block_number_uses_explicit_threshold() {
        let headers = vec![
            header(12, 0x11),
            header(11, 0x11),
            header(10, 0x22),
            header(9, 0x33),
            header(8, 0x44),
        ];

        // 4 validators and explicit threshold 3, reached at block 9.
        assert_eq!(resolved_finalized_block_number(7, &headers, 3, Some(4)).unwrap(), 9);
    }

    #[test]
    fn finalized_block_number_prefers_fast_finalized_if_newer() {
        let headers = vec![header(12, 0x11), header(11, 0x22), header(10, 0x33)];
        // threshold reached at 12 but fast-finalized is higher.
        assert_eq!(resolved_finalized_block_number(20, &headers, -1, Some(3)).unwrap(), 20);
    }

    #[test]
    fn finalized_block_number_prefers_probabilistic_if_newer_than_fast_finalized() {
        let headers = vec![header(16, 0x11), header(15, 0x22), header(14, 0x33), header(13, 0x11)];

        // 3 validators, threshold ceil(3/2) = 2, reached at block 15.
        assert_eq!(resolved_finalized_block_number(10, &headers, -1, Some(3)).unwrap(), 15);
    }

    #[test]
    fn finalized_block_number_negative_two_and_three_thresholds() {
        let headers = vec![header(12, 0x11), header(11, 0x22), header(10, 0x33), header(9, 0x44)];

        // ceil(4*2/3) = 3 -> reached at block 10.
        assert_eq!(resolved_finalized_block_number(0, &headers, -2, Some(4)).unwrap(), 10);

        // 4 validators, threshold 4 -> reached at block 9.
        assert_eq!(resolved_finalized_block_number(0, &headers, -3, Some(4)).unwrap(), 9);
    }

    #[test]
    fn finalized_block_number_empty_headers_uses_fast_finalized_for_negative_values() {
        let headers = Vec::new();
        assert_eq!(resolved_finalized_block_number(42, &headers, -1, Some(4)).unwrap(), 42);
    }

    #[test]
    fn finalized_block_number_empty_headers_uses_fast_finalized_for_positive_validator_values() {
        let headers = Vec::new();
        assert_eq!(resolved_finalized_block_number(42, &headers, 5, Some(3)).unwrap(), 42);
    }

    #[test]
    fn finalized_block_number_rejects_invalid_threshold() {
        let headers = vec![header(1, 0x11)];
        assert!(resolved_finalized_block_number(1, &headers, 0, Some(1)).is_err());
        assert!(resolved_finalized_block_number(1, &headers, -4, Some(1)).is_err());
    }

    #[test]
    fn finalized_block_number_uses_active_validator_count_for_negative_values() {
        let headers = vec![header(12, 0x11), header(11, 0x22), header(10, 0x33), header(9, 0x44)];

        // With active set size 20, -2 => ceil(20*2/3) = 14. Only 4 unique signers seen, so
        // no probabilistic finalization should happen beyond fast-finalized height.
        assert_eq!(resolved_finalized_block_number(8, &headers, -2, Some(20)).unwrap(), 8);
    }

    #[test]
    fn finalized_block_number_requires_validator_count_for_negative_values() {
        let headers = vec![header(3, 0x11), header(2, 0x22), header(1, 0x33)];

        assert!(resolved_finalized_block_number(1, &headers, -1, None)
            .unwrap_err()
            .to_string()
            .contains("Unable to derive validator-count"));
    }
}
