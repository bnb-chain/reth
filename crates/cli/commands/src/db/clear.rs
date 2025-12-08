use clap::{Parser, Subcommand};
use reth_db::static_file::iter_static_files;
use reth_db_api::{
    database::Database,
    transaction::{DbTx, DbTxMut},
    AccountsTrie, StoragesTrie,
};
use reth_node_builder::NodeTypesWithDB;
use reth_provider::{ProviderFactory, StaticFileProviderFactory};
use reth_static_file_types::StaticFileSegment;

/// The arguments for the `reth db clear` command
#[derive(Parser, Debug)]
pub struct Command {
    #[command(subcommand)]
    subcommand: Subcommands,
}

impl Command {
    /// Execute `db clear` command
    pub fn execute<N: NodeTypesWithDB>(
        self,
        provider_factory: ProviderFactory<N>,
    ) -> eyre::Result<()> {
        match self.subcommand {
            Subcommands::Mdbx => {
                let db = provider_factory.db_ref();
                let tx = db.tx_mut()?;

                // Clear AccountsTrie table
                tx.clear::<AccountsTrie>()?;
                println!("Cleared AccountsTrie table");

                // Clear StoragesTrie table
                tx.clear::<StoragesTrie>()?;
                println!("Cleared StoragesTrie table");

                tx.commit()?;
                println!("Successfully cleared AccountsTrie and StoragesTrie tables");
            }
            Subcommands::StaticFile { segment } => {
                let static_file_provider = provider_factory.static_file_provider();
                let static_files = iter_static_files(static_file_provider.directory())?;

                if let Some(segment_static_files) = static_files.get(&segment) {
                    for (block_range, _) in segment_static_files {
                        static_file_provider.delete_jar(segment, block_range.start())?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Deletes AccountsTrie and StoragesTrie table entries
    Mdbx,
    /// Deletes all static file segment entries
    StaticFile { segment: StaticFileSegment },
}
