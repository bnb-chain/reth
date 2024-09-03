//! Chain specification for the BSC Mainnet network.

#[cfg(not(feature = "std"))]
use alloc::sync::Arc;
#[cfg(feature = "std")]
use std::sync::Arc;

use alloy_chains::{Chain, NamedChain};
use alloy_primitives::{b256, U256};
use once_cell::sync::Lazy;
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_ethereum_forks::BscHardfork;

use crate::BscChainSpec;

/// The BSC mainnet spec
pub static BSC_CHAPEL: Lazy<Arc<BscChainSpec>> = Lazy::new(|| {
    BscChainSpec {
        inner: ChainSpec {
            chain: Chain::from_named(NamedChain::BNBSmartChainTestnet),
            genesis: serde_json::from_str(include_str!("../res/genesis/bsc_chapel.json"))
                .expect("Can't deserialize BSC Testnet genesis json"),
            genesis_hash: Some(b256!(
                "6d3c66c5357ec91d5c43af47e234a939b22557cbb552dc45bebbceeed90fbe34"
            )),
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks: BscHardfork::bsc_testnet(),
            deposit_contract: None,
            base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
            prune_delete_limit: 3500,
            ..Default::default()
        },
    }
    .into()
});
