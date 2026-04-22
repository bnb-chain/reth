//! State database arguments

use clap::Args;

/// Parameters for state database configuration
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
#[command(next_help_heading = "State Database")]
pub struct StateDbArgs {
    /// Use `TrieDB` instead of MDBX for state database.
    #[arg(long = "statedb.triedb", default_value_t = false)]
    pub triedb: bool,
}
