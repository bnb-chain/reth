use crate::bsc_wrapper::BscStateTrie;
use alloy_primitives::{Address, B256, U256};
use rand::Rng;
// use reth_primitives_traits::Account;
use reth_triedb_pathdb::{PathDB, PathProviderConfig};
use reth_triedb_state_trie::{SecureTrieId, SecureTrieTrait, account::{StateAccount}};
use reth_triedb_state_trie::state_trie::StateTrie;
use std::collections::HashMap;
use tempfile::TempDir;
use thiserror::Error;
use tracing::info;

#[derive(Error, Debug)]
pub enum SmokeTestError {
    #[error("BSC wrapper error: {0}")]
    BscWrapperError(#[from] crate::bsc_wrapper::BscWrapperError),
    #[error("Failed to create temporary directory: {0}")]
    TempDirError(#[from] std::io::Error),
    #[error("PathDB error: {0}")]
    PathDBError(#[from] reth_triedb_pathdb::PathProviderError),
    #[error("StateTrie error: {0}")]
    StateTrieError(#[from] reth_triedb_state_trie::SecureTrieError),
    #[error("Root mismatch: BSC={0:?}, Reth={1:?}")]
    RootMismatch(B256, B256),
}

/// Smoke test result
#[derive(Debug)]
pub struct SmokeTestResult {
    pub success: bool,
    pub bsc_root: Option<B256>,
    pub reth_root: Option<B256>,
    pub errors: Vec<String>,
}

/// Smoke test structure
pub struct SmokeTest {
    bsc_trie: BscStateTrie,
    reth_trie: StateTrie<PathDB>,
    /// Temporary directory for Reth trie database.
    /// This field is kept alive to ensure the temp directory is not deleted until the struct is dropped.
    #[allow(dead_code)]
    temp_dir: TempDir,
    accounts: HashMap<Address, StateAccount>,
    #[allow(dead_code)]
    storage: HashMap<Address, HashMap<B256, U256>>,
}

impl SmokeTest {
    /// Create a new smoke test instance
    pub fn new() -> Result<Self, SmokeTestError> {
        info!("Starting smoke test...");

        // Create temporary directory for Reth trie
        let temp_dir = TempDir::new()?;
        let db_path = temp_dir.path().to_str().unwrap();

        // Initialize BSC StateTrie
        let bsc_trie = BscStateTrie::new(B256::ZERO, "/tmp/bsc_test_db")?;
        info!("BSC StateTrie initialized successfully");

        // Initialize Reth StateTrie
        let path_db = PathDB::new(db_path, PathProviderConfig::default())?;
        let id = SecureTrieId::default();
        let reth_trie = StateTrie::new(id, path_db)?;
        info!("Reth StateTrie initialized successfully");

        Ok(Self {
            bsc_trie,
            reth_trie,
            temp_dir,
            accounts: HashMap::new(),
            storage: HashMap::new(),
        })
    }

    /// Run the smoke test
    pub fn run_smoke_test(&mut self) -> SmokeTestResult {
        let mut result = SmokeTestResult {
            success: true,
            bsc_root: None,
            reth_root: None,
            errors: Vec::new(),
        };

        let mut rng = rand::thread_rng();
        let total_operations = 1000000;
        let commit_interval = 100000;
        let delete_ratio = 0.5;

        println!("🚀 Starting smoke test with {} total operations", total_operations);
        for i in 0..total_operations {
            let (address, account) = generate_random_address_and_account(&mut rng);
            self.accounts.insert(address, account.clone());

            // Update BSC trie
            if let Err(e) = self.bsc_trie.update_account(
                address,
                account.nonce.try_into().unwrap_or(0),
                account.balance.to_be_bytes().into(),
                account.storage_root,
                account.code_hash,
            ) {
                result.errors.push(format!("BSC update_account failed: {}", e));
                result.success = false;
            }

            // Update Reth trie
            if let Err(e) = self.reth_trie.update_account(address, &account) {
                result.errors.push(format!("Reth update_account failed: {}", e));
                result.success = false;
            }

            // Commit and compare every commit_interval operations
            if (i + 1) % commit_interval == 0 {
                let progress = format!("[{}/{}]", i + 1, total_operations);
                if let Err(e) = self.get_root_and_compare(&mut result, &progress, "Insert") {
                    result.errors.push(e);
                    result.success = false;
                }
            }
        }

        let delete_count = (total_operations as f64 * delete_ratio) as usize;
        // Collect addresses and storage keys for deletion
        let addresses: Vec<Address> = self.accounts.keys().cloned().collect();
        for i in 0..delete_count {    // Delete account
            let address = addresses[i];
            self.accounts.remove(&address);

            // Delete from BSC trie
            if let Err(e) = self.bsc_trie.delete_account(address) {
                result.errors.push(format!("BSC delete_account failed: {}", e));
                result.success = false;
            }

            // Delete from Reth trie
            if let Err(e) = self.reth_trie.delete_account(address) {
                result.errors.push(format!("Reth delete_account failed: {}", e));
                result.success = false;
            }

            // Compare roots every 5 delete operations
            if (i + 1) % commit_interval == 0 {
                let progress = format!("[{}/{}]", i + 1, delete_count);
                if let Err(e) = self.get_root_and_compare(&mut result, &progress, "Delete") {
                    result.errors.push(e);
                    result.success = false;
                }
            }
        }

        result
    }

    pub fn run_storage_smoke_test(&mut self) -> SmokeTestResult {
        let mut result = SmokeTestResult {
            success: true,
            bsc_root: None,
            reth_root: None,
            errors: Vec::new(),
        };

        let mut rng = rand::thread_rng();
        let total_operations = 1000000;
        let commit_interval = 100000;
        let delete_ratio = 0.5;

        println!("🚀 Starting storage smoke test with {} total operations", total_operations);
        for i in 0..total_operations {
            let (address, account) = generate_random_address_and_account(&mut rng);
            self.accounts.insert(address, account.clone());

            // Update BSC trie
            if let Err(e) = self.bsc_trie.update_storage(
                address,
                address.as_slice(),
                address.as_slice(),
            ) {
                result.errors.push(format!("BSC update_storage failed: {}", e));
                result.success = false;
            }

            // Update Reth trie
            if let Err(e) = self.reth_trie.update_storage(
                address,
                address.as_slice(),
                address.as_slice(),
            ) {
                result.errors.push(format!("Reth update_storage failed: {}", e));
                result.success = false;
            }

            // Commit and compare every commit_interval operations
            if (i + 1) % commit_interval == 0 {
                let progress = format!("[{}/{}]", i + 1, total_operations);
                if let Err(e) = self.get_root_and_compare(&mut result, &progress, "Insert") {
                    result.errors.push(e);
                    result.success = false;
                }
            }
        }

        let delete_count = (total_operations as f64 * delete_ratio) as usize;
        // Collect addresses and storage keys for deletion
        let addresses: Vec<Address> = self.accounts.keys().cloned().collect();
        for i in 0..delete_count {    // Delete account
            let address = addresses[i];
            self.accounts.remove(&address);

            // Delete from BSC trie
            if let Err(e) = self.bsc_trie.delete_storage(address, address.as_slice()) {
                result.errors.push(format!("BSC delete_storage failed: {}", e));
                result.success = false;
            }

            // Delete from Reth trie
            if let Err(e) = self.reth_trie.delete_storage(address, address.as_slice()) {
                result.errors.push(format!("Reth delete_storage failed: {}", e));
                result.success = false;
            }

            // Compare roots every 5 delete operations
            if (i + 1) % commit_interval == 0 {
                let progress = format!("[{}/{}]", i + 1, delete_count);
                if let Err(e) = self.get_root_and_compare(&mut result, &progress, "Delete") {
                    result.errors.push(e);
                    result.success = false;
                }
            }
        }

        result
    }

    /// Get root and compare BSC and Reth roots
    fn get_root_and_compare(&mut self, result: &mut SmokeTestResult, progress: &str, operation_type: &str) -> Result<(), String> {
        // Get BSC root
        let bsc_root = self.bsc_trie.root().map_err(|e| format!("BSC root failed: {}", e))?;

        // Get Reth root
        let reth_root = self.reth_trie.hash();

        // Store roots in result
        result.bsc_root = Some(bsc_root);
        result.reth_root = Some(reth_root);

        // Compare roots
        if bsc_root == reth_root {
            println!("✅ {} {} - Root match: BSC={:?}, Reth={:?}", progress, operation_type, bsc_root, reth_root);
        } else {
            let error_msg = format!("{} {} - Root mismatch: BSC={:?}, Reth={:?}", progress, operation_type, bsc_root, reth_root);
            println!("❌ {}", error_msg);
            // Don't return error, just log the mismatch
            result.errors.push(error_msg);
        }

        Ok(())
    }
}

// Generate random address and account
fn generate_random_address_and_account(rng: &mut impl Rng) -> (Address, StateAccount) {
    let mut bytes = [0u8; 20];
    rng.fill(&mut bytes);
    let address = Address::from(bytes);

    let account = StateAccount {
        nonce: rng.gen::<u64>(),
        balance: U256::from(rng.gen::<u64>()),
        storage_root: B256::from_slice(&rng.gen::<[u8; 32]>()),
        code_hash: B256::from_slice(&rng.gen::<[u8; 32]>()),
    };

    (address, account)
}

