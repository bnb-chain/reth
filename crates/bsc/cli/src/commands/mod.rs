use std::fmt;

use clap::Subcommand;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{
    config_cmd, db, dump_genesis, init_cmd, init_state,
    node::{self, NoArgs},
    p2p, prune, recover,
};

use crate::{
    chainspec::BscChainSpecParser,
    commands::{import::ImportCommand, stage::StageCommand},
};

mod build_pipeline;
pub mod import;
pub mod stage;

/// Commands to be executed
#[derive(Debug, Subcommand)]
pub enum Commands<Spec: ChainSpecParser = BscChainSpecParser, Ext: clap::Args + fmt::Debug = NoArgs>
{
    /// Start the node
    #[command(name = "node")]
    Node(Box<node::NodeCommand<Spec, Ext>>),
    /// Initialize the database from a genesis file.
    #[command(name = "init")]
    Init(init_cmd::InitCommand<Spec>),
    /// Initialize the database from a state dump file.
    #[command(name = "init-state")]
    InitState(init_state::InitStateCommand<Spec>),
    /// This syncs RLP encoded blocks from a file, without executing.
    #[command(name = "import")]
    Import(ImportCommand<Spec>),
    /// Dumps genesis block JSON configuration to stdout.
    DumpGenesis(dump_genesis::DumpGenesisCommand<Spec>),
    /// Database debugging utilities
    #[command(name = "db")]
    Db(db::Command<Spec>),
    /// Manipulate individual stages.
    #[command(name = "stage")]
    Stage(Box<StageCommand<Spec>>),
    /// P2P Debugging utilities
    #[command(name = "p2p")]
    P2P(p2p::Command<Spec>),
    /// Write config to stdout
    #[command(name = "config")]
    Config(config_cmd::Command),
    /// Scripts for node recovery
    #[command(name = "recover")]
    Recover(recover::Command<Spec>),
    /// Prune according to the configuration without any limits
    #[command(name = "prune")]
    Prune(prune::PruneCommand<Spec>),
}
