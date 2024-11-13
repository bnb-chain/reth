//! Ethereum protocol-related constants

use alloy_primitives::{b256, B256};

/// Gas units, for example [`GIGAGAS`].
pub mod gas_units;
pub use gas_units::{GIGAGAS, KILOGAS, MEGAGAS};

/// The client version: `reth/v{major}.{minor}.{patch}`
pub const RETH_CLIENT_VERSION: &str = concat!("reth/v", env!("CARGO_PKG_VERSION"));

/// Maximum extra data size in a block after genesis
#[cfg(not(feature = "bsc"))]
pub const MAXIMUM_EXTRA_DATA_SIZE: usize = 32;

/// Maximum extra data size in a block after genesis
#[cfg(feature = "bsc")]
pub const MAXIMUM_EXTRA_DATA_SIZE: usize = 1024 * 1024;

/// Initial base fee as defined in [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559)
#[cfg(not(feature = "bsc"))]
pub const EIP1559_INITIAL_BASE_FEE: u64 = 1_000_000_000;

/// Initial base fee of bsc
#[cfg(feature = "bsc")]
pub const EIP1559_INITIAL_BASE_FEE: u64 = 0;

/// Minimum gas limit allowed for transactions.
pub const MINIMUM_GAS_LIMIT: u64 = 5000;

/// Holesky genesis hash: `0xb5f7f912443c940f21fd611f12828d75b534364ed9e95ca4e307729a4661bde4`
pub const HOLESKY_GENESIS_HASH: B256 =
    b256!("b5f7f912443c940f21fd611f12828d75b534364ed9e95ca4e307729a4661bde4");

/// Ommer root of empty list: `0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347`
pub const EMPTY_OMMER_ROOT_HASH: B256 =
    b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347");

/// Empty mix hash
pub const EMPTY_MIX_HASH: B256 =
    b256!("0000000000000000000000000000000000000000000000000000000000000000");

/// The number of blocks to unwind during a reorg that already became a part of canonical chain.
///
/// In reality, the node can end up in this particular situation very rarely. It would happen only
/// if the node process is abruptly terminated during ongoing reorg and doesn't boot back up for
/// long period of time.
///
/// Unwind depth of `3` blocks significantly reduces the chance that the reorged block is kept in
/// the database.
pub const BEACON_CONSENSUS_REORG_UNWIND_DEPTH: u64 = 3;
