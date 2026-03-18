//! Helper functions for `reth_rpc_eth_api::EthFilterApiServer` implementation.
//!
//! Log parsing for building filter.

use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_eips::{eip2718::Encodable2718, BlockNumHash};
use alloy_primitives::{Address, TxHash};
use alloy_rpc_types_eth::{Filter, Log};
use reth_chainspec::ChainInfo;
use reth_errors::ProviderError;
use reth_primitives_traits::{BlockBody, RecoveredBlock, SignedTransaction, SignerRecoverable};
use reth_storage_api::{BlockReader, ProviderBlock};
use std::sync::Arc;
use thiserror::Error;

/// Returns all matching of a block's receipts when the transaction hashes are known.
pub fn matching_block_logs_with_tx_hashes<'a, I, R>(
    filter: &Filter,
    block_num_hash: BlockNumHash,
    block_timestamp: u64,
    tx_hashes_and_receipts: I,
    removed: bool,
) -> Vec<Log>
where
    I: IntoIterator<Item = (TxHash, &'a R)>,
    R: TxReceipt<Log = alloy_primitives::Log> + 'a,
{
    if !filter.matches_block(&block_num_hash) {
        return vec![];
    }

    let mut all_logs = Vec::new();
    // Tracks the index of a log in the entire block.
    let mut log_index: u64 = 0;

    // Iterate over transaction hashes and receipts and append matching logs.
    for (receipt_idx, (tx_hash, receipt)) in tx_hashes_and_receipts.into_iter().enumerate() {
        for log in receipt.logs() {
            if filter.matches(log) {
                let log = Log {
                    inner: log.clone(),
                    block_hash: Some(block_num_hash.hash),
                    block_number: Some(block_num_hash.number),
                    transaction_hash: Some(tx_hash),
                    // The transaction and receipt index is always the same.
                    transaction_index: Some(receipt_idx as u64),
                    log_index: Some(log_index),
                    removed,
                    block_timestamp: Some(block_timestamp),
                };
                all_logs.push(log);
            }
            log_index += 1;
        }
    }
    all_logs
}

/// Trait for filtering receipts during log retrieval in RPC responses.
///
/// Implementations can exclude certain receipts from `eth_getLogs`, `eth_getFilterChanges`,
/// and `eth_subscribe("logs")` RPC responses. For example, BSC uses this to filter out
/// system transaction logs that should not be visible to RPC consumers.
///
/// The filter is called for each receipt during log iteration. Returning `false`
/// causes all logs from that receipt to be excluded from the result, while still
/// correctly tracking `log_index` for subsequent receipts.
///
/// This trait is object-safe so it can be stored as `Arc<dyn ReceiptFilter>`.
/// Transaction data is extracted by the caller and passed directly to avoid
/// generic type parameters.
pub trait ReceiptFilter: Send + Sync + 'static {
    /// Returns `true` if logs from the receipt at `receipt_idx` should be
    /// included in the RPC response.
    ///
    /// # Arguments
    /// * `block_num_hash` - The block number and hash
    /// * `receipt_idx` - Index of the receipt within the block
    /// * `beneficiary` - The block's beneficiary (coinbase/validator) address
    /// * `tx_signer` - The recovered signer of the transaction
    /// * `tx_to` - The recipient address (`None` for contract creation)
    /// * `tx_max_fee_per_gas` - The transaction's max fee per gas
    fn should_include(
        &self,
        block_num_hash: BlockNumHash,
        receipt_idx: usize,
        beneficiary: Address,
        tx_signer: Address,
        tx_to: Option<Address>,
        tx_max_fee_per_gas: u128,
    ) -> bool;
}

/// Default no-op implementation that includes all receipts (no filtering).
#[derive(Debug, Clone, Default)]
pub struct NoopReceiptFilter;

impl ReceiptFilter for NoopReceiptFilter {
    #[inline]
    fn should_include(
        &self,
        _block_num_hash: BlockNumHash,
        _receipt_idx: usize,
        _beneficiary: Address,
        _tx_signer: Address,
        _tx_to: Option<Address>,
        _tx_max_fee_per_gas: u128,
    ) -> bool {
        true
    }
}

/// Helper enum to fetch a transaction either from a block or from the provider.
#[derive(Debug)]
pub enum ProviderOrBlock<'a, P: BlockReader> {
    /// Provider
    Provider(&'a P),
    /// [`RecoveredBlock`]
    Block(Arc<RecoveredBlock<ProviderBlock<P>>>),
}

/// Appends all matching logs of a block's receipts.
/// If the log matches, look up the corresponding transaction hash.
pub fn append_matching_block_logs<P>(
    all_logs: &mut Vec<Log>,
    provider_or_block: ProviderOrBlock<'_, P>,
    filter: &Filter,
    block_num_hash: BlockNumHash,
    receipts: &[P::Receipt],
    removed: bool,
    block_timestamp: u64,
) -> Result<(), ProviderError>
where
    P: BlockReader<Transaction: SignedTransaction>,
{
    append_matching_block_logs_with_receipt_filter(
        all_logs,
        provider_or_block,
        filter,
        block_num_hash,
        receipts,
        removed,
        block_timestamp,
        None,
    )
}

/// Appends all matching logs of a block's receipts, with an optional [`ReceiptFilter`]
/// to exclude certain receipts from the result.
///
/// This is the same as [`append_matching_block_logs`] but accepts an optional
/// [`ReceiptFilter`] that can exclude certain receipts. This is used by BSC to filter
/// out system transaction logs from `eth_getLogs` responses.
///
/// **Important**: Even when a receipt is filtered out, its logs are still counted
/// for `log_index` to maintain correct global log indexing within the block.
///
/// When `receipt_filter` is `None`, no filtering overhead is incurred — the function
/// behaves identically to [`append_matching_block_logs`].
pub fn append_matching_block_logs_with_receipt_filter<P>(
    all_logs: &mut Vec<Log>,
    provider_or_block: ProviderOrBlock<'_, P>,
    filter: &Filter,
    block_num_hash: BlockNumHash,
    receipts: &[P::Receipt],
    removed: bool,
    block_timestamp: u64,
    receipt_filter: Option<&dyn ReceiptFilter>,
) -> Result<(), ProviderError>
where
    P: BlockReader<Transaction: SignedTransaction>,
{
    // Tracks the index of a log in the entire block.
    let mut log_index: u64 = 0;

    // Lazy loaded number of the first transaction in the block.
    // This is useful for blocks with multiple matching logs because it
    // prevents re-querying the block body indices.
    let mut loaded_first_tx_num = None;

    // Lazy loaded beneficiary - only fetched when receipt_filter is present.
    let beneficiary = if receipt_filter.is_some() {
        Some(match &provider_or_block {
            ProviderOrBlock::Block(block) => block.header().beneficiary(),
            ProviderOrBlock::Provider(provider) => {
                let header = provider
                    .header_by_hash_or_number(block_num_hash.hash.into())?
                    .ok_or_else(|| ProviderError::HeaderNotFound(block_num_hash.hash.into()))?;
                header.beneficiary()
            }
        })
    } else {
        None
    };

    // Iterate over receipts and append matching logs.
    for (receipt_idx, receipt) in receipts.iter().enumerate() {
        // Check if this receipt should be included in the response.
        // Only extract transaction data when a receipt filter is present.
        let include = if let Some(rf) = receipt_filter {
            let beneficiary = beneficiary.expect("beneficiary set when receipt_filter is Some");

            let (tx_signer, tx_to, tx_max_fee) = match &provider_or_block {
                ProviderOrBlock::Block(block) => {
                    let signer = block
                        .senders()
                        .get(receipt_idx)
                        .copied()
                        .unwrap_or(Address::ZERO);
                    if let Some(tx) = block.body().transactions().get(receipt_idx) {
                        (signer, tx.to(), tx.max_fee_per_gas())
                    } else {
                        (signer, None, 0)
                    }
                }
                ProviderOrBlock::Provider(provider) => {
                    let first_tx_num = match loaded_first_tx_num {
                        Some(num) => num,
                        None => {
                            let indices = provider
                                .block_body_indices(block_num_hash.number)?
                                .ok_or(ProviderError::BlockBodyIndicesNotFound(
                                    block_num_hash.number,
                                ))?;
                            loaded_first_tx_num = Some(indices.first_tx_num);
                            indices.first_tx_num
                        }
                    };
                    let transaction_id = first_tx_num + receipt_idx as u64;
                    let tx =
                        provider.transaction_by_id(transaction_id)?.ok_or_else(|| {
                            ProviderError::TransactionNotFound(transaction_id.into())
                        })?;
                    let signer = tx
                        .recover_signer()
                        .map_err(|_| ProviderError::SenderRecoveryError)?;
                    (signer, tx.to(), tx.max_fee_per_gas())
                }
            };

            rf.should_include(
                block_num_hash,
                receipt_idx,
                beneficiary,
                tx_signer,
                tx_to,
                tx_max_fee,
            )
        } else {
            true
        };

        // The transaction hash of the current receipt.
        let mut transaction_hash = None;

        for log in receipt.logs() {
            if include && filter.matches(log) {
                // if this is the first match in the receipt's logs, look up the transaction hash
                if transaction_hash.is_none() {
                    transaction_hash = match &provider_or_block {
                        ProviderOrBlock::Block(block) => {
                            block.body().transactions().get(receipt_idx).map(|t| t.trie_hash())
                        }
                        ProviderOrBlock::Provider(provider) => {
                            let first_tx_num = match loaded_first_tx_num {
                                Some(num) => num,
                                None => {
                                    let block_body_indices = provider
                                        .block_body_indices(block_num_hash.number)?
                                        .ok_or(ProviderError::BlockBodyIndicesNotFound(
                                            block_num_hash.number,
                                        ))?;
                                    loaded_first_tx_num = Some(block_body_indices.first_tx_num);
                                    block_body_indices.first_tx_num
                                }
                            };

                            // This is safe because Transactions and Receipts have the same
                            // keys.
                            let transaction_id = first_tx_num + receipt_idx as u64;
                            let transaction =
                                provider.transaction_by_id(transaction_id)?.ok_or_else(|| {
                                    ProviderError::TransactionNotFound(transaction_id.into())
                                })?;

                            Some(transaction.trie_hash())
                        }
                    };
                }

                let log = Log {
                    inner: log.clone(),
                    block_hash: Some(block_num_hash.hash),
                    block_number: Some(block_num_hash.number),
                    transaction_hash,
                    // The transaction and receipt index is always the same.
                    transaction_index: Some(receipt_idx as u64),
                    log_index: Some(log_index),
                    removed,
                    block_timestamp: Some(block_timestamp),
                };
                all_logs.push(log);
            }
            // Always increment log_index, even for filtered receipts,
            // to maintain correct global log indexing within the block.
            log_index += 1;
        }
    }
    Ok(())
}

/// Returns all matching logs of a block's receipts when the transaction hashes are known,
/// with an optional receipt filter to skip certain receipts.
///
/// This is used by `eth_subscribe("logs")` PubSub to filter logs from the canonical state
/// stream. The `should_skip` closure is called for each receipt index and should return
/// `true` to exclude that receipt's logs from the result.
///
/// **Important**: Even when a receipt is skipped, its logs are still counted for `log_index`
/// to maintain correct global log indexing within the block.
pub fn matching_block_logs_with_tx_hashes_filtered<'a, I, R, F>(
    filter: &Filter,
    block_num_hash: BlockNumHash,
    block_timestamp: u64,
    tx_hashes_and_receipts: I,
    removed: bool,
    should_skip: F,
) -> Vec<Log>
where
    I: IntoIterator<Item = (TxHash, &'a R)>,
    R: TxReceipt<Log = alloy_primitives::Log> + 'a,
    F: Fn(usize) -> bool,
{
    if !filter.matches_block(&block_num_hash) {
        return vec![];
    }

    let mut all_logs = Vec::new();
    // Tracks the index of a log in the entire block.
    let mut log_index: u64 = 0;

    // Iterate over transaction hashes and receipts and append matching logs.
    for (receipt_idx, (tx_hash, receipt)) in tx_hashes_and_receipts.into_iter().enumerate() {
        let skip = should_skip(receipt_idx);

        for log in receipt.logs() {
            if !skip && filter.matches(log) {
                let log = Log {
                    inner: log.clone(),
                    block_hash: Some(block_num_hash.hash),
                    block_number: Some(block_num_hash.number),
                    transaction_hash: Some(tx_hash),
                    // The transaction and receipt index is always the same.
                    transaction_index: Some(receipt_idx as u64),
                    log_index: Some(log_index),
                    removed,
                    block_timestamp: Some(block_timestamp),
                };
                all_logs.push(log);
            }
            log_index += 1;
        }
    }
    all_logs
}

/// Computes the block range based on the filter range and current block numbers.
///
/// Returns an error for invalid ranges rather than silently clamping values.
pub fn get_filter_block_range(
    from_block: Option<u64>,
    to_block: Option<u64>,
    start_block: u64,
    info: ChainInfo,
) -> Result<(u64, u64), FilterBlockRangeError> {
    let from_block_number = from_block.unwrap_or(start_block);
    let to_block_number = to_block.unwrap_or(info.best_number);

    // from > to is an invalid range
    if from_block_number > to_block_number {
        return Err(FilterBlockRangeError::InvalidBlockRange);
    }

    // we cannot query blocks that don't exist yet
    if to_block_number > info.best_number {
        return Err(FilterBlockRangeError::BlockRangeExceedsHead);
    }

    Ok((from_block_number, to_block_number))
}

/// Errors for filter block range validation.
///
/// See also <https://github.com/ethereum/go-ethereum/blob/master/eth/filters/filter.go#L224-L230>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FilterBlockRangeError {
    /// `from_block > to_block`
    #[error("invalid block range params")]
    InvalidBlockRange,
    /// Block range extends beyond current head
    #[error("block range extends beyond current head block")]
    BlockRangeExceedsHead,
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_eth::Filter;

    use super::*;

    #[test]
    fn test_log_range_from_and_to() {
        let from = 14000000u64;
        let to = 14000100u64;
        let info = ChainInfo { best_number: 15000000, ..Default::default() };
        let range = get_filter_block_range(Some(from), Some(to), info.best_number, info).unwrap();
        assert_eq!(range, (from, to));
    }

    #[test]
    fn test_log_range_from() {
        let from = 14000000u64;
        let info = ChainInfo { best_number: 15000000, ..Default::default() };
        let range = get_filter_block_range(Some(from), None, 0, info).unwrap();
        assert_eq!(range, (from, info.best_number));
    }

    #[test]
    fn test_log_range_to() {
        let to = 14000000u64;
        let start_block = 0u64;
        let info = ChainInfo { best_number: 15000000, ..Default::default() };
        let range = get_filter_block_range(None, Some(to), start_block, info).unwrap();
        assert_eq!(range, (start_block, to));
    }

    #[test]
    fn test_log_range_higher_error() {
        // Range extends beyond head -> should error instead of clamping
        let from = 15000001u64;
        let to = 15000002u64;
        let info = ChainInfo { best_number: 15000000, ..Default::default() };
        let err = get_filter_block_range(Some(from), Some(to), info.best_number, info).unwrap_err();
        assert_eq!(err, FilterBlockRangeError::BlockRangeExceedsHead);
    }

    #[test]
    fn test_log_range_to_below_start_error() {
        // to_block < start_block, default from -> invalid range
        let to = 14000000u64;
        let info = ChainInfo { best_number: 15000000, ..Default::default() };
        let err = get_filter_block_range(None, Some(to), info.best_number, info).unwrap_err();
        assert_eq!(err, FilterBlockRangeError::InvalidBlockRange);
    }

    #[test]
    fn test_log_range_empty() {
        let info = ChainInfo { best_number: 15000000, ..Default::default() };
        let range = get_filter_block_range(None, None, info.best_number, info).unwrap();

        // no range given -> head
        assert_eq!(range, (info.best_number, info.best_number));
    }

    #[test]
    fn test_invalid_block_range_error() {
        let from = 100;
        let to = 50;
        let info = ChainInfo { best_number: 150, ..Default::default() };
        let err = get_filter_block_range(Some(from), Some(to), 0, info).unwrap_err();
        assert_eq!(err, FilterBlockRangeError::InvalidBlockRange);
    }

    #[test]
    fn test_block_range_exceeds_head_error() {
        let from = 100;
        let to = 200;
        let info = ChainInfo { best_number: 150, ..Default::default() };
        let err = get_filter_block_range(Some(from), Some(to), 0, info).unwrap_err();
        assert_eq!(err, FilterBlockRangeError::BlockRangeExceedsHead);
    }

    #[test]
    fn parse_log_from_only() {
        let s = r#"{"fromBlock":"0xf47a42","address":["0x7de93682b9b5d80d45cd371f7a14f74d49b0914c","0x0f00392fcb466c0e4e4310d81b941e07b4d5a079","0xebf67ab8cff336d3f609127e8bbf8bd6dd93cd81"],"topics":["0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f"]}"#;
        let filter: Filter = serde_json::from_str(s).unwrap();

        assert_eq!(filter.get_from_block(), Some(16022082));
        assert!(filter.get_to_block().is_none());

        let best_number = 17229427;
        let info = ChainInfo { best_number, ..Default::default() };

        let (from_block, to_block) = filter.block_option.as_range();

        let start_block = info.best_number;

        let (from_block_number, to_block_number) = get_filter_block_range(
            from_block.and_then(alloy_rpc_types_eth::BlockNumberOrTag::as_number),
            to_block.and_then(alloy_rpc_types_eth::BlockNumberOrTag::as_number),
            start_block,
            info,
        )
        .unwrap();
        assert_eq!(from_block_number, 16022082);
        assert_eq!(to_block_number, best_number);
    }
}
