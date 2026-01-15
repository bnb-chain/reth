//! Custom RPC header types with additional fields

use alloy_network::primitives::HeaderResponse;
use alloy_primitives::{BlockHash, B256, U256};

/// Custom RPC header that extends the standard header with additional fields
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CustomRpcHeader<H = alloy_consensus::Header> {
    /// Hash of the block
    pub hash: BlockHash,
    /// Inner consensus header.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: H,
    /// Total difficulty
    ///
    /// Note: This field is now effectively deprecated: <https://github.com/ethereum/execution-apis/pull/570>
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub total_difficulty: Option<U256>,
    /// Integer the size of this block in bytes.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub size: Option<U256>,
    /// Millisecond timestamp - custom field for BNB Chain
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub milli_timestamp: Option<U256>,
}

impl<H> CustomRpcHeader<H> {
    /// Create a new custom header from consensus header components
    pub const fn new(
        hash: BlockHash,
        inner: H,
        total_difficulty: Option<U256>,
        size: Option<U256>,
        milli_timestamp: Option<U256>,
    ) -> Self {
        Self { hash, inner, total_difficulty, size, milli_timestamp }
    }

    /// Create a custom header from a consensus header
    pub fn from_consensus(
        header: alloy_consensus::Header,
        total_difficulty: Option<U256>,
        size: Option<U256>,
    ) -> CustomRpcHeader<alloy_consensus::Header> {
        let hash = header.hash_slow();
        let milli_timestamp = Some(U256::from(calculate_millisecond_timestamp(&header)));

        CustomRpcHeader { hash, inner: header, total_difficulty, size, milli_timestamp }
    }

    /// Create a custom header from any block header
    pub fn from_header<T: reth_primitives_traits::BlockHeader>(
        header: T,
        hash: BlockHash,
        total_difficulty: Option<U256>,
        size: Option<U256>,
    ) -> CustomRpcHeader<T> {
        let milli_timestamp = Some(U256::from(calculate_millisecond_timestamp(&header)));

        CustomRpcHeader { hash, inner: header, total_difficulty, size, milli_timestamp }
    }
}

impl<H> HeaderResponse for CustomRpcHeader<H>
where
    H: alloy_consensus::BlockHeader,
{
    fn hash(&self) -> BlockHash {
        self.hash
    }
}

impl<H> alloy_consensus::BlockHeader for CustomRpcHeader<H>
where
    H: alloy_consensus::BlockHeader,
{
    fn parent_hash(&self) -> alloy_primitives::B256 {
        self.inner.parent_hash()
    }

    fn ommers_hash(&self) -> alloy_primitives::B256 {
        self.inner.ommers_hash()
    }

    fn beneficiary(&self) -> alloy_primitives::Address {
        self.inner.beneficiary()
    }

    fn state_root(&self) -> alloy_primitives::B256 {
        self.inner.state_root()
    }

    fn transactions_root(&self) -> alloy_primitives::B256 {
        self.inner.transactions_root()
    }

    fn receipts_root(&self) -> alloy_primitives::B256 {
        self.inner.receipts_root()
    }

    fn withdrawals_root(&self) -> Option<alloy_primitives::B256> {
        self.inner.withdrawals_root()
    }

    fn logs_bloom(&self) -> alloy_primitives::Bloom {
        self.inner.logs_bloom()
    }

    fn difficulty(&self) -> alloy_primitives::U256 {
        self.inner.difficulty()
    }

    fn number(&self) -> u64 {
        self.inner.number()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn mix_hash(&self) -> Option<alloy_primitives::B256> {
        self.inner.mix_hash()
    }

    fn nonce(&self) -> Option<alloy_primitives::FixedBytes<8>> {
        self.inner.nonce()
    }

    fn base_fee_per_gas(&self) -> Option<u64> {
        self.inner.base_fee_per_gas()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn excess_blob_gas(&self) -> Option<u64> {
        self.inner.excess_blob_gas()
    }

    fn parent_beacon_block_root(&self) -> Option<alloy_primitives::B256> {
        self.inner.parent_beacon_block_root()
    }

    fn requests_hash(&self) -> Option<alloy_primitives::B256> {
        self.inner.requests_hash()
    }

    fn extra_data(&self) -> &alloy_primitives::Bytes {
        self.inner.extra_data()
    }
}

// RpcObject is automatically implemented via blanket impl for types that implement Serialize +
// Deserialize

/// Type alias for the standard Ethereum custom header
pub type EthereumCustomHeader = CustomRpcHeader<alloy_consensus::Header>;

/// Custom header converter that creates `CustomRpcHeader` instances
#[derive(Debug, Clone)]
pub struct CustomHeaderConverter;

impl<H> crate::transaction::HeaderConverter<H, CustomRpcHeader<H>> for CustomHeaderConverter
where
    H: reth_primitives_traits::BlockHeader + Clone,
{
    fn convert_header(
        &self,
        header: reth_primitives_traits::SealedHeader<H>,
        block_size: usize,
        td: Option<alloy_primitives::U256>,
    ) -> CustomRpcHeader<H> {
        let header_hash = header.hash();
        let consensus_header = header.into_header();
        let milli_timestamp = Some(U256::from(calculate_millisecond_timestamp(&consensus_header)));

        CustomRpcHeader {
            hash: header_hash,
            inner: consensus_header,
            total_difficulty: td,
            size: Some(alloy_primitives::U256::from(block_size)),
            milli_timestamp,
        }
    }
}

/// calculate millisecond timestamp from header `mix_hash` for any `BlockHeader` type  
pub fn calculate_millisecond_timestamp<T: reth_primitives_traits::BlockHeader>(header: &T) -> u64 {
    let seconds = header.timestamp();
    let mix_hash = header.mix_hash();

    let ms_part = if let Some(mix_hash) = mix_hash {
        if mix_hash == B256::ZERO {
            0
        } else {
            let bytes = mix_hash.as_slice();
            // Convert last 8 bytes to u64 (big-endian), equivalent to Go's
            // uint256.SetBytes32().Uint64()
            let mut result = 0u64;
            for &byte in bytes.iter().skip(24).take(8) {
                result = (result << 8) | u64::from(byte);
            }
            result
        }
    } else {
        0
    };

    seconds * 1000 + ms_part
}
