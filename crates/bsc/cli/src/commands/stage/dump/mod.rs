//! Database debugging tool

use clap::Parser;
use reth_bsc_chainspec::BscChainSpec;
use reth_bsc_evm::BscExecutorProvider;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{
    common::{AccessRights, Environment, EnvironmentArgs},
    handle_stage,
    stage::dump::{
        dump_execution_stage, dump_hashing_account_stage, dump_hashing_storage_stage,
        dump_merkle_stage, StageCommand, Stages,
    },
};
use reth_db_common::DbTool;
use reth_node_builder::NodeTypesWithEngine;
use reth_node_core::args::DatadirArgs;

/// `reth dump-stage` command
#[derive(Debug, Parser)]
pub struct Command<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    #[command(subcommand)]
    command: Stages,
}

impl<C: ChainSpecParser<ChainSpec = BscChainSpec>> Command<C> {
    /// Execute `dump-stage` command
    pub async fn execute<N>(self) -> eyre::Result<()>
    where
        N: NodeTypesWithEngine<ChainSpec = C::ChainSpec>,
    {
        let Environment { provider_factory, .. } = self.env.init::<N>(AccessRights::RO)?;
        let tool = DbTool::new(provider_factory.clone())?;

        match &self.command {
            Stages::Execution(cmd) => {
                let executor = BscExecutorProvider::bsc(tool.chain(), provider_factory);
                handle_stage!(dump_execution_stage, &tool, cmd, executor)
            }
            Stages::StorageHashing(cmd) => handle_stage!(dump_hashing_storage_stage, &tool, cmd),
            Stages::AccountHashing(cmd) => handle_stage!(dump_hashing_account_stage, &tool, cmd),
            Stages::Merkle(cmd) => handle_stage!(dump_merkle_stage, &tool, cmd),
        }

        Ok(())
    }
}
