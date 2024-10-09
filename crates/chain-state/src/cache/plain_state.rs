use lazy_static::lazy_static;
use quick_cache::sync::Cache;
use reth_primitives::{Account, Address, Bytecode, StorageKey, StorageValue, B256, U256};
use reth_revm::db::{BundleState, OriginalValuesKnown};

// Cache sizes
const ACCOUNT_CACHE_SIZE: usize = 1000000;
const STORAGE_CACHE_SIZE: usize = ACCOUNT_CACHE_SIZE * 10;
const CONTRACT_CACHE_SIZE: usize = ACCOUNT_CACHE_SIZE / 10;

// Type alias for address and storage key tuple
type AddressStorageKey = (Address, StorageKey);

lazy_static! {
    /// Account cache
    pub(crate) static ref PLAIN_ACCOUNTS: Cache<Address, Account> = Cache::new(ACCOUNT_CACHE_SIZE);

    /// Storage cache
     pub(crate) static ref PLAIN_STORAGES: Cache<AddressStorageKey, StorageValue> = Cache::new(STORAGE_CACHE_SIZE);

    /// Contract cache
    /// The size of contract is large and the hot contracts should be limited.
     pub(crate) static ref CONTRACT_CODES: Cache<B256, Bytecode> = Cache::new(CONTRACT_CACHE_SIZE);
}

pub(crate) fn insert_account(k: Address, v: Account) {
    PLAIN_ACCOUNTS.insert(k, v);
}

/// Insert storage into the cache
pub(crate) fn insert_storage(k: AddressStorageKey, v: U256) {
    PLAIN_STORAGES.insert(k, v);
}

// Get account from cache
pub(crate) fn get_account(k: &Address) -> Option<Account> {
    PLAIN_ACCOUNTS.get(k)
}

// Get storage from cache
pub(crate) fn get_storage(k: &AddressStorageKey) -> Option<StorageValue> {
    PLAIN_STORAGES.get(k)
}

// Get code from cache
pub(crate) fn get_code(k: &B256) -> Option<Bytecode> {
    CONTRACT_CODES.get(k)
}

// Insert code into cache
pub(crate) fn insert_code(k: B256, v: Bytecode) {
    CONTRACT_CODES.insert(k, v);
}

/// Write committed state to cache.
pub(crate) fn write_plain_state(bundle: BundleState) {
    let change_set = bundle.into_plain_state(OriginalValuesKnown::Yes);

    // Update account cache
    for (address, account_info) in &change_set.accounts {
        match account_info {
            None => {
                PLAIN_ACCOUNTS.remove(address);
            }
            Some(acc) => {
                PLAIN_ACCOUNTS.insert(
                    *address,
                    Account {
                        nonce: acc.nonce,
                        balance: acc.balance,
                        bytecode_hash: Some(acc.code_hash),
                    },
                );
            }
        }
    }

    // Update storage cache
    let mut should_wipe = false;
    for storage in &change_set.storage {
        if storage.wipe_storage {
            should_wipe = true;
            break
        }

        for (k, v) in storage.storage.clone() {
            insert_storage((storage.address, StorageKey::from(k)), v);
        }
    }
    if should_wipe {
        PLAIN_STORAGES.clear();
    }
}

/// Clear cached accounts and storages.
pub(crate) fn clear_plain_state() {
    PLAIN_ACCOUNTS.clear();
    PLAIN_STORAGES.clear();
}
