//! `reth db rebuild-td` command: rebuild the parlia total-difficulty table offline.

use alloy_consensus::BlockHeader as _;
use clap::Parser;
use reth_db_api::{tables, transaction::DbTxMut};
use reth_db_common::DbTool;
use reth_provider::{
    providers::ProviderNodeTypes, BlockNumReader, DBProvider, DatabaseProviderFactory,
    HeaderProvider,
};
use tracing::info;

/// `reth db rebuild-td` subcommand
///
/// Recomputes `HeaderTerminalDifficulties` from genesis and rewrites every row.
///
/// The live write path can only rebuild a small gap -- it runs inside a read transaction and
/// would be killed by `--db.read-transaction-timeout` on a large chain. This does the same work
/// with the node stopped, committing in batches so no single transaction is long-lived.
///
/// Needed when the stored TD is wrong rather than missing: rows written before the parlia TD
/// fix were anchored at zero wherever the parent was absent, leaving every later row offset by a
/// constant. Those rows look internally consistent, so nothing detects them -- but the node
/// advertises a TD far below the truth and geth peers refuse to sync from it.
#[derive(Parser, Debug)]
pub struct Command {
    /// Commit every N blocks. Lower values use less memory and hold shorter transactions.
    #[arg(long, default_value_t = 500_000)]
    commit_interval: u64,

    /// Report progress every N blocks.
    #[arg(long, default_value_t = 2_000_000)]
    log_interval: u64,
}

impl Command {
    /// Execute `db rebuild-td`
    pub fn execute<N: ProviderNodeTypes>(self, tool: &DbTool<N>) -> eyre::Result<()> {
        let tip = tool.provider_factory.provider()?.best_block_number()?;
        info!(target: "reth::cli", tip, "Rebuilding total difficulty from genesis");

        // Genesis TD is its own difficulty; `save_blocks` never writes a row for it.
        let mut td = {
            let provider = tool.provider_factory.provider()?;
            provider
                .header_by_number(0)?
                .ok_or_else(|| eyre::eyre!("genesis header missing"))?
                .difficulty()
        };

        let mut written = 0u64;
        let mut next = 0u64;
        while next <= tip {
            let end = (next + self.commit_interval - 1).min(tip);
            let provider_rw = tool.provider_factory.database_provider_rw()?;

            if next == 0 {
                provider_rw.tx_ref().put::<tables::HeaderTerminalDifficulties>(0, td.into())?;
                written += 1;
                next = 1;
            }

            // `headers_range` is served from static files, so this is a sequential read.
            let headers = provider_rw.headers_range(next..=end)?;
            if headers.len() as u64 != end - next + 1 {
                eyre::bail!(
                    "expected {} headers in {next}..={end}, found {} -- static files look \
                     incomplete, refusing to write a truncated TD table",
                    end - next + 1,
                    headers.len()
                );
            }

            for (offset, header) in headers.into_iter().enumerate() {
                let number = next + offset as u64;
                td += header.difficulty();
                provider_rw
                    .tx_ref()
                    .put::<tables::HeaderTerminalDifficulties>(number, td.into())?;
                written += 1;
                if number.is_multiple_of(self.log_interval) {
                    info!(target: "reth::cli", number, td = %td, "Rebuilding total difficulty");
                }
            }

            provider_rw.commit()?;
            next = end + 1;
        }

        info!(target: "reth::cli", tip, written, td = %td, "Total difficulty rebuilt");
        println!("rebuilt {written} rows, total difficulty at block {tip} is {td}");
        Ok(())
    }
}
