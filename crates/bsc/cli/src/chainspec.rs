use std::{ffi::OsStr, sync::Arc};

use clap::{builder::TypedValueParser, error::Result, Arg, Command};
use reth_bsc_chainspec::{BscChainSpec, BSC_CHAPEL, BSC_DEV, BSC_MAINNET, BSC_RIALTO};
use reth_cli::chainspec::ChainSpecParser;

/// Clap value parser for [`ChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
fn chain_value_parser(s: &str) -> eyre::Result<Arc<BscChainSpec>, eyre::Error> {
    Ok(match s {
        "bsc" | "bsc-mainnet" | "bsc_mainnet" => BSC_MAINNET.clone(),
        "bsc-testnet" | "bsc-chapel" | "bsc_testnet" | "bsc_chapel" => BSC_CHAPEL.clone(),
        "bsc-rialto" | "bsc-qa" | "bsc_rialto" | "bsc_qa" => BSC_RIALTO.clone(),
        "dev" => BSC_DEV.clone(),
        _ => return Err(eyre::Report::msg("Invalid chain spec")),
    })
}

/// Bsc chain specification parser.
#[derive(Debug, Clone, Default)]
pub struct BscChainSpecParser;

impl ChainSpecParser for BscChainSpecParser {
    type ChainSpec = BscChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = &[
        "bsc",
        "bsc-mainnet",
        "bsc_mainnet",
        "bsc-testnet",
        "bsc-chapel",
        "bsc_testnet",
        "bsc_chapel",
        "bsc-qa",
        "bsc-rialto",
        "bsc_qa",
        "bsc_rialto",
        "dev",
    ];

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

impl TypedValueParser for BscChainSpecParser {
    type Value = Arc<BscChainSpec>;

    fn parse_ref(
        &self,
        _cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let val =
            value.to_str().ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidUtf8))?;
        <Self as ChainSpecParser>::parse(val).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            let possible_values = Self::SUPPORTED_CHAINS.join(",");
            let msg = format!(
                "Invalid value '{val}' for {arg}: {err}.\n    [possible values: {possible_values}]"
            );
            clap::Error::raw(clap::error::ErrorKind::InvalidValue, msg)
        })
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        let values = Self::SUPPORTED_CHAINS.iter().map(clap::builder::PossibleValue::new);
        Some(Box::new(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_chain_spec() {
        for &chain in BscChainSpecParser::SUPPORTED_CHAINS {
            assert!(<BscChainSpecParser as ChainSpecParser>::parse(chain).is_ok());
        }
    }
}
