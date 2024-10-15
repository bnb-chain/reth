//! Chain specification for the Opbnb Mainnet network.

#[cfg(not(feature = "std"))]
use alloc::sync::Arc;
#[cfg(feature = "std")]
use std::sync::Arc;

use alloy_chains::Chain;
use alloy_primitives::{b256, U256};
use once_cell::sync::Lazy;
use reth_chainspec::{once_cell_set, BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_ethereum_forks::EthereumHardfork;
use reth_optimism_forks::OptimismHardfork;

use crate::OpChainSpec;

/// The opbnb mainnet spec
pub static OPBNB_MAINNET: Lazy<Arc<OpChainSpec>> = Lazy::new(|| {
    OpChainSpec {
        inner: ChainSpec {
            chain: Chain::opbnb_mainnet(),
            genesis: serde_json::from_str(include_str!("../res/genesis/opbnb_mainnet.json"))
                .expect("Can't deserialize opBNB mainent genesis json"),
            genesis_hash: once_cell_set(b256!(
                "4dd61178c8b0f01670c231597e7bcb368e84545acd46d940a896d6a791dd6df4"
            )),
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks: OptimismHardfork::opbnb_mainnet(),
            base_fee_params: BaseFeeParamsKind::Variable(
                vec![(EthereumHardfork::London.boxed(), BaseFeeParams::ethereum())].into(),
            ),
            prune_delete_limit: 0,
            ..Default::default()
        },
    }
    .into()
});
