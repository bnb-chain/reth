use alloy_primitives::{BlockHash, BlockNumber};
use reth_primitives::BlobSidecars;
use reth_storage_errors::provider::ProviderResult;

/// Client trait for fetching [BlobSidecars] for blocks.
#[auto_impl::auto_impl(&, Arc)]
pub trait SidecarsProvider: Send + Sync {
    /// Get sidecars by block hash
    ///
    /// Returns `None` if the sidecars is not found.
    fn sidecars(&self, block_hash: &BlockHash) -> ProviderResult<Option<BlobSidecars>>;

    /// Get sidecar by block number.
    ///
    /// Returns `None` if the sidecars is not found.
    fn sidecars_by_number(&self, num: BlockNumber) -> ProviderResult<Option<BlobSidecars>>;
}
