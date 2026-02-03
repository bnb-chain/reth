//! Helper functions for `reth_rpc_eth_api::EthFilterApiServer` implementation.
//!
//! Log parsing for building filter.

use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::{eip2718::Encodable2718, BlockNumHash};
use alloy_primitives::{address, Address, TxHash};
use alloy_rpc_types_eth::{Filter, Log};
use reth_chainspec::ChainInfo;
use reth_errors::ProviderError;
use reth_primitives_traits::{BlockBody, RecoveredBlock, SignedTransaction, SignerRecoverable};
use reth_storage_api::{BlockReader, ProviderBlock};
use std::sync::Arc;
use thiserror::Error;

/// BSC system contract addresses that are used for system transactions.
/// These match the systemContracts map in geth-bsc's consensus/parlia/parlia.go.
pub const BSC_SYSTEM_CONTRACTS: &[Address] = &[
    address!("0000000000000000000000000000000000001000"), // ValidatorContract
    address!("0000000000000000000000000000000000001001"), // SlashContract
    address!("0000000000000000000000000000000000001002"), // SystemRewardContract
    address!("0000000000000000000000000000000000001003"), // LightClientContract
    address!("0000000000000000000000000000000000001004"), // TokenHubContract
    address!("0000000000000000000000000000000000001005"), // RelayerIncentivizeContract
    address!("0000000000000000000000000000000000001006"), // RelayerHubContract
    address!("0000000000000000000000000000000000001007"), // GovHubContract
    address!("0000000000000000000000000000000000002000"), // CrossChainContract
    address!("0000000000000000000000000000000000002002"), // StakeHubContract
    address!("0000000000000000000000000000000000002004"), // GovernorContract
    address!("0000000000000000000000000000000000002005"), // GovTokenContract
    address!("0000000000000000000000000000000000002006"), // TimelockContract
    address!("0000000000000000000000000000000000003000"), // TokenRecoverPortalContract
];

/// Checks if the given address is a BSC system contract.
#[inline]
pub fn is_bsc_system_contract(addr: &Address) -> bool {
    BSC_SYSTEM_CONTRACTS.contains(addr)
}

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
///
/// This is a convenience wrapper around [`append_matching_block_logs_with_tx_filter`]
/// that includes all transaction logs (no filtering).
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
    append_matching_block_logs_with_tx_filter(
        all_logs,
        provider_or_block,
        filter,
        block_num_hash,
        receipts,
        removed,
        block_timestamp,
        None, // No beneficiary - include all logs
    )
}

/// Appends all matching logs of a block's receipts with optional system transaction filtering.
///
/// If `beneficiary` is provided, transactions where the signer equals the beneficiary and
/// `max_fee_per_gas == 0` are considered system transactions and their logs are skipped.
/// This is useful for BSC and similar chains that have system transactions which should
/// not appear in `eth_getLogs` responses.
///
/// If `beneficiary` is `None`, all transaction logs are included (same as [`append_matching_block_logs`]).
pub fn append_matching_block_logs_with_tx_filter<P>(
    all_logs: &mut Vec<Log>,
    provider_or_block: ProviderOrBlock<'_, P>,
    filter: &Filter,
    block_num_hash: BlockNumHash,
    receipts: &[P::Receipt],
    removed: bool,
    block_timestamp: u64,
    beneficiary: Option<Address>,
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

    // Iterate over receipts and append matching logs.
    for (receipt_idx, receipt) in receipts.iter().enumerate() {
        // Check if this transaction should be skipped (system transaction filtering)
        let should_skip = if let Some(beneficiary) = beneficiary {
            match &provider_or_block {
                ProviderOrBlock::Block(block) => {
                    if let Some(tx) = block.body().transactions().get(receipt_idx) {
                        // System transaction: to is system contract && gas_price == 0 && signer == beneficiary
                        tx.to().is_some_and(|to| is_bsc_system_contract(&to))
                            && tx.max_fee_per_gas() == 0
                            && block
                                .senders()
                                .get(receipt_idx)
                                .is_some_and(|signer| *signer == beneficiary)
                    } else {
                        false
                    }
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
                    let transaction_id = first_tx_num + receipt_idx as u64;
                    if let Some(tx) = provider.transaction_by_id(transaction_id)? {
                        // System transaction: to is system contract && gas_price == 0 && signer == beneficiary
                        tx.to().is_some_and(|to| is_bsc_system_contract(&to))
                            && tx.max_fee_per_gas() == 0
                            && tx.recover_signer().is_ok_and(|signer| signer == beneficiary)
                    } else {
                        false
                    }
                }
            }
        } else {
            false
        };

        // Skip logs from system transactions
        if should_skip {
            // Still need to count logs for correct log_index
            log_index += receipt.logs().len() as u64;
            continue;
        }

        // The transaction hash of the current receipt.
        let mut transaction_hash = None;

        for log in receipt.logs() {
            if filter.matches(log) {
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
            log_index += 1;
        }
    }
    Ok(())
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

    #[test]
    fn test_is_bsc_system_contract() {
        use alloy_primitives::address;

        // All BSC system contracts should be recognized
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001000"
        ))); // Validator
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001001"
        ))); // Slash
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001002"
        ))); // SystemReward
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001003"
        ))); // LightClient
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001004"
        ))); // TokenHub
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001005"
        ))); // RelayerIncentivize
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001006"
        ))); // RelayerHub
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001007"
        ))); // GovHub
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002000"
        ))); // CrossChain
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002002"
        ))); // StakeHub
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002004"
        ))); // Governor
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002005"
        ))); // GovToken
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002006"
        ))); // Timelock
        assert!(is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000003000"
        ))); // TokenRecoverPortal

        // Non-system contracts should NOT be recognized
        assert!(!is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000001008"
        ))); // TokenManager - not in systemContracts
        assert!(!is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002001"
        ))); // Staking - not in systemContracts
        assert!(!is_bsc_system_contract(&address!(
            "0000000000000000000000000000000000002003"
        ))); // StakeCredit - not in systemContracts
        assert!(!is_bsc_system_contract(&Address::ZERO));
        assert!(!is_bsc_system_contract(&address!(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        )));
    }

    #[test]
    fn test_bsc_system_contracts_count() {
        // Ensure we have exactly 14 system contracts (matching geth-bsc)
        assert_eq!(BSC_SYSTEM_CONTRACTS.len(), 14);
    }

    /// Helper function that mirrors the system transaction detection logic used in
    /// `append_matching_block_logs_with_tx_filter`. This allows us to test the logic
    /// independently without needing full block/receipt structures.
    fn is_system_transaction(
        to: Option<Address>,
        max_fee_per_gas: u128,
        signer: Address,
        beneficiary: Address,
    ) -> bool {
        // System transaction conditions (all must be true):
        // 1. Transaction target is a BSC system contract
        // 2. max_fee_per_gas (gas price) is 0
        // 3. Transaction signer equals block beneficiary (coinbase)
        to.is_some_and(|addr| is_bsc_system_contract(&addr))
            && max_fee_per_gas == 0
            && signer == beneficiary
    }

    #[test]
    fn test_system_transaction_detection() {
        use alloy_primitives::address;

        let validator_contract = address!("0000000000000000000000000000000000001000");
        let regular_contract = address!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        let coinbase = address!("1234567890123456789012345678901234567890");
        let user_address = address!("abcdefabcdefabcdefabcdefabcdefabcdefabcd");

        // Case 1: True system transaction - all conditions met
        // to=system_contract, gas_price=0, signer=coinbase
        assert!(
            is_system_transaction(Some(validator_contract), 0, coinbase, coinbase),
            "Should be detected as system tx: system contract, gas_price=0, signer=coinbase"
        );

        // Case 2: NOT system tx - target is NOT a system contract
        assert!(
            !is_system_transaction(Some(regular_contract), 0, coinbase, coinbase),
            "Should NOT be system tx: target is not a system contract"
        );

        // Case 3: NOT system tx - gas_price is NOT 0
        assert!(
            !is_system_transaction(Some(validator_contract), 1, coinbase, coinbase),
            "Should NOT be system tx: gas_price > 0"
        );

        // Case 4: NOT system tx - signer is NOT coinbase
        assert!(
            !is_system_transaction(Some(validator_contract), 0, user_address, coinbase),
            "Should NOT be system tx: signer != coinbase"
        );

        // Case 5: NOT system tx - contract creation (to=None)
        assert!(
            !is_system_transaction(None, 0, coinbase, coinbase),
            "Should NOT be system tx: contract creation (to=None)"
        );

        // Case 6: Regular user transaction to system contract with gas
        // (e.g., user calling staking contract)
        assert!(
            !is_system_transaction(Some(validator_contract), 1_000_000_000, user_address, coinbase),
            "Should NOT be system tx: user tx to system contract with gas"
        );
    }

    #[test]
    fn test_system_transaction_all_system_contracts() {
        use alloy_primitives::address;

        let coinbase = address!("1234567890123456789012345678901234567890");

        // Verify ALL 14 system contracts are filtered when conditions are met
        for system_contract in BSC_SYSTEM_CONTRACTS {
            assert!(
                is_system_transaction(Some(*system_contract), 0, coinbase, coinbase),
                "System contract {:?} should be detected as system tx",
                system_contract
            );
        }
    }

    #[test]
    fn test_non_system_contracts_never_filtered() {
        use alloy_primitives::address;

        let coinbase = address!("1234567890123456789012345678901234567890");

        // These contracts are NOT in the BSC systemContracts map (per geth-bsc)
        // Even with gas_price=0 and signer=coinbase, they should NOT be filtered
        let non_system_contracts = [
            address!("0000000000000000000000000000000000001008"), // TokenManager
            address!("0000000000000000000000000000000000002001"), // Staking
            address!("0000000000000000000000000000000000002003"), // StakeCredit
            address!("0000000000000000000000000000000000000000"), // Zero address
            address!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"), // Random
        ];

        for contract in non_system_contracts {
            assert!(
                !is_system_transaction(Some(contract), 0, coinbase, coinbase),
                "Contract {:?} is NOT a system contract, should NOT be filtered",
                contract
            );
        }
    }

    #[test]
    fn test_user_txs_to_system_contracts_not_filtered() {
        use alloy_primitives::address;

        let validator_contract = address!("0000000000000000000000000000000000001000");
        let stake_hub = address!("0000000000000000000000000000000000002002");
        let coinbase = address!("1234567890123456789012345678901234567890");
        let user = address!("abcdefabcdefabcdefabcdefabcdefabcdefabcd");

        // User transactions to system contracts (e.g., staking, voting) should NOT be filtered
        // These have gas_price > 0 and signer != coinbase
        assert!(
            !is_system_transaction(Some(validator_contract), 5_000_000_000, user, coinbase),
            "User tx to ValidatorContract should NOT be filtered"
        );

        assert!(
            !is_system_transaction(Some(stake_hub), 5_000_000_000, user, coinbase),
            "User tx to StakeHub should NOT be filtered"
        );

        // Edge case: User tx with gas_price=0 but signer != coinbase
        assert!(
            !is_system_transaction(Some(validator_contract), 0, user, coinbase),
            "User tx with gas_price=0 but signer != coinbase should NOT be filtered"
        );

        // Edge case: Coinbase tx to system contract with gas_price > 0
        assert!(
            !is_system_transaction(Some(validator_contract), 1, coinbase, coinbase),
            "Coinbase tx with gas_price > 0 should NOT be filtered"
        );
    }
}
