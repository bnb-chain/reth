use alloy_primitives::{Address, B256};
use std::ffi::{c_int, c_uchar};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BscWrapperError {
    #[error("Failed to create BSC StateTrie: {0}")]
    CreationFailed(String),
    #[error("Failed to update account: {0}")]
    UpdateAccountFailed(String),
    #[error("Failed to update storage: {0}")]
    UpdateStorageFailed(String),
    #[error("Failed to delete account: {0}")]
    DeleteAccountFailed(String),
    #[error("Failed to delete storage: {0}")]
    DeleteStorageFailed(String),
    #[error("Failed to commit: {0}")]
    CommitFailed(String),
    #[error("Failed to get root: {0}")]
    GetRootFailed(String),
    #[error("FFI error: {0}")]
    FFIError(String),
}

// FFI function declarations
#[link(name = "bsc_trie")]
extern "C" {
    fn bsc_new_state_trie(state_root: *const c_uchar, db_path: *const c_uchar) -> c_int;
    fn bsc_update_account(
        tr: c_int,
        address: *const c_uchar,
        nonce: u64,
        balance: *const c_uchar,
        storage_root: *const c_uchar,
        code_hash: *const c_uchar,
    ) -> c_int;
    fn bsc_update_storage(
        tr: c_int,
        address: *const c_uchar,
        key: *const c_uchar,
        key_len: c_int,
        value: *const c_uchar,
        value_len: c_int,
    ) -> c_int;
    fn bsc_delete_account(tr: c_int, address: *const c_uchar) -> c_int;
    fn bsc_delete_storage(tr: c_int, address: *const c_uchar, key: *const c_uchar, key_len: c_int) -> c_int;
    fn bsc_commit(tr: c_int, collect_leaf: c_int, root: *mut c_uchar, node_set: *mut *mut c_uchar) -> c_int;
    fn bsc_get_root(tr: c_int, root: *mut c_uchar) -> c_int;
    fn bsc_free_state_trie(tr: c_int) -> c_int;
}

/// BSC StateTrie wrapper
pub struct BscStateTrie {
    trie_id: c_int,
}

impl BscStateTrie {
    /// Create a new BSC StateTrie instance
    pub fn new(state_root: B256, db_path: &str) -> Result<Self, BscWrapperError> {
        let result = unsafe {
            bsc_new_state_trie(
                state_root.as_ptr(),
                db_path.as_ptr(),
            )
        };

        if result <= 0 {
            return Err(BscWrapperError::CreationFailed(
                "Failed to create BSC StateTrie".to_string(),
            ));
        }

        Ok(Self { trie_id: result })
    }

    /// Update account in the trie
    pub fn update_account(
        &mut self,
        address: Address,
        nonce: u64,
        balance: B256,
        storage_root: B256,
        code_hash: B256,
    ) -> Result<(), BscWrapperError> {
        let result = unsafe {
            bsc_update_account(
                self.trie_id,
                address.as_ptr(),
                nonce,
                balance.as_ptr(),
                storage_root.as_ptr(),
                code_hash.as_ptr(),
            )
        };

        if result != 0 {
            return Err(BscWrapperError::UpdateAccountFailed(
                "BSC update_account failed".to_string(),
            ));
        }

        Ok(())
    }

    /// Update storage in the trie
    pub fn update_storage(
        &mut self,
        address: Address,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), BscWrapperError> {
        let result = unsafe {
            bsc_update_storage(
                self.trie_id,
                address.as_ptr(),
                key.as_ptr(),
                key.len() as c_int,
                value.as_ptr(),
                value.len() as c_int,
            )
        };

        if result != 0 {
            return Err(BscWrapperError::UpdateStorageFailed(
                "BSC update_storage failed".to_string(),
            ));
        }

        Ok(())
    }

    /// Delete account from the trie
    pub fn delete_account(&mut self, address: Address) -> Result<(), BscWrapperError> {
        let result = unsafe { bsc_delete_account(self.trie_id, address.as_ptr()) };

        if result != 0 {
            return Err(BscWrapperError::DeleteAccountFailed(
                "BSC delete_account failed".to_string(),
            ));
        }

        Ok(())
    }

    /// Delete storage from the trie
    pub fn delete_storage(&mut self, address: Address, key: &[u8]) -> Result<(), BscWrapperError> {
        let result = unsafe {
            bsc_delete_storage(self.trie_id, address.as_ptr(), key.as_ptr(), key.len() as c_int)
        };

        if result != 0 {
            return Err(BscWrapperError::DeleteStorageFailed(
                "BSC delete_storage failed".to_string(),
            ));
        }

        Ok(())
    }

    /// Commit the trie and get the root hash
    pub fn commit(&mut self, collect_leaf: bool) -> Result<B256, BscWrapperError> {
        let mut root_bytes = [0u8; 32];
        let mut node_set_ptr = std::ptr::null_mut();

        let result = unsafe {
            bsc_commit(
                self.trie_id,
                if collect_leaf { 1 } else { 0 },
                root_bytes.as_mut_ptr(),
                &mut node_set_ptr,
            )
        };

        if result != 0 {
            return Err(BscWrapperError::CommitFailed(
                "BSC commit failed".to_string(),
            ));
        }

        Ok(B256::from_slice(&root_bytes))
    }

    /// Get the current root hash without committing
    pub fn root(&self) -> Result<B256, BscWrapperError> {
        let mut root_bytes = [0u8; 32];

        let result = unsafe { bsc_get_root(self.trie_id, root_bytes.as_mut_ptr()) };

        if result != 0 {
            return Err(BscWrapperError::GetRootFailed(
                "BSC get_root failed".to_string(),
            ));
        }

        Ok(B256::from_slice(&root_bytes))
    }
}

impl Drop for BscStateTrie {
    fn drop(&mut self) {
        unsafe {
            bsc_free_state_trie(self.trie_id);
        }
    }
}
