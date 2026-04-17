# TrieDB / MDBX Startup Alignment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** At node startup, when TrieDB pathdb lags MDBX (post-crash), unwind MDBX (static files + all stage checkpoints) down to the pathdb tip so the two backends agree before any pipeline or engine work begins. Fail hard on the reverse case or on suspiciously large gaps.

**Architecture:** A new `align_mdbx_to_triedb_at_startup()` runs at the top of `initial_backfill_target()` (before `check_pipeline_consistency()`). The decision (no-op / unwind / fatal) is factored into a pure function that takes inputs as arguments so it can be unit-tested without a real provider. The wrapper collects inputs from the live provider and TrieDB, calls the decision function, and if an unwind is needed, invokes `DatabaseProvider::remove_block_and_execution_above(pathdb_tip)` + `commit()`. Unreachable branches in `check_pipeline_consistency_under_triedb` are removed once alignment guarantees `mdbx_tip == pathdb_tip`.

**Tech Stack:** Rust, reth-provider, reth-node-builder, rust-eth-triedb.

**Spec reference:** `docs/superpowers/specs/2026-04-17-triedb-mdbx-startup-alignment-design.md`.

---

## File Structure

- **Create:** `crates/node/builder/src/launch/alignment.rs`
  - `AlignmentOutcome` enum (decision value)
  - `decide_startup_alignment(...)` pure function
  - Unit tests for the decision function
- **Modify:** `crates/node/builder/src/launch/mod.rs`
  - Add `pub(crate) mod alignment;`
- **Modify:** `crates/storage/errors/src/provider.rs`
  - Add three `ProviderError` variants: `TriedbAheadOfMdbx`, `TriedbMdbxRootMismatch`, `StartupUnwindExceedsLimit`
- **Modify:** `crates/node/builder/src/launch/common.rs`
  - Add `align_mdbx_to_triedb_at_startup()` method
  - Add `MAX_STARTUP_UNWIND_BLOCKS` const
  - Call the new method at the top of `initial_backfill_target()`
  - Remove unreachable branches inside `check_pipeline_consistency_under_triedb`

No new files or modifications are needed in the engine tree, stages, or TrieDB crates.

---

## Task 1: Add `ProviderError` variants

**Files:**
- Modify: `crates/storage/errors/src/provider.rs`

**Context:** The existing enum already has simple single-tuple variants (e.g. `HeaderNotFound(BlockHashOrNumber)`) and struct-form variants (e.g. `AccountChangesetNotFound { block_number, address }`). Use the struct form for the three new variants to keep field names visible at call sites.

- [ ] **Step 1.1: Add the three variants**

Find the line `BestBlockNotFound,` (around line 79) and add the three new variants immediately after the existing `HeaderNotFound`/`TransactionNotFound`/`ReceiptNotFound`/`BestBlockNotFound`/`FinalizedBlockNotFound`/`SafeBlockNotFound` cluster. Insert before `/// Mismatch of sender and transaction` (or whatever follows that cluster — search for the first `#[error` after `BestBlockNotFound` and place the new variants right before it).

```rust
    /// TrieDB pathdb is ahead of MDBX — invariant violation, not automatically
    /// recoverable.
    #[error(
        "triedb pathdb (block #{pathdb_block}) is ahead of mdbx tip (block #{mdbx_tip}); \
         this invariant is maintained by save_blocks ordering and cannot be repaired \
         automatically — restore pathdb from snapshot or resync"
    )]
    TriedbAheadOfMdbx {
        /// Block number reported by pathdb `latest_persist_state`.
        pathdb_block: BlockNumber,
        /// MDBX tip as reported by `last_block_number`.
        mdbx_tip: BlockNumber,
    },
    /// TrieDB pathdb root does not match the header state root at `pathdb_block` —
    /// one of the two backends is corrupted.
    #[error(
        "triedb/mdbx state root mismatch at block #{block}: triedb={triedb_root:?}, \
         mdbx header={mdbx_root:?}"
    )]
    TriedbMdbxRootMismatch {
        /// Block number where the mismatch was detected.
        block: BlockNumber,
        /// State root as reported by pathdb.
        triedb_root: B256,
        /// State root from the MDBX header at `block`.
        mdbx_root: B256,
    },
    /// Gap between MDBX tip and TrieDB pathdb tip exceeds the hard safety limit.
    #[error(
        "startup alignment refused: mdbx tip #{mdbx_tip} is {gap} blocks ahead of \
         triedb pathdb #{pathdb_block} (limit {limit}); verify pathdb path / chain \
         config or run `reth stage unwind` explicitly before restarting"
    )]
    StartupUnwindExceedsLimit {
        /// MDBX tip.
        mdbx_tip: BlockNumber,
        /// TrieDB pathdb tip.
        pathdb_block: BlockNumber,
        /// `mdbx_tip - pathdb_block`.
        gap: u64,
        /// The hard limit (`MAX_STARTUP_UNWIND_BLOCKS`).
        limit: u64,
    },
```

- [ ] **Step 1.2: Verify it compiles**

Run: `cargo check -p reth-storage-errors`
Expected: no errors, no warnings.

- [ ] **Step 1.3: Commit**

```bash
git add crates/storage/errors/src/provider.rs
git commit -m "$(cat <<'EOF'
feat(errors): add ProviderError variants for startup alignment

Add TriedbAheadOfMdbx, TriedbMdbxRootMismatch, and
StartupUnwindExceedsLimit variants to ProviderError. Used by the
startup MDBX/TrieDB alignment step to report backend-disagreement
failure modes with actionable messages.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Pure alignment-decision module with unit tests

**Files:**
- Create: `crates/node/builder/src/launch/alignment.rs`
- Modify: `crates/node/builder/src/launch/mod.rs`

**Context:** The live code must make a decision based on `(pathdb_block, pathdb_root, mdbx_tip, mdbx_root_at_pathdb_block, max_unwind)`. By separating this decision from I/O, we can unit-test every branch without building a full `LaunchContextWith` harness. The wrapper in Task 3 supplies the I/O.

- [ ] **Step 2.1: Write the failing unit tests**

Create `crates/node/builder/src/launch/alignment.rs` with only test stubs first, so the compile failure names every symbol we must define:

```rust
//! Pure decision logic for aligning MDBX to TrieDB at startup.
//!
//! See `docs/superpowers/specs/2026-04-17-triedb-mdbx-startup-alignment-design.md`
//! for context. The I/O wrapper lives in `common.rs`.

use alloy_primitives::{BlockNumber, B256};

/// Hard cap on how many blocks we are willing to unwind MDBX during startup
/// alignment. Larger gaps almost certainly indicate a misconfiguration (wrong
/// pathdb directory, mixed chains) rather than a normal crash window, and must
/// be resolved by the operator.
pub(crate) const MAX_STARTUP_UNWIND_BLOCKS: u64 = 1024;

/// Outcome of evaluating the startup MDBX/TrieDB alignment state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlignmentOutcome {
    /// `mdbx_tip == pathdb_block`. No work required.
    Aligned,
    /// `mdbx_tip > pathdb_block` and all safety checks passed. Caller must
    /// unwind MDBX (and static files + stage checkpoints) down to
    /// `pathdb_block`.
    NeedsUnwind {
        /// Block to unwind MDBX to (exclusive upper bound for removal).
        to: BlockNumber,
    },
    /// `mdbx_tip < pathdb_block`. Invariant violation; caller must fail hard.
    TriedbAhead {
        pathdb_block: BlockNumber,
        mdbx_tip: BlockNumber,
    },
    /// `mdbx_tip - pathdb_block > MAX_STARTUP_UNWIND_BLOCKS`. Caller must fail
    /// hard and tell the operator to investigate.
    ExceedsLimit {
        mdbx_tip: BlockNumber,
        pathdb_block: BlockNumber,
        gap: u64,
        limit: u64,
    },
    /// `pathdb_root != mdbx_header.state_root()` at `pathdb_block`. Caller
    /// must fail hard; data integrity problem.
    RootMismatch {
        block: BlockNumber,
        triedb_root: B256,
        mdbx_root: B256,
    },
}

/// Evaluate alignment purely from its inputs. No I/O.
///
/// `mdbx_root_at_pathdb_block` is `None` only when MDBX does not have a header
/// at `pathdb_block` (unexpected — caller should convert to a `HeaderNotFound`
/// before invoking this function). If it is provided but does not match
/// `pathdb_root`, the returned outcome is `RootMismatch`.
pub(crate) fn decide_startup_alignment(
    pathdb_block: BlockNumber,
    pathdb_root: B256,
    mdbx_tip: BlockNumber,
    mdbx_root_at_pathdb_block: B256,
    max_unwind: u64,
) -> AlignmentOutcome {
    use core::cmp::Ordering;

    match mdbx_tip.cmp(&pathdb_block) {
        Ordering::Equal => AlignmentOutcome::Aligned,
        Ordering::Less => AlignmentOutcome::TriedbAhead { pathdb_block, mdbx_tip },
        Ordering::Greater => {
            let gap = mdbx_tip - pathdb_block;
            if gap > max_unwind {
                return AlignmentOutcome::ExceedsLimit {
                    mdbx_tip,
                    pathdb_block,
                    gap,
                    limit: max_unwind,
                };
            }
            if pathdb_root != mdbx_root_at_pathdb_block {
                return AlignmentOutcome::RootMismatch {
                    block: pathdb_block,
                    triedb_root: pathdb_root,
                    mdbx_root: mdbx_root_at_pathdb_block,
                };
            }
            AlignmentOutcome::NeedsUnwind { to: pathdb_block }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn equal_tips_return_aligned() {
        let r = root(1);
        assert_eq!(
            decide_startup_alignment(100, r, 100, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::Aligned
        );
    }

    #[test]
    fn mdbx_behind_returns_triedb_ahead() {
        let r = root(2);
        assert_eq!(
            decide_startup_alignment(100, r, 95, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::TriedbAhead { pathdb_block: 100, mdbx_tip: 95 }
        );
    }

    #[test]
    fn gap_within_limit_and_root_matches_returns_needs_unwind() {
        let r = root(3);
        assert_eq!(
            decide_startup_alignment(100, r, 105, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::NeedsUnwind { to: 100 }
        );
    }

    #[test]
    fn gap_at_exactly_limit_is_allowed() {
        let r = root(4);
        let tip = MAX_STARTUP_UNWIND_BLOCKS;
        assert_eq!(
            decide_startup_alignment(0, r, tip, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::NeedsUnwind { to: 0 }
        );
    }

    #[test]
    fn gap_exceeds_limit_returns_exceeds_limit() {
        let r = root(5);
        let tip = MAX_STARTUP_UNWIND_BLOCKS + 1;
        assert_eq!(
            decide_startup_alignment(0, r, tip, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::ExceedsLimit {
                mdbx_tip: tip,
                pathdb_block: 0,
                gap: tip,
                limit: MAX_STARTUP_UNWIND_BLOCKS,
            }
        );
    }

    #[test]
    fn root_mismatch_takes_precedence_over_unwind() {
        let triedb_r = root(6);
        let mdbx_r = root(7);
        assert_eq!(
            decide_startup_alignment(100, triedb_r, 105, mdbx_r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::RootMismatch {
                block: 100,
                triedb_root: triedb_r,
                mdbx_root: mdbx_r,
            }
        );
    }

    #[test]
    fn exceeds_limit_takes_precedence_over_root_mismatch() {
        // If the gap is absurdly large we refuse to act, regardless of whether
        // the pathdb root looks corrupt — the operator should investigate both.
        let triedb_r = root(8);
        let mdbx_r = root(9);
        let tip = MAX_STARTUP_UNWIND_BLOCKS + 1;
        assert_eq!(
            decide_startup_alignment(0, triedb_r, tip, mdbx_r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::ExceedsLimit {
                mdbx_tip: tip,
                pathdb_block: 0,
                gap: tip,
                limit: MAX_STARTUP_UNWIND_BLOCKS,
            }
        );
    }
}
```

- [ ] **Step 2.2: Wire the module into the launch mod tree**

Edit `crates/node/builder/src/launch/mod.rs`. Find the line `pub(crate) mod debug;` and add right after it:

```rust
pub(crate) mod alignment;
```

- [ ] **Step 2.3: Verify tests fail to build**

Run: `cargo test -p reth-node-builder --lib launch::alignment::tests --no-run`
Expected: build succeeds, tests compile. (The file was written complete in Step 2.1; this step is present to match TDD cadence. If compilation fails, fix syntax before continuing.)

- [ ] **Step 2.4: Run the tests and verify they pass**

Run: `cargo test -p reth-node-builder --lib launch::alignment::tests`
Expected: 7 tests pass.

- [ ] **Step 2.5: Commit**

```bash
git add crates/node/builder/src/launch/alignment.rs crates/node/builder/src/launch/mod.rs
git commit -m "$(cat <<'EOF'
feat(node-builder): add startup alignment decision module

Introduce a pure decision function decide_startup_alignment covering
all startup MDBX/TrieDB alignment branches: Aligned, NeedsUnwind,
TriedbAhead, ExceedsLimit, RootMismatch. Includes MAX_STARTUP_UNWIND_BLOCKS
= 1024. Unit tests cover every branch and boundary (gap == limit,
precedence between branches).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: I/O wrapper `align_mdbx_to_triedb_at_startup`

**Files:**
- Modify: `crates/node/builder/src/launch/common.rs`

**Context:** The wrapper queries the live provider and TrieDB, calls `decide_startup_alignment`, and acts on the result. It must be available on the same `impl` block that already exposes `check_pipeline_consistency` — look for `pub fn check_pipeline_consistency(&self) -> ProviderResult<Option<B256>>` (around line 1088). The new method belongs directly above it.

The method hierarchy is:

```rust
impl<T, CB> LaunchContextWith<Attached<WithConfigs<T::ChainSpec>, WithComponents<T, CB>>>
where
    T: FullNodeTypes<Types: ProviderNodeTypes<ChainSpec: EthereumHardforks>>,
    CB: NodeComponentsBuilder<T>,
{
    // ... existing methods ...
}
```

The provider exposes:
- `self.blockchain_db().last_block_number()` → `ProviderResult<BlockNumber>` (via `BlockNumReader`)
- `self.blockchain_db().header_by_number(n)` → `ProviderResult<Option<Header>>` (via `HeaderProvider`)
- `self.blockchain_db().database_provider_rw()` → `ProviderResult<impl BlockExecutionWriter + DatabaseProvider>` (via `DatabaseProviderFactory`)

Headers expose `state_root()` via `AlloyBlockHeader` (already imported).

- [ ] **Step 3.1: Add the `BlockExecutionWriter` import**

Edit the `reth_provider::{...}` use statement near the top of `common.rs` (around line 68). Add `BlockExecutionWriter` to the list:

```rust
use reth_provider::{
    providers::{NodeTypesForProvider, ProviderNodeTypes, RocksDBProvider, StaticFileProvider},
    BlockExecutionWriter, BlockHashReader, BlockNumReader, BlockReaderIdExt,
    DatabaseProviderFactory, HeaderProvider, ProviderError, ProviderFactory, ProviderResult,
    RocksDBProviderFactory, StageCheckpointReader, StaticFileProviderBuilder,
    StaticFileProviderFactory,
};
```

- [ ] **Step 3.2: Add the new method above `check_pipeline_consistency`**

Find `/// Check if the pipeline is consistent (all stages have the checkpoint block numbers no less` (around line 1077 — the doc comment for `check_pipeline_consistency`). Insert the following method *immediately before* that doc comment block, on the same `impl`:

```rust
    /// Align MDBX to TrieDB pathdb at startup.
    ///
    /// When TrieDB is active, this reads pathdb's persisted tip
    /// (`latest_persist_state`) and MDBX's tip (`last_block_number`). If MDBX
    /// is ahead, it unwinds MDBX (state + blocks + static files + stage
    /// checkpoints) down to the pathdb tip via
    /// [`BlockExecutionWriter::remove_block_and_execution_above`], committing
    /// atomically. Equal tips are a no-op.
    ///
    /// Fails hard on three backend-disagreement cases:
    /// - TrieDB ahead of MDBX (invariant violated; not automatically recoverable)
    /// - Gap > [`alignment::MAX_STARTUP_UNWIND_BLOCKS`]
    /// - `pathdb_root` does not match the MDBX header state root at `pathdb_block`
    ///
    /// Must be called before any pipeline or engine work — i.e. before
    /// `check_pipeline_consistency` and before the engine service is built.
    /// Called from [`Self::initial_backfill_target`].
    pub fn align_mdbx_to_triedb_at_startup(&self) -> ProviderResult<()> {
        use crate::launch::alignment::{
            decide_startup_alignment, AlignmentOutcome, MAX_STARTUP_UNWIND_BLOCKS,
        };

        if !is_triedb_active() {
            return Ok(());
        }

        let triedb = rust_eth_triedb::get_global_triedb();
        let (pathdb_block, pathdb_root) =
            triedb.latest_persist_state().map_err(ProviderError::other)?;

        let mdbx_tip = self.blockchain_db().last_block_number()?;

        // Fast-path Aligned / TriedbAhead without asking for a header — we
        // don't need one unless we're about to unwind.
        if mdbx_tip == pathdb_block {
            info!(
                target: "reth::cli",
                mdbx_tip,
                pathdb_block,
                gap = 0u64,
                outcome = "noop",
                "Startup alignment: backends already in sync"
            );
            return Ok(());
        }
        if mdbx_tip < pathdb_block {
            error!(
                target: "reth::cli",
                mdbx_tip,
                pathdb_block,
                outcome = "failed:triedb_ahead",
                "Startup alignment: triedb pathdb is ahead of mdbx — aborting"
            );
            return Err(ProviderError::TriedbAheadOfMdbx { pathdb_block, mdbx_tip });
        }

        // mdbx_tip > pathdb_block: need a header at pathdb_block to validate
        // roots before unwinding.
        let mdbx_root_at_pathdb_block = self
            .blockchain_db()
            .header_by_number(pathdb_block)?
            .ok_or_else(|| ProviderError::HeaderNotFound(pathdb_block.into()))?
            .state_root();

        let outcome = decide_startup_alignment(
            pathdb_block,
            pathdb_root,
            mdbx_tip,
            mdbx_root_at_pathdb_block,
            MAX_STARTUP_UNWIND_BLOCKS,
        );

        match outcome {
            AlignmentOutcome::Aligned => {
                // Already handled above; unreachable in practice but harmless.
                Ok(())
            }
            AlignmentOutcome::TriedbAhead { pathdb_block, mdbx_tip } => {
                Err(ProviderError::TriedbAheadOfMdbx { pathdb_block, mdbx_tip })
            }
            AlignmentOutcome::ExceedsLimit { mdbx_tip, pathdb_block, gap, limit } => {
                error!(
                    target: "reth::cli",
                    mdbx_tip,
                    pathdb_block,
                    gap,
                    limit,
                    outcome = "failed:exceeds_limit",
                    "Startup alignment: gap exceeds safety limit — aborting"
                );
                Err(ProviderError::StartupUnwindExceedsLimit {
                    mdbx_tip,
                    pathdb_block,
                    gap,
                    limit,
                })
            }
            AlignmentOutcome::RootMismatch { block, triedb_root, mdbx_root } => {
                error!(
                    target: "reth::cli",
                    block,
                    ?triedb_root,
                    ?mdbx_root,
                    outcome = "failed:root_mismatch",
                    "Startup alignment: pathdb root disagrees with mdbx header — aborting"
                );
                Err(ProviderError::TriedbMdbxRootMismatch { block, triedb_root, mdbx_root })
            }
            AlignmentOutcome::NeedsUnwind { to } => {
                let gap = mdbx_tip - to;
                info!(
                    target: "reth::cli",
                    mdbx_tip,
                    pathdb_block = to,
                    gap,
                    "Startup alignment: unwinding MDBX to match TrieDB pathdb tip"
                );

                let provider_rw = self.blockchain_db().database_provider_rw()?;
                provider_rw.remove_block_and_execution_above(to)?;
                provider_rw.commit()?;

                info!(
                    target: "reth::cli",
                    new_tip = to,
                    gap,
                    outcome = "unwound",
                    "Startup alignment: MDBX unwound; proceeding"
                );
                Ok(())
            }
        }
    }
```

- [ ] **Step 3.3: Call alignment at the top of `initial_backfill_target`**

Find `pub fn initial_backfill_target(&self) -> ProviderResult<Option<B256>> {` (around line 1036). Replace the body:

Old:
```rust
    pub fn initial_backfill_target(&self) -> ProviderResult<Option<B256>> {
        let mut initial_target = self.node_config().debug.tip;

        if initial_target.is_none() {
            initial_target = self.check_pipeline_consistency()?;
        }

        Ok(initial_target)
    }
```

New:
```rust
    pub fn initial_backfill_target(&self) -> ProviderResult<Option<B256>> {
        // Make MDBX and TrieDB agree on the canonical tip before anything else
        // consults disk state. After this call either returns Ok, the two
        // backends are in sync (or TrieDB is inactive).
        self.align_mdbx_to_triedb_at_startup()?;

        let mut initial_target = self.node_config().debug.tip;

        if initial_target.is_none() {
            initial_target = self.check_pipeline_consistency()?;
        }

        Ok(initial_target)
    }
```

- [ ] **Step 3.4: Verify it compiles**

Run: `cargo check -p reth-node-builder`
Expected: no errors. Warnings that predate this change are acceptable; new warnings from this change are not.

- [ ] **Step 3.5: Run the whole crate's tests to confirm no regression**

Run: `cargo nextest run -p reth-node-builder`
Expected: all tests pass, including the 7 new alignment unit tests.

- [ ] **Step 3.6: Commit**

```bash
git add crates/node/builder/src/launch/common.rs
git commit -m "$(cat <<'EOF'
feat(node-builder): unwind MDBX to TrieDB tip at startup

Add align_mdbx_to_triedb_at_startup, invoked at the top of
initial_backfill_target before the pipeline consistency check. When
pathdb lags MDBX (e.g. post-crash during save_blocks' deferred flush
window), we unwind MDBX — state, blocks, static files, and all stage
checkpoints — down to pathdb's tip via remove_block_and_execution_above,
committing atomically. Fails hard when pathdb is ahead of MDBX, when the
gap exceeds MAX_STARTUP_UNWIND_BLOCKS (1024), or when pathdb_root does
not match the MDBX header state root at pathdb_block.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Remove now-unreachable branches in `check_pipeline_consistency_under_triedb`

**Files:**
- Modify: `crates/node/builder/src/launch/common.rs`

**Context:** After alignment runs at the top of `initial_backfill_target`, the "MDBX tip != TrieDB tip" branches inside `check_pipeline_consistency_under_triedb` are dead code. Remove them and add a doc comment explaining that equal tips are now a precondition. The per-stage checkpoint loop (which detects pipeline-interrupt inconsistencies) stays — that is a different concern (a crash mid-pipeline-sync before TrieDB is ever written) and remains necessary.

- [ ] **Step 4.1: Rewrite the function**

Find `pub fn check_pipeline_consistency_under_triedb(&self) -> ProviderResult<Option<B256>> {` (around line 1146). Replace the entire function through its closing brace (`}`) with:

```rust
    /// Check if the pipeline is consistent under TrieDB.
    ///
    /// **Precondition:** [`Self::align_mdbx_to_triedb_at_startup`] has already
    /// run and guarantees `mdbx_tip == pathdb_tip`. This function therefore
    /// only needs to detect *pipeline-interrupt* inconsistencies — stages
    /// whose checkpoints trail the first stage because a previous pipeline run
    /// died mid-flight (e.g. before TrieDB was ever written). Backend
    /// cross-alignment is not handled here; the earlier alignment step is the
    /// only place that unwinds MDBX for that reason.
    pub fn check_pipeline_consistency_under_triedb(&self) -> ProviderResult<Option<B256>> {
        // If no target was provided, check if the stages are congruent - check if the
        // checkpoint of the last stage matches the checkpoint of the first.
        let mut first_stage_checkpoint = self
            .blockchain_db()
            .get_stage_checkpoint(*StageId::ALL.first().unwrap())?
            .unwrap_or_default()
            .block_number;

        let triedb = rust_eth_triedb::get_global_triedb();
        let (triedb_checkpoint_block_number, _triedb_checkpoint_state_root) =
            triedb.latest_persist_state().map_err(ProviderError::other)?;

        // Skip the first stage as we've already retrieved it and comparing all other
        // checkpoints against it.
        for stage_id in &StageId::ALL {
            let stage_checkpoint = self
                .blockchain_db()
                .get_stage_checkpoint(*stage_id)?
                .unwrap_or_default()
                .block_number;

            // If the checkpoint of any stage is less than the checkpoint of the first
            // stage, retrieve and return the block hash of the latest header and use
            // it as the target.
            if stage_checkpoint < first_stage_checkpoint {
                if triedb_checkpoint_block_number > first_stage_checkpoint {
                    info!(
                        target: "consensus::engine",
                        triedb_checkpoint_block_number,
                        first_stage_checkpoint,
                        "TrieDB checkpoint is ahead of the first stage checkpoint, using TrieDB checkpoint as the target"
                    );
                    first_stage_checkpoint = triedb_checkpoint_block_number;
                }
                info!(
                    target: "consensus::engine",
                    first_stage_checkpoint,
                    inconsistent_stage_id = %stage_id,
                    inconsistent_stage_checkpoint = stage_checkpoint,
                    "Pipeline sync progress is inconsistent"
                );
                return self.blockchain_db().block_hash(first_stage_checkpoint);
            }
        }

        info!(
            target: "consensus::engine",
            "Pipeline sync progress is consistent and backends are aligned; starting live sync"
        );

        Ok(None)
    }
```

What was removed: the final block that compared `last_persisted_block_number` to `triedb_checkpoint_block_number` and returned the MDBX tip as a backfill target in either direction. That code is now unreachable because `align_mdbx_to_triedb_at_startup` makes the two equal before this function runs.

What was changed but preserved:
- `.unwrap()` on `latest_persist_state()` replaced with `.map_err(ProviderError::other)?` — that was a latent panic; take the opportunity to do it right.
- `_triedb_checkpoint_state_root` prefix because the variable is no longer referenced.

- [ ] **Step 4.2: Verify it compiles**

Run: `cargo check -p reth-node-builder`
Expected: no errors. No new warnings.

- [ ] **Step 4.3: Run the crate tests**

Run: `cargo nextest run -p reth-node-builder`
Expected: all tests pass.

- [ ] **Step 4.4: Commit**

```bash
git add crates/node/builder/src/launch/common.rs
git commit -m "$(cat <<'EOF'
refactor(node-builder): drop unreachable branches from triedb consistency check

After align_mdbx_to_triedb_at_startup runs at the top of
initial_backfill_target, check_pipeline_consistency_under_triedb can no
longer observe mdbx_tip != pathdb_tip — those branches returned the MDBX
tip as a backfill target, which never triggered any real work because
save_blocks had already set every stage checkpoint to that tip. Remove
them; keep the per-stage checkpoint loop (still needed for pipeline
interrupts) and replace a latent .unwrap() on latest_persist_state with
proper error propagation.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Metrics

**Files:**
- Modify: `crates/node/builder/src/launch/common.rs`

**Context:** The spec calls for a counter `reth_startup_alignment_unwinds_total` and a gauge `reth_startup_alignment_last_gap`. The codebase uses the `metrics` crate for ad-hoc metric emission (see existing `metrics::counter!`/`metrics::gauge!` call sites; the crate is a workspace dep). Keep this minimal — emit inside the `NeedsUnwind` arm and once at the top to set the gauge to the observed gap (even on noop = 0) so dashboards can show "how far behind was pathdb at last startup."

- [ ] **Step 5.1: Emit gauge + counter inside `align_mdbx_to_triedb_at_startup`**

Inside the method body added in Task 3, add two metric calls. Gauge is recorded once, right after `mdbx_tip` is computed; counter is incremented only in the `NeedsUnwind` success arm.

First, find this block in the method (added in Step 3.2):

```rust
        let mdbx_tip = self.blockchain_db().last_block_number()?;

        // Fast-path Aligned / TriedbAhead without asking for a header — we
```

Replace with:

```rust
        let mdbx_tip = self.blockchain_db().last_block_number()?;

        let gap = mdbx_tip.saturating_sub(pathdb_block);
        metrics::gauge!("reth_startup_alignment_last_gap").set(gap as f64);

        // Fast-path Aligned / TriedbAhead without asking for a header — we
```

Then find the successful unwind arm in the same method:

```rust
            AlignmentOutcome::NeedsUnwind { to } => {
                let gap = mdbx_tip - to;
                info!(
                    target: "reth::cli",
                    mdbx_tip,
                    pathdb_block = to,
                    gap,
                    "Startup alignment: unwinding MDBX to match TrieDB pathdb tip"
                );

                let provider_rw = self.blockchain_db().database_provider_rw()?;
                provider_rw.remove_block_and_execution_above(to)?;
                provider_rw.commit()?;
```

Add `metrics::counter!("reth_startup_alignment_unwinds_total").increment(1);` after `provider_rw.commit()?;`:

```rust
            AlignmentOutcome::NeedsUnwind { to } => {
                let gap = mdbx_tip - to;
                info!(
                    target: "reth::cli",
                    mdbx_tip,
                    pathdb_block = to,
                    gap,
                    "Startup alignment: unwinding MDBX to match TrieDB pathdb tip"
                );

                let provider_rw = self.blockchain_db().database_provider_rw()?;
                provider_rw.remove_block_and_execution_above(to)?;
                provider_rw.commit()?;
                metrics::counter!("reth_startup_alignment_unwinds_total").increment(1);
```

Note: the gauge is set even when `mdbx_tip < pathdb_block` — `saturating_sub` yields 0, which is harmless and keeps the gauge defined on every startup.

- [ ] **Step 5.2: Verify `metrics` is already a dep of the crate**

Run: `cargo check -p reth-node-builder`
Expected: no "unresolved import" for `metrics`. If it errors, that means the dep is transitive-only; in that case add `metrics = { workspace = true }` to `crates/node/builder/Cargo.toml` under `[dependencies]` and retry.

- [ ] **Step 5.3: Commit**

```bash
git add crates/node/builder/src/launch/common.rs
# If Cargo.toml was modified in 5.2:
# git add crates/node/builder/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(node-builder): emit startup alignment metrics

Add reth_startup_alignment_last_gap gauge (observed on every startup,
including noops) and reth_startup_alignment_unwinds_total counter
(incremented after a successful unwind). Enables operators to alert on
repeated post-crash alignments and on growing gaps.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Formatting + full lint pass

**Files:**
- (None modified; verification only)

- [ ] **Step 6.1: Format**

Run: `cargo +nightly fmt --all`
Expected: no diff on files not touched by this plan. The touched files may reformat; inspect with `git diff` and re-commit only if the formatter changed something substantive.

- [ ] **Step 6.2: Clippy**

Run: `RUSTFLAGS="-D warnings" cargo +nightly clippy -p reth-node-builder -p reth-storage-errors --all-features --locked`
Expected: no warnings, no errors.

- [ ] **Step 6.3: If anything is reformatted, commit**

```bash
git diff --name-only
# If files touched by this plan have formatter diffs:
git add <files>
git commit -m "style: cargo fmt after startup alignment changes"
```

If there is nothing to commit, skip this step.

---

## Verification Checklist

Run once, after Task 6:

- [ ] `cargo check -p reth-node-builder -p reth-storage-errors` — compiles clean
- [ ] `cargo nextest run -p reth-node-builder -E 'test(launch::alignment)'` — 7 pure-decision tests pass
- [ ] `cargo nextest run -p reth-node-builder` — no regression in the crate's existing suite
- [ ] `RUSTFLAGS="-D warnings" cargo +nightly clippy -p reth-node-builder -p reth-storage-errors --all-features --locked` — clean
- [ ] `cargo +nightly fmt --all --check` — clean

## Out-of-plan items (do not attempt in this PR)

- Runtime pathdb rewind in `on_remove_blocks_above` — lives in `crates/engine/tree/src/persistence.rs` and is orthogonal. See the spec's "Relationship to runtime pathdb rewind" section. Do not modify it.
- Tightening the `Err(_)` branch of the runtime pathdb rewind to fatal — separate follow-up PR.
- Engine-tree pathdb-gap guard optimization (it re-checks on every incoming block) — separate follow-up PR.

## Notes for the implementing engineer

- The one place `is_triedb_active` is imported is already wired in `common.rs` line 98–100. No new triedb imports needed in `common.rs` beyond `rust_eth_triedb::get_global_triedb` (which is referenced by full path in the method body, matching existing style at line 1155).
- `AlloyBlockHeader` is re-exported as `alloy_consensus::BlockHeader` and imported at the top of `common.rs` as `alloy_consensus::BlockHeader as _`. That import provides `state_root()` via trait method.
- `ProviderError::other(e)` is the existing adapter for non-reth error types (see its usage in `crates/storage/provider/src/providers/database/provider.rs`). Preferred over ad-hoc `Display`-based wrapping.
- Do not create a CLI flag for the limit. The spec explicitly rejects that.
- Do not log `pathdb_root`/`mdbx_root` on the happy path — they are only useful in the `RootMismatch` failure arm, where they are already logged.
