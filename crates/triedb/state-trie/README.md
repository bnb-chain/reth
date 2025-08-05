# Secure State Trie Implementation

This module provides a secure trie implementation that wraps a regular trie with key hashing functionality, mirroring BSC's `bsc/trie/secure_trie.go` implementation.

## BSC Compatibility

This implementation maintains full compatibility with BSC's design:

- **SecureTrie = StateTrie**: `SecureTrie` is a type alias for `StateTrie` (same as BSC)
- **NewSecure Function**: `StateTrie::new_secure()` creates a new `StateTrie` (deprecated, use `StateTrie::new`)
- **Key Hashing**: All keys are hashed using keccak256 before storage
- **API Structure**: Maintains the same API structure as BSC's implementation

In a secure trie, all access operations hash the key using keccak256. This prevents calling code from creating long chains of nodes that increase access time.

## Features

- **Key Hashing**: All keys are hashed using keccak256 before being stored in the trie
- **Account Management**: Support for storing and retrieving Ethereum accounts
- **Storage Management**: Support for storing and retrieving storage values
- **Memory Database**: In-memory database implementation for testing and debugging
- **Thread Safety**: Secure key cache with thread safety considerations

## Usage

### Basic Usage

```rust
use alloy_primitives::{Address, B256};
use reth_primitives_traits::StateAccount;
use reth_triedb_memorydb::MemoryDB;
use reth_triedb_state_trie::{StateTrie, SecureTrieBuilder, SecureTrieId};

// Create an in-memory database
let db = MemoryDB::new();

// Create a secure trie identifier
let id = SecureTrieId::new(
    B256::ZERO,           // state root
    Address::ZERO,        // owner
    B256::ZERO,           // root
);

// Create a StateTrie using the builder pattern (recommended)
let mut trie = SecureTrieBuilder::new(db)
    .with_id(id)
    .build()?;

// Alternative: Create using NewSecure function (deprecated, same as BSC)
let mut trie = StateTrie::new_secure(
    B256::ZERO,           // state root
    Address::ZERO,        // owner
    B256::ZERO,           // root
    MemoryDB::new()
)?;

// Create and update an account
let address = Address::from([1u8; 20]);
let account = StateAccount {
    nonce: 1,
    balance: 1000.into(),
    storage_root: B256::ZERO,
    code_hash: B256::ZERO,
};

trie.update_account(address, &account)?;

// Update storage
let storage_key = b"storage_key";
let storage_value = b"storage_value";
trie.update_storage(address, storage_key, storage_value)?;

// Commit the trie
let root = trie.commit()?;
```

### Account Operations

```rust
// Get an account
let account = trie.get_account(address)?;

// Update an account
trie.update_account(address, &new_account)?;

// Delete an account
trie.delete_account(address)?;
```

### Storage Operations

```rust
// Get a storage value
let value = trie.get_storage(address, key)?;

// Update a storage value
trie.update_storage(address, key, value)?;

// Delete a storage value
trie.delete_storage(address, key)?;
```

### General Key-Value Operations

```rust
// Get a value
let value = trie.get(key)?;

// Update a value
trie.update(key, value)?;

// Delete a value
trie.delete(key)?;
```

## Architecture

### StateTrie

The main `StateTrie` struct wraps a database and provides secure trie operations:

- **Key Hashing**: All keys are hashed using keccak256
- **Secure Key Cache**: Maintains a mapping from hashed keys to original keys
- **Thread Safety**: Cache ownership tracking for thread safety

### SecureTrie

`SecureTrie` is a type alias for `StateTrie` (same as BSC's implementation):

```rust
pub type SecureTrie<DB> = StateTrie<DB>;
```

This maintains compatibility with BSC's design where `SecureTrie` and `StateTrie` are the same type.

### MemoryDB

An in-memory database implementation that implements the `TrieDatabase` trait:

- **In-Memory Storage**: Uses a HashMap for storing trie nodes
- **Thread Safety**: Uses RwLock for concurrent access
- **Simple Interface**: Easy to use for testing and debugging

### SecureTrieId

Identifies a secure trie with:

- **State Root**: The state root hash
- **Owner**: The owner address (for storage tries)
- **Root**: The trie root hash

## Implementation Notes

### Key Hashing

All keys are hashed using keccak256 before being stored in the trie. This prevents:

1. **Long Key Attacks**: Prevents creation of long chains of nodes
2. **Access Time Optimization**: Ensures consistent access times
3. **Security**: Provides cryptographic security for key storage

### Secure Key Cache

The secure trie maintains a cache mapping hashed keys to original keys:

- **Performance**: Avoids recomputing hashes
- **Memory Management**: Cache is cleared when ownership changes
- **Thread Safety**: Cache ownership tracking prevents race conditions

### Database Integration

The current implementation provides a framework for database integration:

- **TrieDatabase Trait**: Defines the interface for trie databases
- **MemoryDB**: In-memory implementation for testing
- **Extensible**: Can be extended to support other database backends

## Limitations

1. **Preimage Store**: Preimage store functionality is not implemented (as requested)
2. **Full Trie Integration**: The current implementation is a framework and would need integration with a full trie implementation
3. **Database Backends**: Currently only supports in-memory database

## Future Enhancements

1. **Full Trie Integration**: Integrate with reth's trie implementation
2. **Database Backends**: Support for persistent database backends
3. **Preimage Store**: Implement preimage store functionality
4. **Performance Optimization**: Optimize for large-scale usage
5. **Error Handling**: Enhanced error handling and recovery

## Testing

Run the tests with:

```bash
cargo test
```

Run the example with:

```bash
cargo run --example basic_usage
```

## License

This project is licensed under the MIT OR Apache-2.0 license. 
