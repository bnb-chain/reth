# Prestate compatibility contract

The default (non-diff) `prestateTracer` must report the accounts and storage slots
observed by Go-BSC, with their original transaction state. `includeEmpty` controls
whether originally empty accounts remain. This applies to standalone and mux
tracing, simulated calls, transaction replay, and block/batch execution.

Observations comprise transaction sender, recipient/creation candidate, beneficiary,
recoverable authorization authorities, successful opcode gas checks, delegation
resolution, and explicit native account/storage reads. They survive child reverts.
Gas-cost-only account loads are not observations. CREATE/CREATE2 candidates are
observed before execution-time rejection, subject to Go's memory-copy limit.
Each execution has a fresh native-read scope; batch execution still commits state
between transactions. The scope is synchronous and restored on panic.

Known boolean option names use Go's ASCII case-insensitive matching, input order,
null handling, and type/conflict errors. Raw RPC decoding happens before a JSON
map can collapse repeated keys. Typed callers already holding `Value` cannot
recover earlier duplicate-key occurrences. Chain-specific RPC replacements must
use `with_prestate_config` when building their final module.

This is a bounded compatibility change, not a claim of complete tracer parity.
Diff-mode semantics remain with the upstream inspector. Go-BSC's `codeHash` field
is a pre-existing schema difference and is excluded from the comparison. The
original remote transaction was not replayed against its private deployment.

## Differential fixtures

Reference: Go-BSC `583be4dbf9ab748801ffdd02b4c13e46b103a9f9`.
`cases.json` is consumed by both the Rust RPC test and `generate.go`; `expected.json`
is the Go output, with only `codeHash` removed. The comparison checks complete
account membership and every remaining field, including storage values.

From that Go-BSC checkout:

```sh
go run /path/to/reth/crates/rpc/rpc/testdata/prestate/generate.go /path/to/reth/crates/rpc/rpc/testdata/prestate/cases.json
```

Then, from Reth:

```sh
cargo test -p reth-rpc go_bsc_differential_prestate_matrix --lib
```

The generator also accepts Reth-BSC's `tests/fixtures/prestate/cases.json` for its
native-read matrix. It adjusts `runtime.Call` completion to transaction semantics:
EVM failure produces a failed receipt, not a transaction-processing error.

The matrix covers gas failure versus balance failure, reads before/after reverted
children, repeated accesses, delegation after gas failure, static and non-static
creation, memory-padding boundaries, reverted storage writes, SELFDESTRUCT,
include/omit-empty modes, disabled fields, duplicate aliases and invalid options.
Existing targeted tests additionally cover authorization recovery, block replay,
batch isolation, access-list warming, native storage, and tracer combinations.
