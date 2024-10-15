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

/// The opbnb testnet spec
pub static OPBNB_TESTNET: Lazy<Arc<OpChainSpec>> = Lazy::new(|| {
    OpChainSpec {
        inner: ChainSpec {
            chain: Chain::opbnb_testnet(),
            genesis: serde_json::from_str(include_str!("../res/genesis/opbnb_testnet.json"))
                .expect("Can't deserialize opBNB testnet genesis json"),
            genesis_hash: once_cell_set(b256!(
                "51fa57729dfb1c27542c21b06cb72a0459c57440ceb43a465dae1307cd04fe80"
            )),
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks: OptimismHardfork::opbnb_testnet(),
            base_fee_params: BaseFeeParamsKind::Variable(
                vec![(EthereumHardfork::London.boxed(), BaseFeeParams::ethereum())].into(),
            ),
            prune_delete_limit: 0,
            ..Default::default()
        },
    }
    .into()
});
