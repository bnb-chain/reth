//! Bsc-Reth RPC support.

#![allow(missing_docs)]
#![cfg_attr(all(not(test), feature = "bsc"), warn(unused_crate_dependencies))]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

pub mod error;
pub mod eth;

pub use error::BscEthApiError;
pub use eth::BscEthApi;
