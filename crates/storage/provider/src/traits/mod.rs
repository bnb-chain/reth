//! Collection of common provider traits.

// Re-export all the traits
pub use reth_storage_api::*;

// Re-export for convenience
pub use reth_evm::provider::EvmEnvProvider;

mod block;
pub use block::*;

mod chain_info;
pub use chain_info::CanonChainTracker;

mod header_sync_gap;
pub use header_sync_gap::{HeaderSyncGap, HeaderSyncGapProvider};

mod state;
pub use state::{StateChangeWriter, StateWriter};

pub use reth_chainspec::ChainSpecProvider;

mod hashing;
pub use hashing::HashingWriter;

mod trie;
pub use trie::{StorageTrieWriter, TrieWriter};

mod history;
pub use history::HistoryWriter;

mod static_file_provider;
pub use static_file_provider::StaticFileProviderFactory;

mod stats;
pub use stats::StatsReader;

mod full;
pub use full::{FullProvider, FullRpcProvider};

mod tree_viewer;
pub use tree_viewer::TreeViewer;

mod finalized_block;
pub use finalized_block::{ChainStateBlockReader, ChainStateBlockWriter};

mod parlia_provider;
pub use parlia_provider::ParliaProvider;

mod parlia_snapshot;
pub use parlia_snapshot::ParliaSnapshotReader;

#[cfg(feature = "bsc")]
use reth_bsc_forks::BscHardforks;
#[cfg(not(feature = "bsc"))]
use reth_chainspec::EthereumHardforks;

#[cfg(not(feature = "bsc"))]
/// Use eth hardforks if bsc feature is not enabled
pub trait ChainSpecHardforks = EthereumHardforks;

#[cfg(feature = "bsc")]
/// Use bsc hardforks if bsc feature is enabled
pub trait ChainSpecHardforks = BscHardforks;
