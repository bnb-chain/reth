use reth_primitives::{BlobSidecars, BlockHashOrNumber};
use reth_storage_errors::provider::ProviderResult;

/// Client trait for fetching [BlobSidecars] for blocks.
#[auto_impl::auto_impl(&, Arc)]
pub trait SidecarsProvider: Send + Sync {
    /// Get sidecars by block id.
    fn sidecars_by_block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<BlobSidecars>>;
}
