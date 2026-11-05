//! Chain-specific metadata updates for RPC overrides.

use alloc::string::String;
use alloy_rpc_types_eth::{state::StateOverride, BlockOverrides};

/// Chain-specific metadata updates after applying RPC block or state overrides.
///
/// Implement this trait for the full EVM environment so request-local metadata may own
/// collections. evm2's opcode-visible block extension must remain `Copy`.
/// Errors are returned to the RPC caller as invalid parameters.
pub trait BlockOverridesExt {
    /// Called after the standard block fields are overridden, with the
    /// same overrides. Implementations may adjust derived state or reject invalid values.
    fn apply_block_overrides_ext(&mut self, overrides: &BlockOverrides) -> Result<(), String>;

    /// Updates request-local execution metadata before applying state overrides.
    ///
    /// Must be idempotent: batched calls may apply the same overrides more than once.
    fn apply_state_overrides_ext(&mut self, _overrides: &StateOverride) -> Result<(), String> {
        Ok(())
    }
}
