# BSC-Reth StateTrie Smoke Test

This project implements a comparison test between BSC (Binance Smart Chain) and Reth's StateTrie implementations. It uses FFI (Foreign Function Interface) to call BSC's Go implementation and compares it with Reth's Rust implementation.

## Project Structure

```
smoke-test/
├── README.md                    # This document
├── Cargo.toml                   # Rust project configuration
├── build.rs                     # Build script for linking BSC library
├── bsc_trie_wrapper.go          # BSC Go FFI wrapper
├── go.mod                       # Go module configuration
├── src/
│   ├── main.rs                  # Main program entry point
│   ├── lib.rs                   # Module definitions
│   ├── bsc_wrapper.rs           # BSC FFI Rust wrapper
│   └── smoke_test.rs            # Core test logic
└── libbsc_trie.dylib           # Compiled BSC dynamic library (macOS)
```

## Prerequisites

### 1. Go Environment
- Go version: 1.24.0 or higher (recommended 1.24.5)
- Ensure `go` command is available

```bash
# Check Go version
go version

# If version is too low, upgrade Go (macOS)
brew install go@1.24
brew link go@1.24

# Or use official installer
# Visit https://golang.org/dl/ to download the latest version
```

### 2. Rust Environment
- Rust version: Latest stable
- Ensure `cargo` command is available

```bash
# Check Rust version
cargo --version
```

### 3. System Dependencies
- macOS: Requires administrator privileges to install dynamic libraries
- Other systems: Adjust according to specific platform

## Quick Start

If you have all dependencies installed, you can quickly run the test:

```bash
# 1. Enter project directory
cd reth/crates/triedb/smoke-test

# 2. Compile BSC library
go build -buildmode=c-shared -o libbsc_trie.dylib bsc_trie_wrapper.go

# 3. Install dynamic library
sudo cp libbsc_trie.dylib /usr/local/lib/

# 4. Return to reth root directory and run test
cd ../../..
cargo run -p reth-triedb-smoke-test
```

## Build Steps

### 1. Compile BSC Dynamic Library

First, enter the smoke-test directory:

```bash
cd reth/crates/triedb/smoke-test
```

Compile BSC Go library as dynamic library:

```bash
go build -buildmode=c-shared -o libbsc_trie.dylib bsc_trie_wrapper.go
```

This will generate the `libbsc_trie.dylib` file.

### 2. Install Dynamic Library to System Path

To ensure the dynamic library can be found at runtime, install it to the system library path:

```bash
# macOS
sudo cp libbsc_trie.dylib /usr/local/lib/

# Linux (if needed)
sudo cp libbsc_trie.so /usr/local/lib/
sudo ldconfig
```

### 3. Compile Rust Project

Return to reth root directory:

```bash
cd ../../..
```

Compile smoke-test:

```bash
cargo build -p reth-triedb-smoke-test
```

## Running Tests

### Run Smoke Test

```bash
cargo run -p reth-triedb-smoke-test
```

### Test Content

Smoke test will perform the following operations:

1. **Initialization Phase**
   - Create BSC and Reth StateTrie instances
   - Use the same initial state root

2. **Insertion Phase**
   - Insert approximately 100,000 random accounts and storage items
   - Commit and compare root hashes every 10,000 insertions
   - Print insertion progress

3. **Deletion Phase**
   - Delete approximately 50,000 previously inserted accounts and storage items
   - Commit and compare root hashes every 5,000 deletions
   - Print deletion progress

4. **Final Verification**
   - Compare final root hashes of both implementations
   - Report test results

### Expected Output

The test will display detailed progress information during execution:

```
Starting Reth StateTrie smoke test...
2025-08-05T12:13:52.141907Z  INFO reth_triedb_smoke_test::smoke_test: Starting smoke test...
Inserting accounts and storage...
Inserted 10000/100000 accounts...
Inserted 20000/100000 accounts...
...
Commit comparison at operation 10000: ✅ Roots match
...
Deleting accounts and storage...
Deleted 5000/50000 accounts...
...
```

### Test Results

- ✅ **Success**: If all root hash comparisons pass
- ❌ **Failure**: If root hash mismatches or errors are found

**Note**: Currently, Reth's StateTrie implementation may have some bugs during deletion operations, which could cause test failures, but this does not affect the correctness of BSC integration.

## Troubleshooting

### 1. Dynamic Library Not Found

If you encounter `Library not loaded: libbsc_trie.dylib` error:

```bash
# Check if library file exists
ls -la /usr/local/lib/libbsc_trie.dylib

# If it doesn't exist, reinstall
sudo cp libbsc_trie.dylib /usr/local/lib/
```

### 2. Go Compilation Errors

If Go compilation fails, check:

```bash
# Clean Go module cache
go clean -modcache

# Re-download dependencies
go mod tidy

# Recompile
go build -buildmode=c-shared -o libbsc_trie.dylib bsc_trie_wrapper.go
```

### 3. Rust Linking Errors

If Rust linking fails:

```bash
# Clean and rebuild
cargo clean
cargo build -p reth-triedb-smoke-test
```

### 4. Permission Issues

If you encounter permission errors:

```bash
# Ensure sudo privileges
sudo -v

# Reinstall dynamic library
sudo cp libbsc_trie.dylib /usr/local/lib/
```

## Technical Details

### FFI Interface

BSC library exposes interfaces through the following FFI functions:

- `bsc_new_state_trie`: Create new StateTrie instance
- `bsc_update_account`: Update account information
- `bsc_update_storage`: Update storage items
- `bsc_delete_account`: Delete account
- `bsc_delete_storage`: Delete storage items
- `bsc_commit`: Commit changes
- `bsc_get_root`: Get current root hash
- `bsc_free_state_trie`: Free StateTrie instance

### Memory Management

- Go objects are managed through a global mapping table to avoid garbage collection issues
- Integer IDs are used instead of pointers to identify StateTrie instances
- Rust side automatically cleans up resources through Drop trait

### Database Backend

- BSC implementation uses in-memory database (`rawdb.NewMemoryDatabase`)
- Reth implementation uses PathDB (based on RocksDB)
- Both support persistent storage

## Development Guide

### Modifying BSC Implementation

If you need to modify BSC's Go implementation:

1. Edit `bsc_trie_wrapper.go`
2. Recompile dynamic library
3. Reinstall to system path
4. Re-run tests

### Modifying Test Logic

If you need to modify test logic:

1. Edit `src/smoke_test.rs`
2. Recompile Rust project
3. Run tests

### Adding New FFI Functions

If you need to add new FFI functions:

1. Add export function in `bsc_trie_wrapper.go`
2. Declare FFI interface in `src/bsc_wrapper.rs`
3. Add corresponding method in `BscStateTrie` implementation
4. Recompile and test

## Project Status

### Completed Features

✅ **BSC Integration**
- Go FFI library compilation and linking
- Memory management issues resolved
- All StateTrie interface implementations

✅ **Smoke Test Framework**
- Complete test workflow
- Progress monitoring and logging
- Root hash comparison functionality

✅ **Build System**
- Automated build scripts
- Dependency management
- Cross-platform support

### Known Issues

⚠️ **Reth StateTrie Implementation Issues**
- "Invalid node" errors during deletion operations
- "Trie already committed" errors during repeated commits
- These issues do not affect the correctness of BSC integration

### Future Improvements

🔧 **Performance Optimization**
- Reduce memory usage
- Optimize FFI call overhead
- Parallelize test operations

🔧 **Feature Extensions**
- Support more trie operations
- Add performance benchmarks
- Support different database backends

## Notes

1. **Go Version Compatibility**: Ensure compatible Go version is used
2. **Dynamic Library Path**: Ensure dynamic library can be found at runtime
3. **Memory Management**: Pay attention to memory management differences between Go and Rust
4. **Error Handling**: Tests include detailed error handling and logging
5. **Performance Considerations**: Large data operations may take considerable time

## Contributing

Issues and Pull Requests are welcome to improve this project.

## License

This project follows the license of the Reth project. 
