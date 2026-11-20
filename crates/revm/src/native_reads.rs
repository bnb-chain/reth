//! Explicit native-precompile reads for synchronous tracing.
//!
//! Native precompiles only receive `EvmInternals`, which cannot access the inspector. A scoped
//! thread-local bridge keeps their observations independent of journal rollback. Never hold a
//! capture across an await or move execution to another thread.

use alloy_primitives::{map::AddressMap, Address, U256};
use std::{cell::RefCell, collections::BTreeSet};

/// Accounts and storage slots read during one synchronous execution, including reverted frames.
#[derive(Debug, Default)]
pub struct NativeReads {
    /// Each observed account and its observed storage keys; an empty set means an account read.
    pub accounts: AddressMap<BTreeSet<U256>>,
}

thread_local! {
    static READS: RefCell<Option<NativeReads>> = const { RefCell::new(None) };
}

struct CaptureGuard(Option<NativeReads>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        READS.with(|slot| slot.replace(self.0.take()));
    }
}

/// Runs synchronous execution with a fresh observation scope when enabled.
///
/// Nested scopes are isolated and the previous scope is restored on return or panic. Disabled
/// scopes suppress recording, including when nested inside an enabled scope.
pub fn capture<T>(enabled: bool, execute: impl FnOnce() -> T) -> (T, NativeReads) {
    let guard = CaptureGuard(READS.with(|slot| slot.replace(enabled.then(NativeReads::default))));
    let result = execute();
    let reads = READS.with(|slot| slot.take()).unwrap_or_default();
    drop(guard);
    (result, reads)
}

/// Records a native account read before accessing or changing the account. Outside a capture this
/// is a no-op. The observation survives a subsequent EVM revert.
pub fn account(address: Address) {
    READS.with(|slot| {
        if let Some(reads) = slot.borrow_mut().as_mut() {
            reads.accounts.entry(address).or_default();
        }
    });
}

/// Records a native storage read (including the read preceding a write). Outside a capture this
/// is a no-op. The observation survives a subsequent EVM revert.
pub fn storage(address: Address, key: U256) {
    READS.with(|slot| {
        if let Some(reads) = slot.borrow_mut().as_mut() {
            reads.accounts.entry(address).or_default().insert(key);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_restore_after_panic_and_isolate_requests_and_threads() {
        let address = Address::repeat_byte(1);
        account(address);
        let (_, outer) = capture(true, || {
            storage(address, U256::from(1));
            let (_, disabled) = capture(false, || account(Address::ZERO));
            assert!(disabled.accounts.is_empty());
            let (_, nested) = capture(true, || account(Address::ZERO));
            assert_eq!(nested.accounts.keys().copied().collect::<Vec<_>>(), [Address::ZERO]);
            assert!(std::panic::catch_unwind(|| capture(true, || {
                storage(address, U256::from(2));
                panic!("cancelled inspection");
            }))
            .is_err());
            std::thread::spawn(move || account(Address::ZERO)).join().unwrap();
            storage(address, U256::from(3));
        });
        assert_eq!(
            outer.accounts,
            std::iter::once((address, [U256::from(1), U256::from(3)].into())).collect()
        );
        assert!(capture(true, || ()).1.accounts.is_empty());
    }
}
