# TrieDB / MDBX Startup Alignment

**Status:** Draft
**Author:** will
**Date:** 2026-04-17
**Scope:** BSC reth fork (`bnb-chain/reth`), TrieDB mode only

## Problem

The persistence task commits MDBX before flushing TrieDB pathdb (see f22675185).
This preserves the invariant `pathdb_tip ≤ mdbx_tip` across crashes, but it
means a crash in the window between `provider_rw.commit()` and the deferred
`TriedbPendingFlush::apply()` can leave MDBX ahead of TrieDB by up to a
`PersistenceAction::SaveBlocks` batch of blocks.

On the next startup the gap currently goes **undetected at alignment time** and
**unfixed by the existing pipeline consistency check**:

- `DatabaseProvider::save_blocks` calls `update_pipeline_stages(last_block_number, false)`
  which sets *every* stage checkpoint to the MDBX tip in one shot.
- `check_pipeline_consistency_under_triedb` (`crates/node/builder/src/launch/common.rs:1146`)
  therefore sees all stages at the MDBX tip, finds no inconsistency in its
  stage-checkpoint loop, and even though it notices `mdbx_tip > triedb_tip` and
  returns the MDBX tip as a backfill target, the pipeline runs with every
  stage's `stage_progress == target` and executes no work. TrieDB stays behind.
- At live-sync time the engine tree's pathdb-gap guard
  (`crates/engine/tree/src/tree/mod.rs:2823`) then buffers incoming blocks as
  `Disconnected` and forces sequential P2P re-fetch of ancestors — slow and
  awkward when all needed blocks already exist in MDBX.

The node must not start live sync while the two backends disagree on tip.

## Goal

At startup, before any pipeline or engine work begins, **force MDBX and TrieDB
to agree on a single canonical tip** by unwinding MDBX (and its static files and
stage checkpoints) down to the TrieDB pathdb tip. Normal pipeline sync then
re-advances from there, re-executing blocks and re-flushing TrieDB along the
way.

Non-goal: recovering from TrieDB ahead of MDBX. That direction is impossible
without MDBX changesets, is ruled out by the write-order fix, and if observed
in practice indicates data corruption or external tampering — we fail hard and
let the operator decide.

## Design

### Placement

Add a new function `align_mdbx_to_triedb_at_startup()` in
`crates/node/builder/src/launch/common.rs`, on the same `LaunchContextWith`
impl as `check_pipeline_consistency`. Call it from `initial_backfill_target()`
**before** `check_pipeline_consistency()` runs, so the consistency check sees
an already-aligned state and needs no logic changes.

Rationale for a dedicated function rather than extending
`check_pipeline_consistency_under_triedb`:

- The consistency check is a read-only routine that returns a backfill target.
  Backend alignment is a *write* operation (MDBX unwind + commit). Mixing the
  two muddles the contract.
- Keeping alignment as a discrete step makes it trivial to test in isolation
  and trivial to disable/gate behind a flag later if needed.

### Algorithm

```text
align_mdbx_to_triedb_at_startup():
    if !is_triedb_active(): return Ok(())

    (pathdb_block, pathdb_root) = triedb.latest_persist_state()?
    mdbx_tip                    = blockchain_db.last_block_number()?

    if mdbx_tip == pathdb_block: return Ok(())  # already aligned

    if mdbx_tip < pathdb_block:
        # Invariant violated — unrecoverable.
        return Err(ProviderError::TriedbAheadOfMdbx { pathdb_block, mdbx_tip })

    # mdbx_tip > pathdb_block
    gap = mdbx_tip - pathdb_block
    if gap > MAX_STARTUP_UNWIND_BLOCKS:     # 1024
        return Err(ProviderError::StartupUnwindExceedsLimit {
            mdbx_tip, pathdb_block, gap, limit: MAX_STARTUP_UNWIND_BLOCKS
        })

    # Sanity: pathdb_root must match the header at pathdb_block.
    header = blockchain_db.header_by_number(pathdb_block)?
        .ok_or(ProviderError::HeaderNotFound(pathdb_block.into()))?
    if header.state_root() != pathdb_root:
        return Err(ProviderError::TriedbMdbxRootMismatch {
            block: pathdb_block,
            triedb_root: pathdb_root,
            mdbx_root: header.state_root(),
        })

    info!(mdbx_tip, pathdb_block, gap,
          "Unwinding MDBX to TrieDB pathdb tip for startup alignment");

    provider_rw = blockchain_db.database_provider_rw()?
    provider_rw.remove_block_and_execution_above(pathdb_block)?
    provider_rw.commit()?

    info!(new_tip = pathdb_block,
          "MDBX aligned with TrieDB; proceeding to pipeline consistency check");

    Ok(())
```

### Why `remove_block_and_execution_above` is sufficient

The existing implementation at
`crates/storage/provider/src/providers/database/provider.rs:3363` already
performs in a single MDBX transaction:

1. `unwind_trie_state_from(block + 1)` — unwind MDBX merkle state
2. `remove_state_above(block)` — roll plaintext accounts/storage back via
   changesets
3. `remove_blocks_above(block)` — truncate headers, bodies, receipts (MDBX
   tables + static file segments)
4. `update_pipeline_stages(block, true)` — reset every stage checkpoint to
   `block` and drop per-stage progress

Atomic at MDBX level. Pathdb is intentionally not touched (we are aligning
*to* it, not past it).

### Constants & configuration

- `MAX_STARTUP_UNWIND_BLOCKS = 1024` — hard cap. No CLI flag to bypass.
  Larger gaps signal configuration drift (e.g. pointing at a stale pathdb
  directory, mixing chains) and must be resolved by the operator, not by
  silently discarding ~3 hours of BSC blocks.
- Constant lives in `crates/node/builder/src/launch/common.rs` next to the
  new function. Not a tunable today; can be promoted to config later if real
  workloads need it.

### Error types

Add two variants to `reth_provider::ProviderError`:

- `TriedbAheadOfMdbx { pathdb_block: BlockNumber, mdbx_tip: BlockNumber }` —
  the unrecoverable direction. `Display` message must explicitly tell the
  operator this is not automatically recoverable and suggest the remedies:
  restore pathdb from snapshot, or remove pathdb + MDBX and resync.
- `StartupUnwindExceedsLimit { mdbx_tip, pathdb_block, gap, limit }` —
  tripped safety rail. Message suggests verifying pathdb path / chain config,
  and in legitimate catastrophic-gap cases performing an explicit
  `reth stage unwind` before restarting.

Use the existing `ProviderError::HeaderNotFound(_)` for missing-header cases
and a new `TriedbMdbxRootMismatch { block, triedb_root, mdbx_root }` for the
integrity-check miss.

All three are fatal at startup. None are retried automatically.

### Cleanup in `check_pipeline_consistency_under_triedb`

Once alignment runs first, the `last_persisted_block_number != triedb_checkpoint_block_number`
branches in `check_pipeline_consistency_under_triedb` become unreachable (the
alignment step has already made them equal or failed hard). Delete both
branches as part of this change, leaving the function with:

1. The stage-vs-first-stage checkpoint loop (still needed: detects
   *pipeline-interrupt* inconsistencies, which are orthogonal to backend
   alignment — e.g. a crash mid-pipeline-sync before TrieDB is ever written).
2. The final "aligned, start live sync" log and `Ok(None)` return.

Add a short doc comment at the top of the function pointing to
`align_mdbx_to_triedb_at_startup` and explaining that equal tips are a
precondition maintained by that function.

### Ordering relative to other startup steps

`align_mdbx_to_triedb_at_startup` must run:

- **After** TrieDB is initialized (needs `get_global_triedb` to be live).
- **After** static-file providers and the blockchain DB are ready.
- **Before** `initial_backfill_target()` is used — i.e. before any pipeline
  backfill is started and before `EngineService` is constructed.

Concretely, call it at the top of `initial_backfill_target()` (just after the
`debug.tip` short-circuit). That keeps one single call-site and matches the
existing invariant that backfill decisions are made off a clean disk state.

### Canonical in-memory state

Alignment runs before any `CanonicalInMemoryState` is populated from engine
tree activity, so no in-memory invalidation is required. The next persistence
action that would touch memory will see the new (lower) MDBX tip and behave
correctly.

## Failure modes and recovery

| Situation | Detected as | Behaviour |
|---|---|---|
| `pathdb_tip == mdbx_tip` | `Equal` branch | No-op, continue startup |
| `mdbx_tip > pathdb_tip`, gap ≤ 1024, root matches | success path | Unwind, log, continue |
| `mdbx_tip > pathdb_tip`, gap > 1024 | `StartupUnwindExceedsLimit` | Fatal. Operator must investigate (wrong pathdb path? chain mismatch?) and either manually unwind or resync |
| `mdbx_tip > pathdb_tip`, root mismatch at `pathdb_block` | `TriedbMdbxRootMismatch` | Fatal. Indicates one of the two backends is corrupted |
| `mdbx_tip < pathdb_tip` | `TriedbAheadOfMdbx` | Fatal. Invariant violation; no safe automatic recovery |
| TrieDB not active | early return | No-op |

No partial-failure state: `remove_block_and_execution_above` + `commit` is
atomic at MDBX. If the node crashes *during* the alignment commit, the next
startup simply re-runs alignment from the same pre-crash state — idempotent.

## Testing

Unit tests in `crates/node/builder/src/launch/common.rs` (or a new
`alignment.rs` module):

1. **no-op when aligned** — both backends at same tip → function returns Ok,
   no provider writes.
2. **forward MDBX unwind** — seed MDBX at tip N, TrieDB at tip N-5 with
   matching root → function unwinds MDBX; assert `last_block_number == N-5`,
   stage checkpoints all at N-5, static files truncated.
3. **gap exceeds limit** — MDBX at N, TrieDB at N-2000 → returns
   `StartupUnwindExceedsLimit`; MDBX untouched.
4. **root mismatch** — MDBX at N, TrieDB at N-5 with non-matching root →
   returns `TriedbMdbxRootMismatch`; MDBX untouched.
5. **TrieDB ahead** — MDBX at N-5, TrieDB at N → returns `TriedbAheadOfMdbx`;
   MDBX untouched.
6. **TrieDB inactive** — function returns Ok immediately without touching the
   provider.

Integration: add a scenario to the engine-tree persistence test suite that
exercises "save batch → inject failure between MDBX commit and TrieDB flush →
restart → align → verify live sync resumes and produces matching roots."

## Observability

On every alignment run emit a single INFO log with:
`mdbx_tip_before`, `pathdb_tip`, `gap`, `outcome=[noop|unwound|failed:<reason>]`.
Failures additionally log at ERROR with the structured error fields so
runbook automation can scrape them.

Add one counter metric `reth_startup_alignment_unwinds_total` and a gauge
`reth_startup_alignment_last_gap` for operators to alert on repeated
alignments after crashes.

## Out of scope

- Concurrent MDBX readers during alignment (none exist — this runs before
  the engine or RPC start).
- Aligning MDBX forward to match a newer TrieDB. Unrecoverable by design.
- CLI command for manual alignment. The startup path is the only supported
  entry today; if we later want an explicit command, it can delegate to the
  same function.

## Follow-ups (separate PRs)

- Promote `MAX_STARTUP_UNWIND_BLOCKS` to config if real workloads demand it.
- Extend the pathdb-gap guard in the engine tree to trust a freshly aligned
  backend (today it re-checks on every incoming block, which is harmless but
  costs one triedb read per block).
