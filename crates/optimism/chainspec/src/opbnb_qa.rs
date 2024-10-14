//! Chain specification for the Opbnb QA network.

#[cfg(not(feature = "std"))]
use alloc::sync::Arc;
#[cfg(feature = "std")]
use std::sync::Arc;

use crate::OpChainSpec;
use alloy_chains::Chain;
use alloy_primitives::{b256, U256};
use once_cell::sync::Lazy;
use reth_chainspec::{once_cell_set, BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_ethereum_forks::EthereumHardfork;
use reth_optimism_forks::OptimismHardfork;

/// The opbnb qa spec
pub static OPBNB_QA: Lazy<Arc<OpChainSpec>> = Lazy::new(|| {
    OpChainSpec {
        inner: ChainSpec {
            chain: Chain::from_id(3534),
            genesis: serde_json::from_str(include_str!("../res/genesis/opbnb_qa.json"))
                .expect("Can't deserialize opBNB qa genesis json"),
            genesis_hash: once_cell_set(b256!(
                "1c2ad01526f22793643de4978dbf5cec5aeaedcb628470de8b950f8a46539ddf"
            )),
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks: OptimismHardfork::opbnb_qa(),
            base_fee_params: BaseFeeParamsKind::Variable(
                vec![(EthereumHardfork::London.boxed(), BaseFeeParams::ethereum())].into(),
            ),
            prune_delete_limit: 0,
            ..Default::default()
        },
    }
    .into()
});
