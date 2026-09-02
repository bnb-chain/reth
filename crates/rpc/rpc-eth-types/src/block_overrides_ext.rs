//! Post-override hook for chain-specific block environment extensions.

use alloy_rpc_types_eth::BlockOverrides;
use revm::context::BlockEnv;

/// Chain-specific fix-up applied after [`apply_block_overrides`] has written a set of
/// RPC block overrides into a block environment.
///
/// `apply_block_overrides` only knows the standard [`BlockEnv`] fields it mutates through
/// [`BlockEnvironment::inner_mut`]. A custom block environment that derives additional
/// state from those fields (e.g. a chain that packs a sub-second timestamp remainder next
/// to the seconds) needs to observe the overrides to keep that state consistent, and may
/// reject override values that are invalid under its chain rules.
///
/// This trait is deliberately **not** blanket-implemented: every block environment type
/// used with the RPC call helpers provides its own implementation, so downstream
/// environments remain free to attach real semantics. The stock [`BlockEnv`] used by
/// Ethereum nodes implements it as a no-op.
///
/// Errors are surfaced to the RPC caller as invalid-params errors.
///
/// [`apply_block_overrides`]: alloy_evm::overrides::apply_block_overrides
/// [`BlockEnvironment::inner_mut`]: alloy_evm::env::BlockEnvironment::inner_mut
pub trait BlockOverridesExt {
    /// Called right after `apply_block_overrides(overrides, db, self.inner_mut())` with the
    /// same overrides. Implementations may adjust derived state or reject invalid values.
    fn apply_block_overrides_ext(&mut self, overrides: &BlockOverrides) -> Result<(), String>;
}

impl BlockOverridesExt for BlockEnv {
    /// The standard block environment derives nothing from the overrides beyond what
    /// `apply_block_overrides` already wrote, so there is nothing to fix up.
    fn apply_block_overrides_ext(&mut self, _overrides: &BlockOverrides) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn block_env_hook_is_a_noop() {
        let mut env = BlockEnv::default();
        let before = env.clone();
        let overrides = BlockOverrides {
            time: Some(12_345),
            random: Some(B256::from([0x42; 32])),
            ..Default::default()
        };
        // Accepts any overrides and leaves the environment untouched.
        assert_eq!(env.apply_block_overrides_ext(&overrides), Ok(()));
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
