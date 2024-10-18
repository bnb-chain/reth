//! `reth stage` command

use clap::{Parser, Subcommand};
use reth_bsc_chainspec::BscChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::stage::{drop, unwind};
use reth_cli_runner::CliContext;
use reth_node_builder::NodeTypesWithEngine;

mod dump;
mod run;

/// `reth stage` command
#[derive(Debug, Parser)]
pub struct StageCommand<C: ChainSpecParser> {
    #[command(subcommand)]
    command: Subcommands<C>,
}

/// `reth stage` subcommands
#[derive(Subcommand, Debug)]
pub enum Subcommands<C: ChainSpecParser> {
    /// Run a single stage.
    ///
    /// Note that this won't use the Pipeline and as a result runs stages
    /// assuming that all the data can be held in memory. It is not recommended
    /// to run a stage for really large block ranges if your computer does not have
    /// a lot of memory to store all the data.
    Run(run::Command<C>),
    /// Drop a stage's tables from the database.
    Drop(drop::Command<C>),
    /// Dumps a stage from a range into a new database.
    Dump(dump::Command<C>),
    /// Unwinds a certain block range, deleting it from the database.
    Unwind(unwind::Command<C>),
}

impl<C: ChainSpecParser<ChainSpec = BscChainSpec>> StageCommand<C> {
    /// Execute `stage` command
    pub async fn execute<N>(self, ctx: CliContext) -> eyre::Result<()>
    where
        N: NodeTypesWithEngine<ChainSpec = C::ChainSpec>,
    {
        match self.command {
            Subcommands::Run(command) => command.execute::<N>(ctx).await,
            Subcommands::Drop(command) => command.execute::<N>().await,
            Subcommands::Dump(command) => command.execute::<N>().await,
            Subcommands::Unwind(command) => command.execute::<N>().await,
        }
    }
}
