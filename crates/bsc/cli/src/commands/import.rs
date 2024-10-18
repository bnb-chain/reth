//! Command that initializes the node by importing a chain from a file.
use crate::commands::build_pipeline::build_import_pipeline;
use clap::Parser;
use reth_bsc_chainspec::BscChainSpec;
use reth_bsc_consensus::Parlia;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_db::tables;
use reth_db_api::transaction::DbTx;
use reth_downloaders::file_client::{
    ChunkedFileReader, FileClient, DEFAULT_BYTE_LEN_CHUNK_CHAIN_FILE,
};
use reth_node_builder::NodeTypesWithEngine;
use reth_node_core::version::SHORT_VERSION;
use reth_provider::StageCheckpointReader;
use reth_prune::PruneModes;
use reth_stages::StageId;
use reth_static_file::StaticFileProducer;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info};

/// Syncs RLP encoded blocks from a file.
#[derive(Debug, Parser)]
pub struct ImportCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// Chunk byte length to read from file.
    #[arg(long, value_name = "CHUNK_LEN", verbatim_doc_comment)]
    chunk_len: Option<u64>,

    /// The path to a block file for import.
    ///
    /// The online stages (headers and bodies) are replaced by a file import, after which the
    /// remaining stages are executed.
    #[arg(value_name = "IMPORT_PATH", verbatim_doc_comment)]
    path: PathBuf,
}

impl<C: ChainSpecParser<ChainSpec = BscChainSpec>> ImportCommand<C> {
    /// Execute `import` command
    pub async fn execute<N: NodeTypesWithEngine<ChainSpec = C::ChainSpec>>(
        self,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "reth {} starting", SHORT_VERSION);

        debug!(target: "reth::cli",
            chunk_byte_len=self.chunk_len.unwrap_or(DEFAULT_BYTE_LEN_CHUNK_CHAIN_FILE),
            "Chunking chain import"
        );

        let Environment { provider_factory, config, .. } = self.env.init::<N>(AccessRights::RW)?;

        let consensus = Arc::new(Parlia::new(self.env.chain.clone(), config.parlia.clone()));
        info!(target: "reth::cli", "Consensus engine initialized");

        // open file
        let mut reader = ChunkedFileReader::new(&self.path, self.chunk_len).await?;

        let mut total_decoded_blocks = 0;
        let mut total_decoded_txns = 0;

        while let Some(file_client) = reader.next_chunk::<FileClient>().await? {
            // create a new FileClient from chunk read from file
            info!(target: "reth::cli",
                "Importing chain file chunk"
            );

            let tip = file_client.tip().ok_or(eyre::eyre!("file client has no tip"))?;
            info!(target: "reth::cli", "Chain file chunk read");

            total_decoded_blocks += file_client.headers_len();
            total_decoded_txns += file_client.total_transactions();

            let (mut pipeline, events) = build_import_pipeline(
                &config,
                provider_factory.clone(),
                &consensus,
                Arc::new(file_client),
                StaticFileProducer::new(provider_factory.clone(), PruneModes::default()),
                true,
                self.env.performance_optimization.skip_state_root_validation,
            )
            .await?;

            // override the tip
            pipeline.set_tip(tip);
            debug!(target: "reth::cli", ?tip, "Tip manually set");

            let provider = provider_factory.provider()?;

            let latest_block_number =
                provider.get_stage_checkpoint(StageId::Finish)?.map(|ch| ch.block_number);
            tokio::spawn(reth_node_events::node::handle_events(None, latest_block_number, events));

            // Run pipeline
            info!(target: "reth::cli", "Starting sync pipeline");
            tokio::select! {
                res = pipeline.run() => res?,
                _ = tokio::signal::ctrl_c() => {},
            }
        }

        let provider = provider_factory.provider()?;

        let total_imported_blocks = provider.tx_ref().entries::<tables::HeaderNumbers>()?;
        let total_imported_txns = provider.tx_ref().entries::<tables::TransactionHashNumbers>()?;

        if total_decoded_blocks != total_imported_blocks ||
            total_decoded_txns != total_imported_txns
        {
            error!(target: "reth::cli",
                total_decoded_blocks,
                total_imported_blocks,
                total_decoded_txns,
                total_imported_txns,
                "Chain was partially imported"
            );
        }

        info!(target: "reth::cli",
            total_imported_blocks,
            total_imported_txns,
            "Chain file imported"
        );

        Ok(())
    }
}
