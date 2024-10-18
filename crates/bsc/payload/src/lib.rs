//! Bsc's payload builder implementation.

#![allow(missing_docs)]
#![cfg_attr(all(not(test), feature = "bsc"), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(clippy::useless_let_if_seq)]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

pub mod builder;
pub use builder::BscPayloadBuilder;
pub mod payload;
pub use payload::{BscBuiltPayload, BscPayloadBuilderAttributes};
