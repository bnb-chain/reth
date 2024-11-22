//! Performance optimization arguments

use clap::Args;

/// Parameters for performance optimization
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
#[command(next_help_heading = "Performance Optimization")]
pub struct PerformanceOptimizationArgs {
    /// Skips state root validation during block import.
    /// This flag is intended for performance optimization when importing blocks from trusted
    /// sources.
    /// **Warning: Enabling this option reduces the integrity of chain data validation.
    /// Once enabled, it cannot be disabled, and the node will permanently skip state root
    /// validation. Use only if you fully understand the consequences.**
    #[arg(long = "optimize.skip-state-root-validation", default_value_t = false)]
    pub skip_state_root_validation: bool,

    /// Enable execution cache during live-sync block import.
    /// This flag is intended for performance optimization when importing blocks of live-sync.
    #[arg(long = "optimize.enable-execution-cache", default_value_t = false)]
    pub enable_execution_cache: bool,

    /// Enables state root computation in the background with a persistent database.
    ///
    /// This option is intended for performance optimization when importing blocks
    /// during live sync. It allows state root calculations to be performed
    /// concurrently with other operations, potentially reducing overall
    /// processing time.
    #[arg(long = "optimize.compute-state-root-in-background", default_value_t = false)]
    pub compute_state_root_in_background: bool,
}
