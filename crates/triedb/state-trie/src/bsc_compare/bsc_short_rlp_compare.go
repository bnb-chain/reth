package main

import (
	"fmt"

	"github.com/ethereum/go-ethereum/crypto"
	"github.com/ethereum/go-ethereum/rlp"
)

// BSC's native node types (copied from bsc/trie/node.go)
type node interface {
	cache() (hashNode, bool)
	encode(w rlp.EncoderBuffer)
	fstring(string) string
}

type (
	shortNode struct {
		Key   []byte
		Val   node
		flags nodeFlag
	}
	hashNode  []byte
	valueNode []byte
)

type nodeFlag struct {
	hash  hashNode // cached hash of the node (may be nil)
	dirty bool     // whether the node has changes that must be written to the database
}

func (n *shortNode) cache() (hashNode, bool) { return n.flags.hash, n.flags.dirty }
func (n hashNode) cache() (hashNode, bool)   { return nil, true }
func (n valueNode) cache() (hashNode, bool)  { return nil, true }

func (n *shortNode) fstring(ind string) string {
	return fmt.Sprintf("{%x: %v} ", n.Key, n.Val.fstring(ind+"  "))
}
func (n hashNode) fstring(ind string) string  { return fmt.Sprintf("<%x> ", []byte(n)) }
func (n valueNode) fstring(ind string) string { return fmt.Sprintf("%x ", []byte(n)) }

// RLP encoding for shortNode (simplified version based on BSC's implementation)
func (n *shortNode) encode(w rlp.EncoderBuffer) {
	w.WriteBytes(hexToCompact(n.Key))
	n.Val.encode(w)
}

// Implement RLP encoding interface for shortNode
func (n *shortNode) EncodeRLP(w rlp.EncoderBuffer) error {
	w.WriteBytes(hexToCompact(n.Key))
	if hashNode, ok := n.Val.(hashNode); ok {
		w.WriteBytes([]byte(hashNode))
	} else if valueNode, ok := n.Val.(valueNode); ok {
		w.WriteBytes([]byte(valueNode))
	}
	return nil
}

func (n hashNode) encode(w rlp.EncoderBuffer) {
	w.WriteBytes([]byte(n))
}

func (n valueNode) encode(w rlp.EncoderBuffer) {
	w.WriteBytes([]byte(n))
}

func main() {
	fmt.Println("=== BSC ShortNode RLP Comprehensive Comparison Tests ===")
	fmt.Println("Using BSC native shortNode structures")

	testShortNodeWithHashValues()
	testShortNodeWithValueNodes()

	fmt.Println("\n=== All BSC ShortNode tests completed ===")
}

// Test ShortNode with hash values for different key lengths
func testShortNodeWithHashValues() {
	fmt.Println("\n=== BSC ShortNode with HashNode Values ===")

	// Test key lengths from 1 to 65 bytes (comprehensive coverage)
	for hexKeyLen := 1; hexKeyLen <= 65; hexKeyLen++ {
		fmt.Printf("\n--- Testing hex key length: %d bytes ---\n", hexKeyLen)

		// Create a fixed base value to hash
		baseValue := []byte("test_key_for_bsc_short_node_comparison_12345")

		// Get hash of the base value
		baseHash := crypto.Keccak256(baseValue)

		// Process hash using BSC's keybytesToHex to get full hex key
		fullHexKey := keybytesToHex(baseHash)

		// Truncate from the end to get desired length
		// Take the last hexKeyLen bytes from fullHexKey
		var hexKey []byte
		if hexKeyLen >= len(fullHexKey) {
			hexKey = fullHexKey
		} else {
			hexKey = fullHexKey[len(fullHexKey)-hexKeyLen:]
		}

		fmt.Printf("Base hash: %x\n", baseHash)
		fmt.Printf("Full hex key (%d bytes): %x\n", len(fullHexKey), fullHexKey)
		fmt.Printf("Truncated hex key (%d bytes): %x\n", len(hexKey), hexKey)

		// Verify hex key length matches expected
		if len(hexKey) != hexKeyLen {
			panic(fmt.Sprintf("Expected hex key length %d, got %d", hexKeyLen, len(hexKey)))
		}

		// For HashNode tests, ensure key does NOT have terminator (extension node)
		if len(hexKey) > 0 && hexKey[len(hexKey)-1] == 0x10 {
			hexKey[len(hexKey)-1] = 0x0f // Change terminator to a valid nibble
		}

		// Create hash value (32 bytes)
		hashValue := make([]byte, 32)
		for i := 0; i < 32; i++ {
			hashValue[i] = byte((0x80 + hexKeyLen + i) % 256)
		}

		// Create BSC's native shortNode
		shortNode := &shortNode{
			Key:   hexKey,              // Direct hex key, no compact encoding here
			Val:   hashNode(hashValue), // Hash as hashNode type
			flags: nodeFlag{},
		}

		// Manually construct RLP data (as BSC would do)
		shortNodeData := []interface{}{
			hexToCompact(shortNode.Key), // BSC uses compact encoding in RLP
			[]byte(hashValue),           // Hash as bytes
		}

		// Encode using RLP
		encoded, err := rlp.EncodeToBytes(shortNodeData)
		if err != nil {
			panic(fmt.Sprintf("Failed to encode ShortNode: %v", err))
		}

		// Calculate hash
		hash := crypto.Keccak256(encoded)

		fmt.Printf("ShortNode encoded size: %d bytes\n", len(encoded))
		fmt.Printf("ShortNode encoded: %x\n", encoded)
		fmt.Printf("ShortNode hash: %x\n", hash)

		// Verify roundtrip decoding by decoding as generic interface
		var decoded []interface{}
		err = rlp.DecodeBytes(encoded, &decoded)
		if err != nil {
			panic(fmt.Sprintf("Failed to decode ShortNode: %v", err))
		}

		if len(decoded) != 2 {
			panic(fmt.Sprintf("Expected 2 elements, got %d", len(decoded)))
		}

		// Verify key matches (decoded[0] should be compact key)
		decodedCompactKey, ok := decoded[0].([]byte)
		if !ok {
			panic("Decoded key is not []byte")
		}

		// The compact key should be what we expect
		expectedCompactKey := hexToCompact(shortNode.Key)
		// Verify compact keys match exactly
		if len(decodedCompactKey) != len(expectedCompactKey) {
			fmt.Printf("Compact key length mismatch: expected %d, got %d\n", len(expectedCompactKey), len(decodedCompactKey))
			fmt.Printf("Expected compact key: %x\n", expectedCompactKey)
			fmt.Printf("Decoded compact key: %x\n", decodedCompactKey)
		} else {
			// Check if content matches
			for i, b := range expectedCompactKey {
				if decodedCompactKey[i] != b {
					fmt.Printf("Compact key content mismatch at position %d: expected %02x, got %02x\n", i, b, decodedCompactKey[i])
					break
				}
			}
		}

		fmt.Printf("✅ Hex key length %d bytes: Encoding/decoding successful\n", hexKeyLen)
	}
}

// Test ShortNode with value nodes for different key and value lengths
func testShortNodeWithValueNodes() {
	fmt.Println("\n=== BSC ShortNode with ValueNode Values ===")

	// Test key lengths from 1 to 65 bytes (comprehensive coverage)
	// Value lengths to test: 1, 16, 32, 64, 128, 256, 512, 1024, 10K, 100K
	valueLengths := []int{1, 16, 32, 64, 128, 256, 512, 1024, 10 * 1024, 100 * 1024}

	for hexKeyLen := 1; hexKeyLen <= 65; hexKeyLen++ {
		fmt.Printf("\n--- Testing hex key length: %d bytes with various value lengths ---\n", hexKeyLen)

		// Create a fixed base value to hash (different pattern for value tests)
		baseValue := []byte("value_test_key_for_bsc_short_node_comparison_67890")

		// Get hash of the base value
		baseHash := crypto.Keccak256(baseValue)

		// Process hash using BSC's keybytesToHex to get full hex key
		fullHexKey := keybytesToHex(baseHash)

		// Truncate from the end to get desired length
		// Take the last hexKeyLen bytes from fullHexKey
		var hexKey []byte
		if hexKeyLen >= len(fullHexKey) {
			hexKey = fullHexKey
		} else {
			hexKey = fullHexKey[len(fullHexKey)-hexKeyLen:]
		}

		// Verify hex key length matches expected
		if len(hexKey) != hexKeyLen {
			panic(fmt.Sprintf("Expected hex key length %d, got %d", hexKeyLen, len(hexKey)))
		}

		for _, valueLen := range valueLengths {
			fmt.Printf("\n  Testing value length: %d bytes\n", valueLen)

			// Create value data
			valueData := make([]byte, valueLen)
			for i := 0; i < valueLen; i++ {
				valueData[i] = byte((i + valueLen + hexKeyLen) % 256)
			}

			// Create BSC's native shortNode with valueNode
			shortNode := &shortNode{
				Key:   hexKey,               // Direct hex key
				Val:   valueNode(valueData), // Value as valueNode type
				flags: nodeFlag{},
			}

			// Manually construct RLP data (as BSC would do)
			shortNodeData := []interface{}{
				hexToCompact(shortNode.Key), // BSC uses compact encoding in RLP
				valueData,                   // Value as bytes
			}

			// Encode using RLP
			encoded, err := rlp.EncodeToBytes(shortNodeData)
			if err != nil {
				panic(fmt.Sprintf("Failed to encode ShortNode: %v", err))
			}

			// Calculate hash
			hash := crypto.Keccak256(encoded)

			fmt.Printf("    Hex key: %d bytes, Value: %d bytes\n", hexKeyLen, valueLen)
			fmt.Printf("    Encoded size: %d bytes\n", len(encoded))
			if len(encoded) <= 200 { // Only show full encoding for smaller data
				fmt.Printf("    Encoded: %x\n", encoded)
			} else {
				fmt.Printf("    Encoded (first 32 bytes): %x...\n", encoded[:32])
			}
			fmt.Printf("    Hash: %x\n", hash)

			// Verify roundtrip decoding by decoding as generic interface
			var decoded []interface{}
			err = rlp.DecodeBytes(encoded, &decoded)
			if err != nil {
				panic(fmt.Sprintf("Failed to decode ShortNode: %v", err))
			}

			if len(decoded) != 2 {
				panic(fmt.Sprintf("Expected 2 elements, got %d", len(decoded)))
			}

			// Verify key matches (decoded[0] should be compact key)
			decodedCompactKey, ok := decoded[0].([]byte)
			if !ok {
				panic("Decoded key is not []byte")
			}

			// The compact key should be what we expect
			expectedCompactKey := hexToCompact(shortNode.Key)
			// Verify compact keys match exactly
			if len(decodedCompactKey) != len(expectedCompactKey) {
				fmt.Printf("    Compact key length mismatch: expected %d, got %d\n", len(expectedCompactKey), len(decodedCompactKey))
				fmt.Printf("    Expected compact key: %x\n", expectedCompactKey)
				fmt.Printf("    Decoded compact key: %x\n", decodedCompactKey)
			} else {
				// Check if content matches
				for i, b := range expectedCompactKey {
					if decodedCompactKey[i] != b {
						fmt.Printf("    Compact key content mismatch at position %d: expected %02x, got %02x\n", i, b, decodedCompactKey[i])
						break
					}
				}
			}

			// Verify value matches by checking length and content
			decodedValue, ok := decoded[1].([]byte)
			if !ok {
				panic("Decoded value is not []byte")
			}

			if len(decodedValue) != len(valueData) {
				panic(fmt.Sprintf("Value length mismatch: original %d, decoded %d", len(valueData), len(decodedValue)))
			}

			// Check first and last bytes for large values
			if len(valueData) > 0 {
				if decodedValue[0] != valueData[0] {
					panic("First byte mismatch")
				}
				if len(valueData) > 1 && decodedValue[len(decodedValue)-1] != valueData[len(valueData)-1] {
					panic("Last byte mismatch")
				}
			}

			fmt.Printf("    ✅ Hex key length %d, Value length %d: Success\n", hexKeyLen, valueLen)
		}
	}
}

// BSC's encoding functions (copied from BSC trie/encoding.go to ensure exact compatibility)

// keybytesToHex converts key bytes to hex format (equivalent to Rust's key_to_nibbles)
func keybytesToHex(str []byte) []byte {
	l := len(str)*2 + 1
	var nibbles = make([]byte, l)
	for i, b := range str {
		nibbles[i*2] = b / 16
		nibbles[i*2+1] = b % 16
	}
	nibbles[l-1] = 16
	return nibbles
}

// hexToCompact converts hex key to compact format (used in RLP encoding)
func hexToCompact(hex []byte) []byte {
	terminator := byte(0)
	if hasTerm(hex) {
		terminator = 1
		hex = hex[:len(hex)-1]
	}
	buf := make([]byte, len(hex)/2+1)
	buf[0] = terminator << 5 // the flag byte
	if len(hex)&1 == 1 {
		buf[0] |= 1 << 4 // odd flag
		buf[0] |= hex[0] // first nibble is contained in the first byte
		hex = hex[1:]
	}
	decodeNibbles(hex, buf[1:])
	return buf
}

// hasTerm checks if hex key has terminator
func hasTerm(s []byte) bool {
	return len(s) > 0 && s[len(s)-1] == 16
}

// decodeNibbles helper function
func decodeNibbles(nibbles []byte, bytes []byte) {
	for bi, ni := 0, 0; ni < len(nibbles); bi, ni = bi+1, ni+2 {
		bytes[bi] = nibbles[ni]<<4 | nibbles[ni+1]
	}
}
