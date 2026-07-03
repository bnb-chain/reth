//! Global fastnode-mode flag.
//!
//! Lives in `reth-chain-state` (rather than `reth-engine-primitives`, where it is
//! re-exported from) so that storage-layer crates such as `reth-provider` can
//! consult it without depending on engine crates.

use core::sync::atomic::{AtomicBool, Ordering};

/// Global flag indicating whether the node is running in fastnode mode
/// (i.e., `--engine.skip-state-root-validation` is enabled).
///
/// When active, trie-dependent RPC methods such as `eth_getProof` and `eth_getAccount`
/// are unavailable because the hashing stages and state root computation are skipped,
/// meaning the trie tables in the database are not kept up to date.
static FASTNODE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Activate fastnode mode globally.
///
/// This should be called once during node startup when `skip_state_root_validation` is enabled.
pub fn activate_fastnode() {
    FASTNODE_ACTIVE.store(true, Ordering::SeqCst);
}

/// Returns `true` if the node is running in fastnode mode.
pub fn is_fastnode_active() -> bool {
    FASTNODE_ACTIVE.load(Ordering::SeqCst)
}
