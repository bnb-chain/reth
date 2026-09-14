use alloc::vec::Vec;
use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{BlockHash, BlockNumber};
use core::ops::RangeBounds;
use reth_primitives_traits::{BlockHeader, SealedHeader};
use reth_storage_errors::provider::ProviderResult;

/// A helper type alias to access [`HeaderProvider::Header`].
pub type ProviderHeader<P> = <P as HeaderProvider>::Header;

/// Client trait for fetching `Header` related data.
#[auto_impl::auto_impl(&, Arc)]
pub trait HeaderProvider: Send {
    /// The header type this provider supports.
    type Header: BlockHeader;

    /// Check if block is known
    fn is_known(&self, block_hash: BlockHash) -> ProviderResult<bool> {
        self.header(block_hash).map(|header| header.is_some())
    }

    /// Get header by block hash
    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>>;

    /// Retrieves the header sealed by the given block hash.
    fn sealed_header_by_hash(
        &self,
        block_hash: BlockHash,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        Ok(self.header(block_hash)?.map(|header| SealedHeader::new(header, block_hash)))
    }

    /// Get header by block number
    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Self::Header>>;

    /// Get the total difficulty for the block with the given hash (BSC parlia fork choice).
    ///
    /// Default returns `None`; providers that track total difficulty override this. Hash-keyed
    /// so it resolves the TD of a specific (possibly non-canonical) block during reorg handling.
    fn header_td(&self, _hash: &BlockHash) -> ProviderResult<Option<alloy_primitives::U256>> {
        Ok(None)
    }

    /// Get the total difficulty at the given block number (BSC parlia fork choice).
    ///
    /// Default returns `None`; providers that track total difficulty override this.
    fn header_td_by_number(
        &self,
        _number: BlockNumber,
    ) -> ProviderResult<Option<alloy_primitives::U256>> {
        Ok(None)
    }

    /// Largest gap this will rebuild inline before giving up.
    ///
    /// A rebuild runs inside the caller's read transaction, so it has to finish well inside
    /// `--db.read-transaction-timeout` (30s by default). A mainnet-sized walk does not: it is
    /// killed, retried, killed again, and the node makes no progress while looking healthy.
    /// Refusing loudly past this point keeps that failure out of the live path -- use the
    /// offline `db rebuild-td` repair for anything larger.
    const MAX_INLINE_TD_REBUILD: u64 = 100_000;

    /// Total difficulty at `number`, rebuilding it when the entry is missing.
    ///
    /// `header_td_by_number` only answers for blocks whose TD this node actually persisted, so
    /// a datadir older than the parlia TD feature (or one whose table was cleared to repair it)
    /// has nothing to return. Callers that advertise TD to peers must not treat that as zero:
    /// a zero total difficulty makes geth rank us below its own head and refuse to sync from us.
    ///
    /// Stored TDs form a contiguous suffix ending at the tip, so a miss at `number` means every
    /// lower block is missing too, and the only anchor left is genesis. Providers that can seek
    /// the table directly should override this with a cheaper walk.
    ///
    /// Bounded by [`Self::MAX_INLINE_TD_REBUILD`]; a larger gap is an error, not a stall.
    fn total_difficulty_at(&self, number: BlockNumber) -> ProviderResult<alloy_primitives::U256> {
        if let Some(td) = self.header_td_by_number(number)? {
            return Ok(td)
        }

        if number > Self::MAX_INLINE_TD_REBUILD {
            return Err(reth_storage_errors::provider::ProviderError::TotalDifficultyRebuildTooLarge {
                number,
                span: number,
            })
        }

        let genesis = self
            .header_by_number(0)?
            .ok_or(reth_storage_errors::provider::ProviderError::HeaderNotFound(0.into()))?;
        let mut td = genesis.difficulty();

        // Chunked so a cold datadir does not materialize millions of headers at once.
        const CHUNK: u64 = 65_536;
        let mut base = 0u64;
        while base < number {
            let end = core::cmp::min(base + CHUNK, number);
            let headers = self.headers_range(base + 1..=end)?;
            // A short range means a header is missing; summing it would silently under-count.
            if headers.len() as u64 != end - base {
                return Err(reth_storage_errors::provider::ProviderError::HeaderNotFound(
                    (base + 1).into(),
                ))
            }
            for header in headers {
                td += header.difficulty();
            }
            base = end;
        }

        Ok(td)
    }

    /// Get header by block number or hash
    fn header_by_hash_or_number(
        &self,
        hash_or_num: BlockHashOrNumber,
    ) -> ProviderResult<Option<Self::Header>> {
        match hash_or_num {
            BlockHashOrNumber::Hash(hash) => self.header(hash),
            BlockHashOrNumber::Number(num) => self.header_by_number(num),
        }
    }

    /// Get headers in range of block numbers
    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>>;

    /// Get a single sealed header by block number.
    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>>;

    /// Get headers in range of block numbers.
    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.sealed_headers_while(range, |_| true)
    }

    /// Get sealed headers while `predicate` returns `true` or the range is exhausted.
    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>>;
}
