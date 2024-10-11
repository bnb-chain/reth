//! Performance optimization arguments

use clap::Args;

/// Parameters for performance optimization
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
#[command(next_help_heading = "Performance Optimization")]
pub struct PerformanceOptimizationArgs {
    /// Skips state root validation during block import.
    /// This flag is intended for performance optimization when importing blocks from trusted
    /// sources.
    /// **Warning: This option compromises the validation of chain data. Use with caution.**
    #[arg(long = "optimize.skip-state-root-validation", default_value_t = false)]
    pub skip_state_root_validation: bool,

    /// Enable execution cache during live-sync block import.
    /// This flag is intended for performance optimization when importing blocks of live-sync.
    #[arg(long = "optimize.enable-execution-cache", default_value_t = false)]
    pub enable_execution_cache: bool,
}
