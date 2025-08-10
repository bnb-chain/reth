package main

import (
	"encoding/hex"
	"fmt"
	"io"
	"log"

	"github.com/ethereum/go-ethereum/crypto"
	"github.com/ethereum/go-ethereum/rlp"
)

// Node types copied from BSC's trie/node.go (since they're not exported)
type (
	fullNode struct {
		Children [17]node // Indices 0-15: hex digits, index 16: value
		flags    nodeFlag
	}
	hashNode  []byte
	valueNode []byte
	nodeFlag  struct {
		hash  hashNode
		dirty bool
	}
)

type node interface {
	// Node interface methods would go here, but we don't need them for encoding
}

// Implement the node interface for our types
func (fullNode) node()  {}
func (hashNode) node()  {}
func (valueNode) node() {}

// Custom EncodeRLP method to match BSC's behavior
func (n *fullNode) EncodeRLP(w io.Writer) error {
	eb := rlp.NewEncoderBuffer(w)
	n.encode(eb)
	return eb.Flush()
}

// Custom encode method (copied from BSC node_enc.go)
func (n *fullNode) encode(w rlp.EncoderBuffer) {
	offset := w.List()
	for _, c := range n.Children {
		if c != nil {
			switch node := c.(type) {
			case hashNode:
				w.WriteBytes(node)
			case valueNode:
				w.WriteBytes(node)
			default:
				w.Write(rlp.EmptyString)
			}
		} else {
			w.Write(rlp.EmptyString)
		}
	}
	w.ListEnd(offset)
}

// Helper function to create a hash node with specific pattern
func createHashNode(index, seed int) hashNode {
	hash := make([]byte, 32)
	for j := 0; j < 32; j++ {
		hash[j] = byte((index*16 + j + seed) % 256)
	}
	return hashNode(hash)
}

// Helper function to create a value node with specific pattern
func createValueNode(length, seed int) valueNode {
	value := make([]byte, length)
	for i := 0; i < length; i++ {
		value[i] = byte((i + length + seed) % 256)
	}
	return valueNode(value)
}

// Scenario 1: All 16 children (0-15) are HashNodes + child 16 is ValueNode
func testScenario1AllChildrenWithValue() {
	fmt.Println("=== BSC FullNode Scenario 1: All 16 Children + ValueNode ===")

	valueLengths := []int{1, 16, 128, 256, 512, 1024, 10 * 1024, 100 * 1024}

	for _, valueLen := range valueLengths {
		fmt.Printf("\n--- Value length: %d bytes ---\n", valueLen)

		var fn fullNode

		// Set all 16 children (indices 0-15) as HashNodes
		for i := 0; i < 16; i++ {
			fn.Children[i] = createHashNode(i, valueLen)
		}

		// Set child 16 as ValueNode
		fn.Children[16] = createValueNode(valueLen, valueLen)

		// Encode using RLP
		encoded, err := rlp.EncodeToBytes(&fn)
		if err != nil {
			log.Fatalf("Failed to encode FullNode: %v", err)
		}

		// Calculate hash
		hash := crypto.Keccak256(encoded)

		fmt.Printf("Encoded size: %d bytes\n", len(encoded))
		fmt.Printf("Hash: %s\n", hex.EncodeToString(hash))
		if len(encoded) <= 100 {
			fmt.Printf("Encoded: %s\n", hex.EncodeToString(encoded))
		} else {
			fmt.Printf("Encoded (first 32 bytes): %s...\n", hex.EncodeToString(encoded[:32]))
		}
	}
}

// Scenario 2: Children at indices 1,3,5,7 are HashNodes, others empty, child 16 is ValueNode
func testScenario2SpecificChildrenWithValue() {
	fmt.Println("=== BSC FullNode Scenario 2: Children 1,3,5,7 + ValueNode ===")

	valueLengths := []int{1, 16, 128, 256, 512, 1024, 10 * 1024, 100 * 1024}

	for _, valueLen := range valueLengths {
		fmt.Printf("\n--- Value length: %d bytes ---\n", valueLen)

		var fn fullNode

		// Set children at indices 1, 3, 5, 7 as HashNodes
		childIndices := []int{1, 3, 5, 7}
		for _, i := range childIndices {
			fn.Children[i] = createHashNode(i, valueLen)
		}

		// Set child 16 as ValueNode
		fn.Children[16] = createValueNode(valueLen, valueLen+100)

		// Encode using RLP
		encoded, err := rlp.EncodeToBytes(&fn)
		if err != nil {
			log.Fatalf("Failed to encode FullNode: %v", err)
		}

		// Calculate hash
		hash := crypto.Keccak256(encoded)

		fmt.Printf("Encoded size: %d bytes\n", len(encoded))
		fmt.Printf("Hash: %s\n", hex.EncodeToString(hash))
		if len(encoded) <= 100 {
			fmt.Printf("Encoded: %s\n", hex.EncodeToString(encoded))
		} else {
			fmt.Printf("Encoded (first 32 bytes): %s...\n", hex.EncodeToString(encoded[:32]))
		}
	}
}

// Scenario 3: Children at indices 2,4,6,8 are HashNodes, others empty, child 16 is empty (no value)
func testScenario3SpecificChildrenNoValue() {
	fmt.Println("=== BSC FullNode Scenario 3: Children 2,4,6,8 + No ValueNode ===")

	valueLengths := []int{1, 16, 128, 256, 512, 1024, 10 * 1024, 100 * 1024}

	for _, valueLen := range valueLengths {
		fmt.Printf("\n--- Reference value length: %d bytes ---\n", valueLen)

		var fn fullNode

		// Set children at indices 2, 4, 6, 8 as HashNodes
		childIndices := []int{2, 4, 6, 8}
		for _, i := range childIndices {
			fn.Children[i] = createHashNode(i, valueLen)
		}

		// Child 16 remains nil (no value)

		// Encode using RLP
		encoded, err := rlp.EncodeToBytes(&fn)
		if err != nil {
			log.Fatalf("Failed to encode FullNode: %v", err)
		}

		// Calculate hash
		hash := crypto.Keccak256(encoded)

		fmt.Printf("Encoded size: %d bytes\n", len(encoded))
		fmt.Printf("Hash: %s\n", hex.EncodeToString(hash))
		if len(encoded) <= 100 {
			fmt.Printf("Encoded: %s\n", hex.EncodeToString(encoded))
		} else {
			fmt.Printf("Encoded (first 32 bytes): %s...\n", hex.EncodeToString(encoded[:32]))
		}
	}
}

func main() {
	fmt.Println("BSC FullNode RLP Comparison Tests - 3 Scenarios")
	fmt.Println("================================================")

	testScenario1AllChildrenWithValue()
	fmt.Println()

	testScenario2SpecificChildrenWithValue()
	fmt.Println()

	testScenario3SpecificChildrenNoValue()
	fmt.Println()

	fmt.Println("All BSC FullNode scenario tests completed!")
}
