// Run from a Go-BSC checkout: go run /path/to/generate.go /path/to/cases.json
// Writes expected.json beside cases.json. Both clients consume the same inputs.
package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"os"
	"path/filepath"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/state"
	"github.com/ethereum/go-ethereum/core/tracing"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/core/vm"
	"github.com/ethereum/go-ethereum/core/vm/runtime"
	"github.com/ethereum/go-ethereum/eth/tracers"
	_ "github.com/ethereum/go-ethereum/eth/tracers/native"
	"github.com/ethereum/go-ethereum/params"
	"github.com/holiman/uint256"
)

type account struct {
	Nonce   uint64                      `json:"nonce"`
	Balance string                      `json:"balance"`
	Code    string                      `json:"code"`
	Storage map[common.Hash]common.Hash `json:"storage"`
}
type testCase struct {
	Name     string                     `json:"name"`
	Accounts map[common.Address]account `json:"accounts"`
	Gas      uint64                     `json:"gas"`
	Input    string                     `json:"input"`
	Config   string                     `json:"config"`
	Native   bool                       `json:"native"`
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: generate.go cases.json")
	}
	input, err := os.ReadFile(os.Args[1])
	must(err)
	var cases []testCase
	must(json.Unmarshal(input, &cases))
	results := make(map[string]json.RawMessage)
	for _, test := range cases {
		for _, mux := range []bool{false, true} {
			chain := *params.AllDevChainProtocolChanges
			chain.OsakaTime = nil
			if test.Native {
				chain.Parlia = params.BSCChainConfig.Parlia
				chain.ChainID = big.NewInt(56)
				zero := uint64(0)
				chain.JennerTime = &zero
			}
			st, err := state.New(types.EmptyRootHash, state.NewDatabaseForTesting())
			must(err)
			for address, account := range test.Accounts {
				st.SetNonce(address, account.Nonce, tracing.NonceChangeUnspecified)
				if account.Balance != "" {
					n, err := uint256.FromHex(account.Balance)
					must(err)
					st.SetBalance(address, n, tracing.BalanceChangeUnspecified)
				}
				if account.Code != "" {
					st.SetCode(address, common.FromHex(account.Code), tracing.CodeChangeUnspecified)
				}
				for key, value := range account.Storage {
					st.SetState(address, key, value)
				}
			}
			tracerName, config := "prestateTracer", test.Config
			if mux {
				tracerName = "muxTracer"
				config = `{"prestateTracer":` + config + `}`
			}
			tracer, err := tracers.DefaultDirectory.New(tracerName, new(tracers.Context), json.RawMessage(config), &chain)
			name := fmt.Sprintf("%s/mux=%v", test.Name, mux)
			if err != nil {
				result, _ := json.Marshal(map[string]any{"error": err.Error()})
				results[name] = result
				continue
			}
			data := common.FromHex(test.Input)
			intrinsic := uint64(21000)
			for _, b := range data {
				if b == 0 {
					intrinsic += 4
				} else {
					intrinsic += 16
				}
			}
			// runtime.Call passes execution errors to OnTxEnd, unlike transaction tracing,
			// where a reverted/failed receipt is still a successfully processed transaction.
			hooks := *tracer.Hooks
			hooks.OnTxEnd = nil
			_, _, executionErr := runtime.Call(common.HexToAddress("0x42"), data, &runtime.Config{
				State: st, ChainConfig: &chain, GasLimit: test.Gas - intrinsic, BaseFee: big.NewInt(0),
				Coinbase: common.HexToAddress("0x99"), EVMConfig: vm.Config{Tracer: &hooks},
			})
			if executionErr != nil && !errors.Is(executionErr, vm.ErrOutOfGas) && !errors.Is(executionErr, vm.ErrExecutionReverted) {
				panic(executionErr)
			}
			if tracer.Hooks.OnTxEnd != nil {
				tracer.Hooks.OnTxEnd(&types.Receipt{}, nil)
			}

			result, err := tracer.GetResult()
			must(err)
			if mux {
				var wrapped map[string]json.RawMessage
				must(json.Unmarshal(result, &wrapped))
				result = wrapped["prestateTracer"]
			}
			// codeHash is a pre-existing response-schema difference, outside this compatibility contract.
			var accounts map[string]map[string]any
			must(json.Unmarshal(result, &accounts))
			for _, a := range accounts {
				delete(a, "codeHash")
			}
			result, err = json.Marshal(accounts)
			must(err)
			results[name] = result
		}
	}
	output, err := json.MarshalIndent(results, "", "  ")
	must(err)
	must(os.WriteFile(filepath.Join(filepath.Dir(os.Args[1]), "expected.json"), append(output, '\n'), 0644))
	fmt.Printf("generated %d Go-BSC expectations\n", len(results))
}
func must(err error) {
	if err != nil {
		panic(err)
	}
}
