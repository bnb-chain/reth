//! Standalone crate for ethereum-specific Reth configuration and builder types.

#![allow(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

pub use reth_ethereum_engine_primitives::EthEngineTypes;

pub mod evm;
pub use evm::{BscEvmConfig, BscExecutorProvider};

pub mod node;
pub use node::BscNode;
