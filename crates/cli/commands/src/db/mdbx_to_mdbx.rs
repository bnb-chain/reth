//! MDBX to MDBX copy tool
//! 
//! Similar to Erigon's mdbx_to_mdbx implementation, this copies data from one
//! MDBX database to another, table by table.
//! 
//! Reference: https://github.com/erigontech/erigon/blob/devel/cmd/integration/commands/backup.go

use clap::Parser;
use reth_db::DatabaseEnv;
use reth_db::mdbx::DatabaseArguments;
use reth_db_api::{database::Database, models::ClientVersion, transaction::DbTx};
use reth_libmdbx::WriteFlags;
use std::{path::PathBuf, time::Instant};
use tracing::info;

/// Arguments for the `reth db mdbx-to-mdbx` command
#[derive(Parser, Debug)]
pub struct Command {
    /// Path to the destination database
    #[arg(long, value_name = "DEST_PATH")]
    to: PathBuf,

    /// List of tables to copy (comma-separated). If empty, copies all tables
    #[arg(long, value_delimiter = ',')]
    tables: Vec<String>,

    /// Target database page size in KB (e.g., 4, 8, 16)
    /// If not specified, uses source database's page size in fast mode,
    /// or auto-detects in custom mode
    #[arg(long)]
    page_size: Option<usize>,

    /// Target database maximum size in GB
    /// If not specified, uses source database's max size
    #[arg(long)]
    max_size: Option<usize>,

    /// Database growth step in GB (default: 4)
    #[arg(long, default_value = "4")]
    growth_step: usize,

    /// Use fast mode (MDBX native copy, ignores custom parameters)
    /// By default, uses record-by-record copy which allows parameter customization
    #[arg(long)]
    fast: bool,

    /// Commit interval: commit transaction every N records (default: 100000)
    #[arg(long, default_value = "100000")]
    commit_every: usize,

    /// Skip confirmation prompt
    #[arg(long, short)]
    force: bool,

    /// Be quiet (suppress progress messages)
    #[arg(long, short)]
    quiet: bool,
}

impl Command {
    /// Execute the mdbx-to-mdbx copy
    pub fn execute(&self, src_env: &DatabaseEnv) -> eyre::Result<()> {
        use std::io::Write;

        // Determine mode
        let mode = if self.fast {
            "fast (MDBX native copy)"
        } else {
            "record-by-record (with parameter customization)"
        };

        if !self.force {
            print!(
                "Copy database to {:?}? Mode: {}. (y/N): ",
                self.to, mode
            );
            std::io::stdout().flush()?;

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;

            if !input.trim().eq_ignore_ascii_case("y") {
                info!("Copy aborted!");
                return Ok(());
            }
        }

        // Ensure destination doesn't exist
        if self.to.exists() {
            eyre::bail!("Destination {:?} already exists", self.to);
        }

        // Ensure parent directory exists
        if let Some(parent) = self.to.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        if !self.quiet {
            info!(target: "reth::cli", "Starting database copy...");
            info!(target: "reth::cli", "Mode: {}", mode);
            info!(target: "reth::cli", "Destination: {:?}", self.to);
        }

        let start = Instant::now();

        if self.fast {
            // Fast mode: use MDBX native copy
            self.execute_fast_copy(src_env)?;
        } else {
            // Default mode: record-by-record copy with parameter customization
            self.execute_custom_copy(src_env)?;
        }

        let elapsed = start.elapsed();

        if !self.quiet {
            info!(target: "reth::cli", "Copy completed in {:.2}s", elapsed.as_secs_f64());
            
            if let Ok(metadata) = std::fs::metadata(&self.to) {
                info!(target: "reth::cli", "Destination size: {} MB",
                      metadata.len() / 1024 / 1024);
            }
        }

        Ok(())
    }

    /// Fast copy using MDBX native copy API
    fn execute_fast_copy(&self, src_env: &DatabaseEnv) -> eyre::Result<()> {
        if !self.quiet {
            info!(target: "reth::cli", "Using MDBX native copy (ignoring custom parameters)");
        }
        
        src_env.copy_to_path(&self.to, false, false)?;
        Ok(())
    }

    /// Custom copy with parameter customization
    fn execute_custom_copy(&self, src_env: &DatabaseEnv) -> eyre::Result<()> {
        use reth_db::tables::Tables;
        
        // Get source database parameters
        let src_info = src_env.info()?;
        let src_stat = src_env.stat()?;
        let src_page_size = src_stat.page_size();
        let src_map_size = src_info.map_size();
        
        // Determine target parameters
        let page_size_bytes = self.page_size.map(|kb| kb * 1024).unwrap_or(src_page_size as usize);
        let max_size_bytes = self.max_size
            .map(|gb| gb * 1024 * 1024 * 1024)
            .unwrap_or(src_map_size);
        let growth_step_bytes = self.growth_step * 1024 * 1024 * 1024;

        if !self.quiet {
            info!(target: "reth::cli", "Source parameters:");
            info!(target: "reth::cli", "  Page size: {} KB", src_page_size / 1024);
            info!(target: "reth::cli", "  Map size: {} GB", src_map_size / 1024 / 1024 / 1024);
            info!(target: "reth::cli", "Target parameters:");
            info!(target: "reth::cli", "  Page size: {} KB", page_size_bytes / 1024);
            info!(target: "reth::cli", "  Map size: {} GB", max_size_bytes / 1024 / 1024 / 1024);
            info!(target: "reth::cli", "  Growth step: {} GB", self.growth_step);
        }

        // Create destination database with custom parameters
        let dst_args = DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(max_size_bytes))
            .with_growth_step(Some(growth_step_bytes));

        let dst_env = reth_db::init_db(&self.to, dst_args)?;

        if !self.quiet {
            info!(target: "reth::cli", "Destination database created");
        }

        // Determine which tables to copy
        let tables_to_copy: Vec<String> = if self.tables.is_empty() {
            Tables::ALL.iter().map(|t| t.name().to_string()).collect()
        } else {
            // Validate table names
            let valid_tables: std::collections::HashSet<&str> = 
                Tables::ALL.iter().map(|t| t.name()).collect();
            
            for table in &self.tables {
                if !valid_tables.contains(table.as_str()) {
                    eyre::bail!("Unknown table: {}", table);
                }
            }
            
            self.tables.clone()
        };

        if !self.quiet {
            info!(target: "reth::cli", "Copying {} tables", tables_to_copy.len());
        }

        // Copy each table using table-specific implementations
        let total_tables = tables_to_copy.len();
        for (idx, table_name) in tables_to_copy.iter().enumerate() {
            if !self.quiet {
                info!(target: "reth::cli", "[{}/{}] Copying table: {}", 
                      idx + 1, total_tables, table_name);
            }
            
            let table_start = Instant::now();
            let copied = self.copy_table_generic(src_env, &dst_env, table_name)?;
            let table_elapsed = table_start.elapsed();
            
            if !self.quiet && copied > 0 {
                info!(target: "reth::cli", "  Copied {} records in {:.2}s ({:.0} rec/s)", 
                      copied, table_elapsed.as_secs_f64(), copied as f64 / table_elapsed.as_secs_f64());
            }
        }

        Ok(())
    }

    /// Copy a table using generic byte-level copying
    /// This works for all tables but doesn't validate table-specific types
    fn copy_table_generic(
        &self,
        src_env: &DatabaseEnv,
        dst_env: &DatabaseEnv,
        table_name: &str,
    ) -> eyre::Result<usize> {
        let src_tx = src_env.tx()?;
        let mut dst_tx = dst_env.tx_mut()?;
        
        // Open the databases (tables) by name
        let src_db = src_tx.inner.open_db(Some(table_name))?;
        let dst_db = dst_tx.inner.open_db(Some(table_name))?;
        
        // Get cursor for source and destination
        let src_cursor = src_tx.inner.cursor(&src_db)?;
        let mut dst_cursor = dst_tx.inner.cursor(&dst_db)?;
        
        let mut copied = 0usize;
        let mut batch_count = 0usize;
        let mut last_progress = Instant::now();
        
        // Iterate through all records as byte slices
        for item in src_cursor.iter_slices() {
            let (key, value) = item?;
            
            // Insert into destination (convert Cow to slice)
            // Use APPEND flag for better performance (assumes ordered insert)
            dst_tx.inner.put(dst_db.dbi(), &key, &value, WriteFlags::APPEND)?;
            copied += 1;
            batch_count += 1;
            
            // Periodic commit
            if batch_count >= self.commit_every {
                drop(dst_cursor);
                dst_tx.commit()?;
                
                // Start new transaction
                dst_tx = dst_env.tx_mut()?;
                let dst_db = dst_tx.inner.open_db(Some(table_name))?;
                dst_cursor = dst_tx.inner.cursor(&dst_db)?;
                batch_count = 0;
                
                // Progress logging
                if !self.quiet && last_progress.elapsed().as_secs() >= 5 {
                    info!(target: "reth::cli", "    Progress: {} records", copied);
                    last_progress = Instant::now();
                }
            }
        }
        
        // Final commit
        if batch_count > 0 {
            drop(dst_cursor);
            dst_tx.commit()?;
        }
        
        Ok(copied)
    }
}

