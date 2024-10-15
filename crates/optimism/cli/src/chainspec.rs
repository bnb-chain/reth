use reth_cli::chainspec::{parse_genesis, ChainSpecParser};
use reth_optimism_chainspec::{
    OpChainSpec, BASE_MAINNET, BASE_SEPOLIA, OPBNB_MAINNET, OPBNB_QA, OPBNB_TESTNET, OP_DEV,
    OP_MAINNET, OP_SEPOLIA,
};
use std::sync::Arc;

/// Optimism chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct OpChainSpecParser;

impl ChainSpecParser for OpChainSpecParser {
    type ChainSpec = OpChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = &[
        "dev",
        "optimism",
        "optimism_sepolia",
        "optimism-sepolia",
        "base",
        "base_sepolia",
        "base-sepolia",
        "opbnb_mainnet",
        "opbnb-mainnet",
        "opbnb_testnet",
        "opbnb-testnet",
        "opbnb_qa",
        "opbnb-qa",
    ];

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

/// Clap value parser for [`OpChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<OpChainSpec>, eyre::Error> {
    Ok(match s {
        "dev" => OP_DEV.clone(),
        "optimism" => OP_MAINNET.clone(),
        "optimism_sepolia" | "optimism-sepolia" => OP_SEPOLIA.clone(),
        "base" => BASE_MAINNET.clone(),
        "base_sepolia" | "base-sepolia" => BASE_SEPOLIA.clone(),
        "opbnb_mainnet" | "opbnb-mainnet" => OPBNB_MAINNET.clone(),
        "opbnb_testnet" | "opbnb-testnet" => OPBNB_TESTNET.clone(),
        "opbnb_qa" | "opbnb-qa" => OPBNB_QA.clone(),
        _ => Arc::new(parse_genesis(s)?.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_chain_spec() {
        for &chain in OpChainSpecParser::SUPPORTED_CHAINS {
            assert!(<OpChainSpecParser as ChainSpecParser>::parse(chain).is_ok());
        }
    }
}
