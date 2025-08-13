package main

/*
#include <stdlib.h>
#include <stdint.h>
*/
import "C"
import (
	"encoding/hex"
	"sync"
	"unsafe"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/rawdb"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/trie"
	"github.com/ethereum/go-ethereum/triedb"
	"github.com/holiman/uint256"
)

// Global map to keep references to StateTrie objects
var trieRefs = make(map[int]*trie.StateTrie)
var trieRefsMutex sync.Mutex
var nextTrieID = 1

//export bsc_new_state_trie
func bsc_new_state_trie(state_root *C.char, db_path *C.char) C.int {
	stateRootStr := C.GoString(state_root)
	_ = C.GoString(db_path) // Ignore db_path for now, use memory database

	// Parse state root
	var stateRootStrClean string
	if len(stateRootStr) >= 2 && stateRootStr[:2] == "0x" {
		stateRootStrClean = stateRootStr[2:] // Remove "0x" prefix
	} else {
		stateRootStrClean = stateRootStr
	}

	stateRootBytes, err := hex.DecodeString(stateRootStrClean)
	if err != nil {
		return 0
	}
	var stateRoot common.Hash
	copy(stateRoot[:], stateRootBytes)

	// Create memory database using rawdb
	diskdb := rawdb.NewMemoryDatabase()

	// Create triedb
	triedb := triedb.NewDatabase(diskdb, nil)

	// Create StateTrie
	stateTrie, err := trie.NewStateTrie(trie.StateTrieID(stateRoot), triedb)
	if err != nil {
		return 0
	}

	// Create a unique ID and store the reference
	trieRefsMutex.Lock()
	trieID := nextTrieID
	nextTrieID++
	trieRefs[trieID] = stateTrie
	trieRefsMutex.Unlock()

	return C.int(trieID)
}

//export bsc_update_account
func bsc_update_account(tr C.int, address *C.uint8_t, nonce C.uint64_t, balance *C.uint8_t, storage_root *C.uint8_t, code_hash *C.uint8_t) C.int {
	trieRefsMutex.Lock()
	stateTrie, exists := trieRefs[int(tr)]
	trieRefsMutex.Unlock()
	if !exists {
		return C.int(1)
	}

	// Convert address
	var addr common.Address
	copy(addr[:], C.GoBytes(unsafe.Pointer(address), 20))

	// Convert balance
	var bal common.Hash
	copy(bal[:], C.GoBytes(unsafe.Pointer(balance), 32))
	balanceU256 := new(uint256.Int).SetBytes(bal[:])

	// Convert storage root
	var storageRoot common.Hash
	copy(storageRoot[:], C.GoBytes(unsafe.Pointer(storage_root), 32))

	// Convert code hash
	var codeHash common.Hash
	copy(codeHash[:], C.GoBytes(unsafe.Pointer(code_hash), 32))

	// Create account
	account := &types.StateAccount{
		Nonce:    uint64(nonce),
		Balance:  balanceU256,
		Root:     storageRoot,
		CodeHash: codeHash[:],
	}

	err := stateTrie.UpdateAccount(addr, account, 0)
	if err != nil {
		return C.int(1)
	}

	return C.int(0)
}

//export bsc_update_storage
func bsc_update_storage(tr C.int, address *C.uint8_t, key *C.uint8_t, key_len C.size_t, value *C.uint8_t, value_len C.size_t) C.int {
	trieRefsMutex.Lock()
	stateTrie, exists := trieRefs[int(tr)]
	trieRefsMutex.Unlock()
	if !exists {
		return C.int(1)
	}

	// Convert address
	var addr common.Address
	copy(addr[:], C.GoBytes(unsafe.Pointer(address), 20))

	// Convert key and value
	keyBytes := C.GoBytes(unsafe.Pointer(key), C.int(key_len))
	valueBytes := C.GoBytes(unsafe.Pointer(value), C.int(value_len))

	err := stateTrie.UpdateStorage(addr, keyBytes, valueBytes)
	if err != nil {
		return C.int(1)
	}

	return C.int(0)
}

//export bsc_delete_account
func bsc_delete_account(tr C.int, address *C.uint8_t) C.int {
	trieRefsMutex.Lock()
	stateTrie, exists := trieRefs[int(tr)]
	trieRefsMutex.Unlock()
	if !exists {
		return C.int(1)
	}

	// Convert address
	var addr common.Address
	copy(addr[:], C.GoBytes(unsafe.Pointer(address), 20))

	err := stateTrie.DeleteAccount(addr)
	if err != nil {
		return C.int(1)
	}

	return C.int(0)
}

//export bsc_delete_storage
func bsc_delete_storage(tr C.int, address *C.uint8_t, key *C.uint8_t, key_len C.size_t) C.int {
	trieRefsMutex.Lock()
	stateTrie, exists := trieRefs[int(tr)]
	trieRefsMutex.Unlock()
	if !exists {
		return C.int(1)
	}

	// Convert address
	var addr common.Address
	copy(addr[:], C.GoBytes(unsafe.Pointer(address), 20))

	// Convert key
	keyBytes := C.GoBytes(unsafe.Pointer(key), C.int(key_len))

	err := stateTrie.DeleteStorage(addr, keyBytes)
	if err != nil {
		return C.int(1)
	}

	return C.int(0)
}

//export bsc_commit
func bsc_commit(tr C.int, collect_leaf C.int, root_out *C.uint8_t, node_set_out **unsafe.Pointer) C.int {
	trieRefsMutex.Lock()
	stateTrie, exists := trieRefs[int(tr)]
	trieRefsMutex.Unlock()
	if !exists {
		return C.int(1)
	}

	root, _ := stateTrie.Commit(collect_leaf != 0)

	// Copy root to output
	copy((*[32]byte)(unsafe.Pointer(root_out))[:], root[:])

	// Set node set pointer (for now, we don't return the actual node set)
	*node_set_out = nil

	// Note: After commit, the trie is no longer usable according to BSC's documentation.
	// However, for our smoke test, we'll continue using it since we only commit at the end.
	// In a production environment, you would need to recreate the trie here.

	return C.int(0)
}

//export bsc_get_root
func bsc_get_root(tr C.int, root_out *C.uint8_t) C.int {
	trieRefsMutex.Lock()
	stateTrie, exists := trieRefs[int(tr)]
	trieRefsMutex.Unlock()
	if !exists {
		return C.int(1)
	}

	root := stateTrie.Hash()

	// Copy root to output
	copy((*[32]byte)(unsafe.Pointer(root_out))[:], root[:])

	return C.int(0)
}

//export bsc_free_state_trie
func bsc_free_state_trie(tr C.int) {
	trieRefsMutex.Lock()
	delete(trieRefs, int(tr))
	trieRefsMutex.Unlock()
}

//export bsc_free_node_set
func bsc_free_node_set(node_set unsafe.Pointer) {
	// Go's garbage collector will handle the cleanup
	// This is just a placeholder for the FFI interface
}

func main() {
	// This is required for building a C library
}
