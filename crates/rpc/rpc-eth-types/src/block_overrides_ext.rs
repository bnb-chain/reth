//! RPC override hooks for chain-specific block environment extensions.

use alloy_rpc_types_eth::{state::StateOverride, BlockOverrides};
use evm2::env::BlockEnvExt;

/// Chain-specific metadata updates after applying RPC block or state overrides.
///
/// Implement this trait for a chain's block extension. The generic block environment
/// delegates to that extension so standard fields and derived metadata stay consistent.
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

impl BlockOverridesExt for () {
    /// The standard block environment derives nothing from the overrides beyond what
    /// `apply_block_overrides` already wrote, so there is nothing to fix up.
    fn apply_block_overrides_ext(&mut self, _overrides: &BlockOverrides) -> Result<(), String> {
        Ok(())
    }
}

impl<E: BlockOverridesExt> BlockOverridesExt for BlockEnvExt<E> {
    fn apply_block_overrides_ext(&mut self, overrides: &BlockOverrides) -> Result<(), String> {
        self.ext.apply_block_overrides_ext(overrides)
    }

    fn apply_state_overrides_ext(&mut self, overrides: &StateOverride) -> Result<(), String> {
        self.ext.apply_state_overrides_ext(overrides)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use evm2::env::BlockEnv;

    #[test]
    fn block_env_hook_is_a_noop() {
        let mut env = BlockEnv::<evm2::BaseEvmTypes>::default();
        let before = env;
        let overrides = BlockOverrides {
            time: Some(12_345),
            random: Some(B256::from([0x42; 32])),
            ..Default::default()
        };
        // Accepts any overrides and leaves the environment untouched.
        assert_eq!(env.apply_block_overrides_ext(&overrides), Ok(()));
        assert_eq!(env.apply_state_overrides_ext(&StateOverride::default()), Ok(()));
        assert_eq!(env, before);
    }

    #[test]
    fn hook_errors_propagate() {
        // A custom environment can reject override values; the error is surfaced verbatim.
        struct Rejecting;
        impl BlockOverridesExt for Rejecting {
            fn apply_block_overrides_ext(
                &mut self,
                _overrides: &BlockOverrides,
            ) -> Result<(), String> {
                Err("bad override".to_string())
            }
        }
        assert_eq!(
            Rejecting.apply_block_overrides_ext(&BlockOverrides::default()),
            Err("bad override".to_string())
        );
    }
}
