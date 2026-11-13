//! State conversion at the evm2 execution boundary.

use alloy_primitives::{keccak256, B256};
use evm2::evm::AccountInfo;
use reth_primitives_traits::Account;
use reth_trie_common::{HashedPostState, HashedStorage};

pub use evm2::block::{AccountUpdate, BlockState, BundleSource, StateUpdate, TransactionChanges};

/// Hashes a transaction's state update into the trie update consumed by state root tasks.
pub fn state_update_to_hashed_post_state(update: &StateUpdate) -> HashedPostState {
    let mut hashed_state = HashedPostState::with_capacity(update.accounts.len());
    for account in &update.accounts {
        // No persisted account exists on either side of this transaction. In particular,
        // create-then-delete and zero-value calls to missing accounts produce no bundle write.
        if account.original.is_none() && account.current.is_none() {
            continue;
        }
        let hashed_address = keccak256(account.address);
        let destroyed = account.current.is_none();
        let info = account.current.clone().unwrap_or_default();
        let unchanged = account
            .original
            .as_ref()
            .map_or_else(|| info == AccountInfo::default(), |original| info == *original);
        // EIP-161: a touched account that ends up empty is deleted, unless it never existed.
        let deleted = destroyed || (info.is_empty() && account.original.is_some());
        if deleted {
            hashed_state.accounts.insert(hashed_address, None);
        } else if !unchanged {
            hashed_state.accounts.insert(hashed_address, Some(Account::from(&info)));
        }
        if !deleted && !account.storage.is_empty() {
            hashed_state.storages.insert(
                hashed_address,
                HashedStorage::from_iter(
                    account
                        .storage
                        .iter()
                        .map(|(key, slot)| (keccak256(B256::from(*key)), slot.present_value)),
                ),
            );
        }
    }
    hashed_state
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use evm2::{
        block::bundle_state::StorageSlot,
        evm::{
            AccountChangeRef, AccountChanges, StateChangeSink, StorageChange, StorageOverlay,
            StorageSlot as NativeSlot, Tracked,
        },
    };

    #[test]
    fn emptied_account_does_not_emit_storage() {
        // BSC system execution can drain an account without SELFDESTRUCT. The streamed
        // update must not resurrect its storage after marking the account for deletion.
        let address = Address::with_last_byte(3);
        let update = StateUpdate {
            accounts: vec![AccountUpdate {
                address,
                original: Some(AccountInfo { balance: U256::ONE, ..Default::default() }),
                current: Some(AccountInfo::default()),
                created: false,
                wiped: false,
                storage: vec![(U256::ONE, StorageSlot::new_changed(U256::ZERO, U256::from(2)))],
            }],
        };
        let hashed = state_update_to_hashed_post_state(&update);
        assert_eq!(hashed.accounts.get(&keccak256(address)), Some(&None));
        assert!(!hashed.storages.contains_key(&keccak256(address)));
    }

    #[test]
    fn streamed_accounts_match_bundle_after_native_updates() {
        let address = Address::with_last_byte(4);
        let funded = AccountInfo { balance: U256::ONE, ..Default::default() };
        let updated = AccountInfo { balance: U256::from(2), ..Default::default() };
        let empty = AccountInfo::default();
        // Native evm2 emits None when an EIP-161-empty account is deleted. Cover ordinary
        // deletion, touching an existing empty account, creation/deletion, and unchanged state.
        for (original, current, created, selfdestructed) in [
            (Some(&funded), None, false, false),
            (Some(&empty), None, false, false),
            (None, None, true, true),
            (Some(&funded), None, false, true),
            (Some(&funded), Some(&updated), false, false),
            (Some(&funded), Some(&funded), false, false),
            (None, None, false, false),
        ] {
            let mut block = BlockState::new();
            let mut update = StateUpdate::default();
            block.commit_account_changes(
                AccountChanges {
                    address,
                    original,
                    current,
                    created,
                    selfdestructed,
                    touched: true,
                    code: None,
                    storage: &StorageOverlay::default(),
                },
                Some(&mut update),
            );
            let bundle = block.into_bundle();
            let streamed = state_update_to_hashed_post_state(&update);
            let persisted = HashedPostState::from_bundle_state::<reth_trie_common::KeccakKeyHasher>(
                bundle.state.iter(),
            );
            assert_eq!(
                streamed.accounts, persisted.accounts,
                "account mismatch: {original:?} -> {current:?}"
            );
            assert_eq!(
                streamed.storages, persisted.storages,
                "storage mismatch: {original:?} -> {current:?}"
            );
        }
    }

    /// Builds a storage overlay from `(key, original, current)` slots.
    fn overlay(wiped: bool, slots: &[(U256, U256, U256)]) -> StorageOverlay {
        let mut storage = StorageOverlay { wiped, ..Default::default() };
        for &(key, original, current) in slots {
            let slot =
                NativeSlot { value: Tracked::from_parts(original, current), ..Default::default() };
            storage.slots.insert(key, slot);
        }
        storage
    }

    #[cfg(feature = "account-ext")]
    #[test]
    fn account_extensions_survive_native_updates_and_reverts() {
        let address = Address::with_last_byte(1);
        let original = AccountInfo {
            balance: U256::from(10),
            extension: evm2::evm::AccountExtension::copy_from_slice(b"original extension"),
            ..Default::default()
        };
        let mut updated = original.clone();
        updated.balance = U256::from(20);
        updated.extension = evm2::evm::AccountExtension::copy_from_slice(b"updated extension");
        let mut state = BlockState::new();
        state.commit_account_changes(
            AccountChanges {
                address,
                original: Some(&original),
                current: Some(&updated),
                created: false,
                selfdestructed: false,
                touched: false,
                code: None,
                storage: &StorageOverlay::default(),
            },
            None,
        );
        let mut bundle = state.into_bundle();
        let account = bundle.state.get(&address).unwrap();
        assert_eq!(
            account.original_info.as_ref().unwrap().extension.as_ref(),
            b"original extension"
        );
        assert_eq!(account.info.as_ref().unwrap().extension.as_ref(), b"updated extension");
        assert!(bundle.revert_latest());
        assert_eq!(
            bundle.state.get(&address).unwrap().info.as_ref().unwrap().extension.as_ref(),
            b"original extension"
        );
    }

    #[test]
    fn native_sink_preserves_storage_lifetimes_and_reverts() {
        let address = Address::with_last_byte(1);
        let code = evm2::bytecode::Bytecode::new_raw(alloy_primitives::bytes!("60015b00"));
        let info = AccountInfo::default().with_nonce(1).with_code(code.clone());
        for steps in 1..=7 {
            let mut converted = BlockState::new();
            let mut native = BlockState::new();
            // Storage-only writes, restoration, deletion, recreation, a surviving storage wipe,
            // and another deletion exercise both occupied and vacant transition entries.
            for step in 0..steps {
                let slots: &[_] = if step < 2 || step == 3 || step == 4 {
                    let (original, current) = if step == 1 { (8, 7) } else { (7, 8) };
                    &[(U256::from(3), U256::from(original), U256::from(current))]
                } else {
                    &[]
                };
                let storage = overlay(step >= 2, slots);
                let changes = if step < 2 {
                    AccountChanges {
                        address,
                        original: Some(&info),
                        current: Some(&info),
                        created: false,
                        selfdestructed: false,
                        touched: false,
                        code: None,
                        storage: &storage,
                    }
                } else {
                    AccountChanges {
                        address,
                        original: (step != 3 && step != 6).then_some(&info),
                        current: (step == 3 || step == 4).then_some(&info),
                        created: step == 3,
                        selfdestructed: step != 3,
                        touched: false,
                        code: (step == 3).then(|| (code.hash_slow(), &code)),
                        storage: &storage,
                    }
                };
                let empty = StorageOverlay::default();
                // Reads alone must never introduce an account transition.
                let read = AccountChanges {
                    address: Address::with_last_byte(2),
                    original: Some(&info),
                    current: Some(&info),
                    created: false,
                    selfdestructed: false,
                    touched: false,
                    code: None,
                    storage: &empty,
                };
                let mut transaction = TransactionChanges::default();
                let Ok(()) = changes.visit(&mut transaction);
                let Ok(()) = read.visit(&mut transaction);
                converted.commit(&transaction);
                native.commit_account_changes(changes, None);
                native.commit_account_changes(read, None);
            }
            let mut native = native.into_bundle();
            let mut converted = converted.into_bundle();
            assert_eq!(native, converted, "after {steps} steps");
            assert!(native.revert_latest());
            assert!(converted.revert_latest());
            assert_eq!(native, converted, "after {steps} steps");
        }
    }

    #[test]
    fn cached_bytecode_does_not_change_bundle_output() {
        let address = Address::with_last_byte(1);
        let code = evm2::bytecode::Bytecode::new_raw(alloy_primitives::bytes!("60015b00"));
        let info = AccountInfo {
            balance: U256::from(5),
            code_hash: code.hash_slow(),
            ..Default::default()
        };
        let bundle = |info: &AccountInfo| {
            let mut changes = TransactionChanges::default();
            changes
                .account(AccountChangeRef {
                    address,
                    original: None,
                    current: Some(info),
                    created: false,
                    selfdestructed: false,
                })
                .unwrap();
            let mut block = BlockState::new();
            block.commit(&changes);
            block.into_bundle()
        };
        let uncached = bundle(&info);
        let cached = bundle(&AccountInfo { code: Some(code), ..info });
        assert_eq!(uncached, cached);
        assert!(cached.contracts.is_empty());
    }

    #[test]
    fn deployed_bytecode_is_retained_separately_from_account_metadata() {
        let address = Address::with_last_byte(1);
        let code = evm2::bytecode::Bytecode::new_raw(alloy_primitives::bytes!("60015b00"));
        let hash = code.hash_slow();
        let info = AccountInfo {
            nonce: 1,
            code_hash: hash,
            code: Some(code.clone()),
            ..Default::default()
        };
        let mut changes = TransactionChanges::default();
        changes.bytecode(hash, &code).unwrap();
        changes
            .account(AccountChangeRef {
                address,
                original: None,
                current: Some(&info),
                created: true,
                selfdestructed: false,
            })
            .unwrap();
        let mut block = BlockState::new();
        block.commit(&changes);
        let bundle = block.into_bundle();
        assert_eq!(bundle.contracts[&hash].original_bytes(), code.original_bytes());
        assert_eq!(bundle.account(&address).unwrap().info.as_ref().unwrap().code_hash, hash);
    }

    #[test]
    fn storage_only_change_retains_account_and_reverts() {
        let address = Address::with_last_byte(1);
        let info = AccountInfo { nonce: 1, ..Default::default() };
        let mut changes = TransactionChanges::default();
        changes
            .storage(StorageChange {
                address,
                key: U256::from(3),
                original: U256::from(7),
                current: U256::from(8),
            })
            .unwrap();
        changes.account_read(address, Some(&info)).unwrap();
        let mut block = BlockState::new();
        block.commit(&changes);
        let mut bundle = block.into_bundle();
        assert_eq!(bundle.account(&address).unwrap().info.as_ref().unwrap().nonce, 1);
        assert_eq!(bundle.storage(&address, U256::from(3)), Some(U256::from(8)));
        assert_eq!(bundle.reverts.len(), 1);
        assert!(bundle.revert_latest());
        assert_eq!(bundle.storage(&address, U256::from(3)), Some(U256::from(7)));
    }

    #[test]
    fn destroy_then_recreate_retains_parent_storage_wipe() {
        let address = Address::with_last_byte(1);
        let original = AccountInfo { nonce: 1, balance: U256::from(7), ..Default::default() };
        let recreated = AccountInfo { nonce: 1, balance: U256::from(9), ..Default::default() };
        let mut block = BlockState::new();
        let mut destroyed = TransactionChanges::default();
        destroyed
            .account(AccountChangeRef {
                address,
                original: Some(&original),
                current: None,
                created: false,
                selfdestructed: true,
            })
            .unwrap();
        block.commit(&destroyed);
        let mut created = TransactionChanges::default();
        created.storage_wipe(address).unwrap();
        created
            .storage(StorageChange {
                address,
                key: U256::from(3),
                original: U256::ZERO,
                current: U256::from(8),
            })
            .unwrap();
        created
            .account(AccountChangeRef {
                address,
                original: None,
                current: Some(&recreated),
                created: true,
                selfdestructed: false,
            })
            .unwrap();
        block.commit(&created);
        let bundle = block.into_bundle();
        let account = bundle.account(&address).unwrap();
        assert!(account.was_destroyed());
        assert_eq!(account.original_info.as_ref().unwrap().balance, U256::from(7));
        assert_eq!(account.info.as_ref().unwrap().balance, U256::from(9));
        assert_eq!(account.storage_slot(U256::from(4)), Some(U256::ZERO));
        assert_eq!(account.storage_slot(U256::from(3)), Some(U256::from(8)));
        assert!(bundle.reverts[0][0].1.wipe_storage);
    }
}
