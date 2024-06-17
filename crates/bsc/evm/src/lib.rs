//! EVM config for bsc.

// TODO: doc
#![allow(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// The `bsc` feature must be enabled to use this crate.
#![cfg(feature = "bsc")]

use reth_evm::{ConfigureEvm, ConfigureEvmEnv};
use reth_primitives::{
    revm::{config::revm_spec, env::fill_tx_env},
    revm_primitives::{AnalysisKind, CfgEnvWithHandlerCfg, TxEnv},
    Address, ChainSpec, Head, Header, TransactionSigned, U256,
};
use reth_revm::{inspector_handle_register, Database, Evm, EvmBuilder, GetInspector};

pub mod execute;
pub use execute::*;
mod error;
pub use error::BscBlockExecutionError;
mod patch_hertz;
mod post_execution;
mod pre_execution;

/// Bsc-related EVM configuration.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscEvmConfig;

impl ConfigureEvmEnv for BscEvmConfig {
    fn fill_tx_env(tx_env: &mut TxEnv, transaction: &TransactionSigned, sender: Address) {
        fill_tx_env(tx_env, transaction, sender)
    }

    fn fill_cfg_env(
        cfg_env: &mut CfgEnvWithHandlerCfg,
        chain_spec: &ChainSpec,
        header: &Header,
        total_difficulty: U256,
    ) {
        let spec_id = revm_spec(
            chain_spec,
            Head {
                number: header.number,
                timestamp: header.timestamp,
                difficulty: header.difficulty,
                total_difficulty,
                hash: Default::default(),
            },
        );

        cfg_env.chain_id = chain_spec.chain().id();
        cfg_env.perf_analyse_created_bytecodes = AnalysisKind::Analyse;

        // Disable block gas limit check
        // system transactions do not have gas limit
        cfg_env.disable_block_gas_limit = true;

        cfg_env.handler_cfg.spec_id = spec_id;
        cfg_env.handler_cfg.is_bsc = chain_spec.is_bsc();
    }
}

impl ConfigureEvm for BscEvmConfig {
    type DefaultExternalContext<'a> = ();

    fn evm<'a, DB: Database + 'a>(&self, db: DB) -> Evm<'a, Self::DefaultExternalContext<'a>, DB> {
        EvmBuilder::default().with_db(db).bsc().build()
    }

    fn evm_with_inspector<'a, DB, I>(&self, db: DB, inspector: I) -> Evm<'a, I, DB>
    where
        DB: Database + 'a,
        I: GetInspector<DB>,
    {
        EvmBuilder::default()
            .with_db(db)
            .with_external_context(inspector)
            .bsc()
            .append_handler_register(inspector_handle_register)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_primitives::revm_primitives::{BlockEnv, CfgEnv};
    use revm_primitives::SpecId;

    #[test]
    #[ignore]
    fn test_fill_cfg_and_block_env() {
        let mut cfg_env = CfgEnvWithHandlerCfg::new_with_spec_id(CfgEnv::default(), SpecId::LATEST);
        let mut block_env = BlockEnv::default();
        let header = Header::default();
        let chain_spec = ChainSpec::default();
        let total_difficulty = U256::ZERO;

        BscEvmConfig::fill_cfg_and_block_env(
            &mut cfg_env,
            &mut block_env,
            &chain_spec,
            &header,
            total_difficulty,
        );

        assert_eq!(cfg_env.chain_id, chain_spec.chain().id());
    }
}
