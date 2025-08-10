use crate::bsc_wrapper::BscStateTrie;
use alloy_primitives::{Address, B256, U256};
use std::str::FromStr;
use reth_triedb_state_trie::account::{empty_storage_root, empty_hash};

#[test]
fn test_single_account() {
    println!("Testing single account insertion...");

    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use reth_triedb_state_trie::{StateTrie, SecureTrieId, SecureTrieTrait, account::StateAccount};
    use tempfile::TempDir;

    // Create temporary directory for Reth trie
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_str().unwrap();

    // Initialize Reth StateTrie
    let path_db = PathDB::new(db_path, PathProviderConfig::default()).unwrap();
    let id = SecureTrieId::new(empty_storage_root(), Address::ZERO, empty_storage_root());
    let mut reth_trie = StateTrie::new(id, path_db).unwrap();

    // Create BSC StateTrie instance
    let mut bsc_trie = BscStateTrie::new(empty_storage_root(), "/tmp/bsc_test_db").unwrap();

    // Insert a single account
    let address = Address::from([0x01; 20]);
    let nonce = U256::from(0u64);
    let balance = U256::from(0u64);
    let storage_root = B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap();
    let code_hash = B256::from_str("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").unwrap();

    println!("Inserting account: {:?}", address);

    // Add to Reth trie
    let account = StateAccount::new_with_hashes(nonce, balance, storage_root, code_hash);
    reth_trie.update_account(address, &account).unwrap();

    // Add to BSC trie
    bsc_trie.update_account(address, nonce.try_into().unwrap_or(0), balance.into(), storage_root, code_hash).unwrap();

    // Get roots
    let bsc_root = bsc_trie.root().unwrap();
    let reth_root = reth_trie.root();

    println!("BSC root:  {:?}", bsc_root);
    println!("Reth root: {:?}", reth_root);
    println!("Root match: {}", if bsc_root == reth_root { "YES" } else { "NO" });

    // Clean up
    drop(reth_trie);
    drop(bsc_trie);
}

#[test]
fn test_two_accounts() {
    println!("Testing two accounts insertion...");

    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use reth_triedb_state_trie::{StateTrie, SecureTrieId, SecureTrieTrait, account::StateAccount};
    use tempfile::TempDir;

    // Create temporary directory for Reth trie
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_str().unwrap();

    // Initialize Reth StateTrie
    let path_db = PathDB::new(db_path, PathProviderConfig::default()).unwrap();
    let id = SecureTrieId::new(empty_storage_root(), Address::ZERO, empty_storage_root());
    let mut reth_trie = StateTrie::new(id, path_db).unwrap();

    // Create BSC StateTrie instance
    let mut bsc_trie = BscStateTrie::new(empty_storage_root(), "/tmp/bsc_test_db2").unwrap();

    // Insert first account
    let address1 = Address::from([0x01; 20]);
    let nonce = U256::from(0u64);
    let balance = U256::from(0u64);
    let storage_root = B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap();
    let code_hash = B256::from_str("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").unwrap();

    println!("Inserting account 1: {:?}", address1);

    // Add to Reth trie
    let account1 = StateAccount::new_with_hashes(nonce, balance, storage_root, code_hash);
    reth_trie.update_account(address1, &account1).unwrap();

    // Add to BSC trie
    bsc_trie.update_account(address1, nonce.try_into().unwrap_or(0), balance.into(), storage_root, code_hash).unwrap();

    // Insert second account
    let address2 = Address::from([0x02; 20]);
    println!("Inserting account 2: {:?}", address2);

    // Add to Reth trie
    let account2 = StateAccount::new_with_hashes(nonce, balance, storage_root, code_hash);
    reth_trie.update_account(address2, &account2).unwrap();

    // Add to BSC trie
    bsc_trie.update_account(address2, nonce.try_into().unwrap_or(0), balance.into(), storage_root, code_hash).unwrap();

    // Get roots
    let bsc_root = bsc_trie.root().unwrap();
    let reth_root = reth_trie.root();

    println!("BSC root:  {:?}", bsc_root);
    println!("Reth root: {:?}", reth_root);
    println!("Root match: {}", if bsc_root == reth_root { "YES" } else { "NO" });

    // Clean up
    drop(reth_trie);
    drop(bsc_trie);
}

#[test]
fn test_smoke_test_addresses() {
    println!("Testing smoke test address pattern...");

    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use reth_triedb_state_trie::{StateTrie, SecureTrieId, SecureTrieTrait, account::StateAccount};
    use tempfile::TempDir;

    // Create temporary directory for Reth trie
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_str().unwrap();

    // Initialize Reth StateTrie
    let path_db = PathDB::new(db_path, PathProviderConfig::default()).unwrap();
    let id = SecureTrieId::new(empty_storage_root(), Address::ZERO, empty_storage_root());
    let mut reth_trie = StateTrie::new(id, path_db).unwrap();

    // Create BSC StateTrie instance
    let mut bsc_trie = BscStateTrie::new(empty_storage_root(), "/tmp/bsc_test_db3").unwrap();

    // Insert accounts using the same pattern as smoke test
    for i in 0..10 {
        let address = Address::from([i as u8; 20]); // Same pattern as smoke test
        let nonce = U256::from(0u64);
        let balance = U256::from(0u64);
        let storage_root = B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap();
        let code_hash = B256::from_str("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").unwrap();

        println!("Inserting account {}: {:?}", i, address);

        // Add to Reth trie
        let account = StateAccount::new_with_hashes(nonce, balance, storage_root, code_hash);
        reth_trie.update_account(address, &account).unwrap();

        // Add to BSC trie
        bsc_trie.update_account(address, nonce.try_into().unwrap_or(0), balance.into(), storage_root, code_hash).unwrap();

        // Get roots every 5 accounts
        if (i + 1) % 5 == 0 {
            let bsc_root = bsc_trie.root().unwrap();
            let reth_root = reth_trie.root();

            println!("After {} accounts:", i + 1);
            println!("BSC root:  {:?}", bsc_root);
            println!("Reth root: {:?}", reth_root);
            println!("Root match: {}", if bsc_root == reth_root { "YES" } else { "NO" });
            println!("---");
        }
    }

    // Clean up
    drop(reth_trie);
    drop(bsc_trie);
}

#[test]
fn test_state_trie_hash() {
    println!("Testing StateTrie hash calculation...");

    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use reth_triedb_state_trie::{StateTrie, SecureTrieId, SecureTrieTrait, account::StateAccount};
    use tempfile::TempDir;

    // Create temporary directory for Reth trie
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_str().unwrap();

    // Initialize Reth StateTrie
    let path_db = PathDB::new(db_path, PathProviderConfig::default()).unwrap();
    let id = SecureTrieId::new(empty_storage_root(), Address::ZERO, empty_storage_root());
    let mut reth_trie = StateTrie::new(id, path_db).unwrap();

    // Insert a single account
    let address = Address::from([0x01; 20]);
    let account = StateAccount::new(U256::from(0u64), U256::from(0u64));

    println!("Inserting account: {:?}", address);

    // Add to Reth trie
    reth_trie.update_account(address, &account).unwrap();

    // Get root
    let reth_root = reth_trie.root();

    println!("Reth root: {:?}", reth_root);

    // Clean up
    drop(reth_trie);
}

#[test]
fn test_state_trie_smoke_pattern() {
    println!("Testing StateTrie with smoke test address pattern...");

    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use reth_triedb_state_trie::{StateTrie, SecureTrieId, SecureTrieTrait, account::StateAccount};
    use tempfile::TempDir;

    // Create temporary directory for Reth trie
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_str().unwrap();

    // Initialize Reth StateTrie
    let path_db = PathDB::new(db_path, PathProviderConfig::default()).unwrap();
    let id = SecureTrieId::new(empty_storage_root(), Address::ZERO, empty_storage_root());
    let mut reth_trie = StateTrie::new(id, path_db).unwrap();

    // Create BSC StateTrie instance
    let mut bsc_trie = BscStateTrie::new(empty_storage_root(), "/tmp/bsc_test_db4").unwrap();

    // Insert accounts using the same pattern as smoke test
    for i in 0..10 {
        let address = Address::from([i as u8; 20]); // Same pattern as smoke test
        let nonce = U256::from(0u64);
        let balance = U256::from(0u64);
        let storage_root = B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap();
        let code_hash = B256::from_str("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").unwrap();

        println!("Inserting account {}: {:?}", i, address);

        // Add to Reth trie
        let account = StateAccount::new_with_hashes(nonce, balance, storage_root, code_hash);
        reth_trie.update_account(address, &account).unwrap();

        // Add to BSC trie
        bsc_trie.update_account(address, nonce.try_into().unwrap_or(0), balance.into(), storage_root, code_hash).unwrap();

        // Get roots every 5 accounts
        if (i + 1) % 5 == 0 {
            let bsc_root = bsc_trie.root().unwrap();
            let reth_root = reth_trie.root();

            println!("After {} accounts:", i + 1);
            println!("BSC root:  {:?}", bsc_root);
            println!("Reth root: {:?}", reth_root);
            println!("Root match: {}", if bsc_root == reth_root { "YES" } else { "NO" });
            println!("---");
        }
    }

    // Clean up
    drop(reth_trie);
    drop(bsc_trie);
}

#[test]
fn test_single_account_debug() {
    println!("Testing single account insertion with debug info...");

    use reth_triedb_pathdb::{PathDB, PathProviderConfig};
    use reth_triedb_state_trie::{StateTrie, SecureTrieId, SecureTrieTrait, account::StateAccount};
    use tempfile::TempDir;

    // Create temporary directory for Reth trie
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().to_str().unwrap();

    // Initialize Reth StateTrie
    let path_db = PathDB::new(db_path, PathProviderConfig::default()).unwrap();
    let id = SecureTrieId::new(empty_storage_root(), Address::ZERO, empty_storage_root());
    let mut reth_trie = StateTrie::new(id, path_db).unwrap();

    // Create BSC StateTrie instance
    let mut bsc_trie = BscStateTrie::new(empty_storage_root(), "/tmp/bsc_test_db_debug").unwrap();

    // Insert a single account
    let address = Address::from([0x01; 20]);
    let nonce = U256::from(0u64);
    let balance = U256::from(0u64);
    let storage_root = B256::from_str("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").unwrap();
    let code_hash = B256::from_str("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").unwrap();

    println!("Inserting account: {:?}", address);
    println!("Address bytes: {:?}", address.as_slice());
    println!("Address hex: 0x{}", hex::encode(address.as_slice()));

    // Add to Reth trie
    let account = StateAccount::new_with_hashes(nonce, balance, storage_root, code_hash);
    reth_trie.update_account(address, &account).unwrap();

    // Add to BSC trie
    bsc_trie.update_account(address, nonce.try_into().unwrap_or(0), balance.into(), storage_root, code_hash).unwrap();

    // Get roots
    let bsc_root = bsc_trie.root().unwrap();
    let reth_root = reth_trie.root();

    println!("BSC root:  {:?}", bsc_root);
    println!("Reth root: {:?}", reth_root);
    println!("Root match: {}", if bsc_root == reth_root { "YES" } else { "NO" });

    // Clean up
    drop(reth_trie);
    drop(bsc_trie);
}

#[test]
fn test_keybytes_to_hex() {
    println!("Testing keybytes_to_hex...");

    // Test address
    let address = Address::from([0x01; 20]);
    println!("Address: {:?}", address);
    println!("Address bytes: {:?}", address.as_slice());

    // Manual keybytes_to_hex implementation
    let mut nibbles = Vec::new();
    for byte in address.as_slice() {
        nibbles.push(byte / 16); // High nibble
        nibbles.push(byte % 16); // Low nibble
    }
    nibbles.push(16); // terminator

    println!("Nibbles: {:?}", nibbles);
    println!("Nibbles hex: 0x{}", nibbles.iter().map(|n| format!("{:x}", n)).collect::<String>());

    // Expected nibbles for address [0x01; 20] should be [0, 1, 0, 1, ..., 16]
    let expected: Vec<u8> = (0..20).flat_map(|_| vec![0, 1]).chain(vec![16]).collect();
    println!("Expected: {:?}", expected);
    println!("Expected hex: 0x{}", expected.iter().map(|n| format!("{:x}", n)).collect::<String>());

    assert_eq!(nibbles, expected, "Nibbles don't match expected");
    println!("✅ Nibbles match expected!");
}

#[test]
fn test_rlp_encoding() {
    println!("Testing RLP encoding...");

    // Test empty string encoding
    let mut encoded = Vec::new();
    alloy_rlp::Header { list: false, payload_length: 0 }.encode(&mut encoded);
    println!("Empty string encoding: {:?}", encoded);
    println!("Empty string hex: 0x{}", hex::encode(&encoded));

    // BSC's rlp.EmptyString is [0x80]
    let expected = vec![0x80];
    println!("Expected: {:?}", expected);
    println!("Expected hex: 0x{}", hex::encode(&expected));

    assert_eq!(encoded, expected, "RLP encoding doesn't match BSC's EmptyString");
    println!("✅ RLP encoding matches BSC's EmptyString!");
}


