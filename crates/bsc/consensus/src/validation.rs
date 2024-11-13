use alloy_eips::eip4844::{DATA_GAS_PER_BLOB, MAX_DATA_GAS_PER_BLOCK};
use alloy_primitives::{Bloom, B256};
use reth_chainspec::{EthChainSpec, EthereumHardfork, EthereumHardforks};
use reth_consensus::ConsensusError;
use reth_primitives::{
    gas_spent_by_transactions, BlockWithSenders, GotExpected, Header, Receipt, SealedHeader,
};

/// Validate the 4844 header of BSC block.
/// Compared to Ethereum, BSC block doesn't have `parent_beacon_block_root`.
pub fn validate_4844_header_of_bsc(header: &SealedHeader) -> Result<(), ConsensusError> {
    let blob_gas_used = header.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
    let excess_blob_gas = header.excess_blob_gas.ok_or(ConsensusError::ExcessBlobGasMissing)?;

    if blob_gas_used > MAX_DATA_GAS_PER_BLOCK {
        return Err(ConsensusError::BlobGasUsedExceedsMaxBlobGasPerBlock {
            blob_gas_used,
            max_blob_gas_per_block: MAX_DATA_GAS_PER_BLOCK,
        })
    }

    if blob_gas_used % DATA_GAS_PER_BLOB != 0 {
        return Err(ConsensusError::BlobGasUsedNotMultipleOfBlobGasPerBlob {
            blob_gas_used,
            blob_gas_per_blob: DATA_GAS_PER_BLOB,
        })
    }

    // `excess_blob_gas` must also be a multiple of `DATA_GAS_PER_BLOB`. This will be checked later
    // (via `calculate_excess_blob_gas`), but it doesn't hurt to catch the problem sooner.
    if excess_blob_gas % DATA_GAS_PER_BLOB != 0 {
        return Err(ConsensusError::ExcessBlobGasNotMultipleOfBlobGasPerBlob {
            excess_blob_gas,
            blob_gas_per_blob: DATA_GAS_PER_BLOB,
        })
    }

    Ok(())
}

/// Validates the base fee against the parent and EIP-1559 rules of BSC block.
/// Compared to Ethereum, BSC base fee is always zero.
#[inline]
pub fn validate_against_parent_eip1559_base_fee_of_bsc<
    ChainSpec: EthChainSpec + EthereumHardforks,
>(
    header: &Header,
    _parent: &Header,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    if chain_spec.fork(EthereumHardfork::London).active_at_block(header.number) {
        let base_fee = header.base_fee_per_gas.ok_or(ConsensusError::BaseFeeMissing)?;
        let expected_base_fee = reth_primitives::constants::EIP1559_INITIAL_BASE_FEE;

        if expected_base_fee != base_fee {
            return Err(ConsensusError::BaseFeeDiff(GotExpected {
                expected: expected_base_fee,
                got: base_fee,
            }))
        }
    }

    Ok(())
}

/// Validate a block with regard to execution results:
///
/// - Compares the receipts root in the block header to the block body
/// - Compares the gas used in the block header to the actual gas usage after execution
///
/// Compared to Ethereum, BSC doesn't have to check `requests_root`.
pub fn validate_block_post_execution_of_bsc<ChainSpec: EthereumHardforks>(
    block: &BlockWithSenders,
    chain_spec: &ChainSpec,
    receipts: &[Receipt],
) -> Result<(), ConsensusError> {
    // Check if gas used matches the value set in header.
    let cumulative_gas_used =
        receipts.last().map(|receipt| receipt.cumulative_gas_used).unwrap_or(0);
    if block.gas_used != cumulative_gas_used {
        return Err(ConsensusError::BlockGasUsed {
            gas: GotExpected { got: cumulative_gas_used, expected: block.gas_used },
            gas_spent_by_tx: gas_spent_by_transactions(receipts),
        })
    }

    // Before Byzantium, receipts contained state root that would mean that expensive
    // operation as hashing that is required for state root got calculated in every
    // transaction This was replaced with is_success flag.
    // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
    if chain_spec.is_byzantium_active_at_block(block.header.number) {
        verify_receipts(block.header.receipts_root, block.header.logs_bloom, receipts)?;
    }

    Ok(())
}

/// Calculate the receipts root, and compare it against the expected receipts root and logs
/// bloom.
fn verify_receipts(
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
    receipts: &[Receipt],
) -> Result<(), ConsensusError> {
    // Calculate receipts root.
    let receipts_with_bloom = receipts.iter().map(Receipt::with_bloom_ref).collect::<Vec<_>>();
    let receipts_root = reth_primitives::proofs::calculate_receipt_root_ref(&receipts_with_bloom);

    // Calculate header logs bloom.
    let logs_bloom = receipts_with_bloom.iter().fold(Bloom::ZERO, |bloom, r| bloom | r.bloom);

    compare_receipts_root_and_logs_bloom(
        receipts_root,
        logs_bloom,
        expected_receipts_root,
        expected_logs_bloom,
    )?;

    Ok(())
}

/// Compare the calculated receipts root with the expected receipts root, also compare
/// the calculated logs bloom with the expected logs bloom.
fn compare_receipts_root_and_logs_bloom(
    calculated_receipts_root: B256,
    calculated_logs_bloom: Bloom,
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
) -> Result<(), ConsensusError> {
    if calculated_receipts_root != expected_receipts_root {
        return Err(ConsensusError::BodyReceiptRootDiff(
            GotExpected { got: calculated_receipts_root, expected: expected_receipts_root }.into(),
        ))
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(ConsensusError::BodyBloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        ))
    }

    Ok(())
}
