# Engine Graceful-Shutdown Flush

**Status:** Draft
**Author:** will
**Date:** 2026-04-20
**Scope:** reth fork (`bnb-chain/reth`), all node modes; BSC validators are the primary motivation but the fix applies generally to `reth-node-builder`'s engine-tree launcher.

## Problem

On `SIGTERM` / `Ctrl-C`, the engine-tree's in-memory canonical blocks above
`last_persisted_block` are **never persisted** before the process exits. The
mechanism to persist them exists and is covered by an e2e test, but it is
**never triggered from the signal path in production**. For a BSC validator
running with `--engine.persistence-threshold 256
--engine.memory-block-buffer-target 128`, every graceful kill regresses the
canonical tip by 128–257 blocks on the next startup.

### Write-path recap

- Every canonical commit only updates engine-tree's in-memory `TreeState`
  (`crates/engine/tree/src/tree/mod.rs` around `on_engine_message`).
- `advance_persistence()` (`crates/engine/tree/src/tree/mod.rs:1395`) triggers
  a batch `save_blocks` via the persistence worker when
  `canonical_head_number − last_persisted_block > persistence_threshold`
  (`should_persist`, `:1973`).
- Steady state: `last_persisted_block` lags `canonical_head` by
  `memory_block_buffer_target..(persistence_threshold + 1)` blocks. For the BSC
  config above, that is 128–257 blocks, unconditionally, by design.
- Startup reads the canonical tip from MDBX
  (`BlockchainProvider::last_block_number`), which equals the last successful
  `save_blocks` flush. Everything above is lost on an unclean process exit.

### Existing shutdown-flush plumbing (dormant)

- `EngineShutdown` handle in `crates/node/builder/src/rpc.rs:1470-1510` is
  explicitly documented to "persist all remaining in-memory blocks before the
  engine terminates". It is created in `crates/node/builder/src/launch/engine.rs:307`
  and wired into the consensus-engine `tokio::select!` at `:342-350` as
  `shutdown_rx`.
- When the consensus engine sees an `EngineShutdownRequest`, it forwards it
  into the engine-tree as `FromOrchestrator::Terminate { tx: done_tx }`, which
  drives `finish_termination()` (`crates/engine/tree/src/tree/mod.rs:1413-1421`)
  → `persist_until_complete()` (`:1424-1442`) → one final
  `get_canonical_blocks_to_persist(PersistTarget::Head)` +
  `persist_blocks()` loop until **every** canonical block up to head is on
  disk.
- There is a passing e2e test proving the mechanism works:
  `crates/ethereum/node/tests/e2e/eth.rs:170-186` asserts that
  `db_block_before == 0` and `db_block_after == 1` bracket a single call to
  `engine_shutdown.shutdown() + done_rx.await`.

### Why it never runs in production

Three independent code-path gaps conspire:

1. **No signal-to-shutdown wiring.** A repo-wide search for
   `engine_shutdown.shutdown(` turns up exactly one call site: the e2e test
   above. `bin/reth/src/main.rs`, `crates/cli/runner/src/lib.rs`, and every
   downstream node (`reth-bsc` included) never call it.
2. **The runner cancels the user closure on signal.**
   `run_until_ctrl_c()` (`crates/cli/runner/src/lib.rs:283-323`) uses a
   `tokio::select!` that treats the user future as just one branch:

   ```rust
   tokio::select! {
       _ = ctrl_c  => { ... }
       _ = sigterm => { ... }
       res = fut   => res?,   // user's launch-and-wait closure
   }
   ```

   When the signal arm wins, the user future is immediately dropped. Any
   shutdown logic a downstream consumer could try to install inside its
   closure never gets a chance to run.
3. **The consensus engine is a non-graceful spawn.**
   `crates/node/builder/src/launch/engine.rs:418`:

   ```rust
   ctx.task_executor().spawn_critical("consensus engine", Box::pin(consensus_engine));
   ```

   `spawn_critical` does **not** register a `GracefulShutdownGuard`. When
   `task_manager.graceful_shutdown_with_timeout` subsequently runs
   (`crates/cli/runner/src/lib.rs:83`), this task is dropped immediately — it
   does not get the `graceful_shutdown_timeout` window to drain. Even if we
   could call `engine_shutdown.shutdown()` from somewhere, the receiving task
   (`shutdown_rx`) lives inside the consensus engine future that has already
   been aborted.

Net effect: every real `SIGTERM` takes the path
*run_until_ctrl_c-cancels-closure → TaskManager drops consensus engine →
engine-tree `incoming` channel senders drop → engine-tree's `run()` loop exits
via the `Disconnected` branch (`crates/engine/tree/src/tree/mod.rs:559-562`),
which simply `return`s*. No final flush runs. In-memory blocks are lost.

### Observable symptom

On BSC qanet validators, the startup-alignment log now prints
`Startup alignment: backends already in sync mdbx_tip=<N> pathdb_block=<N>
gap=0 outcome="noop"` (from the 2026-04-17 alignment design), but `<N>` equals
the last `save_blocks` flush height, not the pre-kill canonical head. The
startup-alignment log correctly reports that MDBX and pathdb agree with each
other; what it cannot report is that both of them lag behind the real tip at
kill time by up to `persistence_threshold + 1` blocks.

## Goal

On graceful shutdown (`SIGTERM` / `Ctrl-C`), persist **every** in-memory
canonical block that engine-tree holds above `last_persisted_block`, so that
on next startup the canonical tip equals the pre-shutdown canonical head.

Non-interactive corollary: there must be no new dependency on any downstream
consumer (`reth-bsc` or other forks) wiring signals to `engine_shutdown`.
Everything needed lives inside reth.

## Non-goals

- **Unclean process exits.** `SIGKILL`, panics that bypass unwinding, OOM,
  hypervisor force-stop, power loss, kernel freezes. These require a
  persistent diff-layer journal (geth's `triesinmemory.journal`) or a
  reverse-diff freezer; both are significantly larger changes and belong in
  follow-up work.
- **`--engine.persistence-threshold` and `--engine.memory-block-buffer-target`
  defaults.** Operators keep full control over the steady-state lag. This
  design only bounds regression on graceful kill; lowering the thresholds
  reduces regression on unclean kill and is orthogonal.
- **MDBX flush semantics.** `save_blocks` → `commit` already reaches disk
  synchronously; no change needed there.
- **TrieDB rewind / journal / reverse-diff freezer.** Out of scope.
- **Lowering the overall shutdown hard deadline.** Raising the graceful
  timeout is acceptable because the dominant task (this flush) is bounded by
  `persistence_threshold + 1` blocks of pathdb apply, which is the whole
  point. If that exceeds the tuned timeout, the deployment is mis-tuned;
  we surface it as a log, not as silent data loss.
- **New public API surface.** `EngineShutdown` already exists and is already
  re-exported via `RpcHandle`; keep it for programmatic triggers (tests,
  admin RPCs). This spec only adds an implicit trigger driven by the
  graceful-shutdown signal.

## Design

### Placement

Two files are touched:

1. `crates/node/builder/src/launch/engine.rs` — convert the `consensus engine`
   task from `spawn_critical` to
   `spawn_critical_with_graceful_shutdown_signal` and add a `graceful` branch
   to its `tokio::select!` that self-triggers the `Terminate` flow.
2. `crates/cli/runner/src/lib.rs` — raise `DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT`
   from 5 seconds to 60 seconds.

No new public types. `EngineShutdown` stays intact for RPC/test triggers.

### Part 1: Consensus engine as a graceful task

**Current shape** (`crates/node/builder/src/launch/engine.rs:326-418`,
elided):

```rust
let consensus_engine = async move {
    /* ...init and backfill kickoff... */
    let mut res = Ok(());
    let mut shutdown_rx = shutdown_rx.fuse();
    loop {
        tokio::select! {
            shutdown_req = &mut shutdown_rx => {
                if let Ok(req) = shutdown_req {
                    engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(
                        FromOrchestrator::Terminate { tx: req.done_tx }.into()
                    );
                }
            }
            payload = built_payloads.select_next_some() => { ... }
            req     = engine_api_rx.recv() => { ... }
            event   = engine_service.next() => { match event { ... break ... } }
        }
    }
    let _ = exit.send(res);
};
ctx.task_executor().spawn_critical("consensus engine", Box::pin(consensus_engine));
```

**New shape:**

```rust
ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
    "consensus engine",
    move |graceful| async move {
        /* ...same init and backfill kickoff... */
        let mut res = Ok(());
        let mut shutdown_rx = shutdown_rx.fuse();
        let mut graceful = std::pin::pin!(graceful);
        let mut terminating = false;
        loop {
            tokio::select! {
                // New arm: graceful shutdown triggered by TaskManager
                // (runner already captured SIGTERM/Ctrl-C).
                _guard = &mut graceful, if !terminating => {
                    debug!(target: "reth::cli", "graceful shutdown signal; triggering engine terminate");
                    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                    engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(
                        FromOrchestrator::Terminate { tx: done_tx }.into()
                    );
                    terminating = true;
                    // Wait here for engine-tree's finish_termination() to finish the
                    // final persist_until_complete() and send on done_tx.
                    // If something on engine-tree side fails before done_tx fires,
                    // done_rx returns Err(_), which we log and proceed to exit.
                    match done_rx.await {
                        Ok(()) => debug!(target: "reth::cli", "engine flush complete; exiting consensus loop"),
                        Err(err) => warn!(target: "reth::cli", %err, "engine shutdown done-channel closed before completion"),
                    }
                    break;
                }
                // Existing arm: explicit programmatic trigger via EngineShutdown RPC.
                shutdown_req = &mut shutdown_rx, if !terminating => {
                    if let Ok(req) = shutdown_req {
                        engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(
                            FromOrchestrator::Terminate { tx: req.done_tx }.into()
                        );
                        terminating = true;
                        // No await here — the explicit caller owns `done_rx`.
                    }
                }
                payload = built_payloads.select_next_some() => { ... unchanged ... }
                req     = engine_api_rx.recv()             => { ... unchanged ... }
                event   = engine_service.next()            => { ... unchanged ... }
            }
        }
        let _ = exit.send(res);
        // `graceful` guard drops here → TaskManager.graceful_tasks counter decrements,
        // allowing graceful_shutdown_with_timeout to see completion.
    },
);
```

Key properties:

- `graceful` is a `GracefulShutdown` future provided by the task executor. It
  completes the moment `TaskManager::graceful_shutdown_with_timeout` is
  invoked (i.e. **after** `run_until_ctrl_c` returns on signal, but before
  the runtime starts dropping tasks). At that point the consensus-engine task
  is still alive, still being polled, and has the full
  `graceful_shutdown_timeout` window to run the flush.
- The graceful arm constructs a fresh `oneshot::channel` and hands the
  `done_tx` to the engine-tree via `FromOrchestrator::Terminate`. This
  mirrors what the explicit `EngineShutdown` path does, without needing a
  call from outside the task.
- `terminating` guards the select to prevent double-trigger if both a
  programmatic `EngineShutdown::shutdown()` and a graceful signal arrive
  (harmless race, but cleanly handled: the first one wins, the second arm is
  disabled).
- The closure's return implicitly drops `_guard`, which decrements
  `TaskManager`'s `graceful_tasks` counter. This is the contract
  `graceful_shutdown_with_timeout` is waiting on.

Engine-tree's `finish_termination` / `persist_until_complete`
(`crates/engine/tree/src/tree/mod.rs:1413-1442`) is unchanged. It already:
- drains any in-flight `save_blocks`,
- pulls `PersistTarget::Head` (not `Threshold`) so the full range to canonical
  head is flushed,
- loops until empty,
- `send`s the `pending_termination` oneshot.

### Part 2: Default `graceful_shutdown_timeout`

`crates/cli/runner/src/lib.rs:221`:

```rust
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
```

Change to:

```rust
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
```

Rationale:

- The dominant work in graceful shutdown is now the final flush. At
  `persistence_threshold = 256` and `memory_block_buffer_target = 128`, that
  is up to 257 blocks of (a) MDBX `save_blocks` batch commit and (b) 257
  sequential `pending.apply()` calls into the pathdb RocksDB. Empirically
  this can exceed 5 seconds on debug builds or slow disks.
- 60 seconds is safely above realistic worst-case flush durations for typical
  production configs and leaves headroom. Deployments that want to override
  can still use `CliRunnerConfig::with_graceful_shutdown_timeout`.
- Other graceful tasks in reth (RPC servers, network discovery, etc.) already
  return within sub-second of receiving the signal. A longer default does
  not delay their cleanup; the timeout is a ceiling, not a fixed wait.
- Raising the default is a user-visible behavior change, but "wait up to 60
  seconds to persist data safely" is the right default for a blockchain
  node; nothing else reth does asks for sub-10-second shutdown latency.

### Timing walkthrough

| t | Event |
|---|-------|
| 0 | `SIGTERM` delivered. |
| 0⁺ | tokio `sigterm`/`ctrl_c` future resolves inside `run_until_ctrl_c`'s `select!`; user future is dropped. |
| ε | `run_to_completion_or_panic` returns `Ok(())`; control returns to `run_command_until_exit` in `runner/src/lib.rs:83`. |
| ε⁺ | `task_manager.graceful_shutdown_with_timeout(60s)` runs. It broadcasts `TaskEvent::GracefulShutdown`; every `GracefulShutdown` future held by a graceful task resolves. |
| ε⁺⁺ | Consensus-engine task's `select!` chooses its graceful arm; sends `FromOrchestrator::Terminate { tx: done_tx }` via crossbeam to the engine-tree `Engine Task` OS thread. |
| ~μs | Engine-tree's `run()` loop processes `Terminate` → `finish_termination` → `persist_until_complete`. This drives one or more batch `save_blocks` through the persistence OS thread, which calls `provider_rw.save_blocks + commit` (MDBX) and then applies pending triedb flushes (pathdb RocksDB). Both are synchronous, OS-thread work; tokio schedule is irrelevant here. |
| ≤ 60s | Final flush completes; engine-tree sends `done_tx.send(())`. |
| ~60s⁺ε | Consensus-engine task's `done_rx.await` returns; loop breaks; function returns; `_guard` drops; `graceful_tasks` counter decrements to 0. |
| ~60s⁺2ε | `graceful_shutdown_with_timeout` returns `true`; runner proceeds to runtime drop (with its own 5s-on-another-thread ceiling). |
| ~65s | `main` returns; process exits. |

On the timeout edge — if the flush genuinely takes > 60 seconds:
- `graceful_shutdown_with_timeout` returns `false`; runtime drop proceeds.
- tokio aborts consensus-engine task mid-`done_rx.await`.
- But engine-tree's OS thread **is not bound to tokio**; it keeps running
  `persist_until_complete` until either it finishes or `main` returns and the
  kernel terminates the process.
- If it finishes within the runtime-drop window (another ≤ 5 s), the flush
  persists. If not, the remaining unfliushed blocks behave identically to
  today's regression (best-effort; never worse than today).

### Interaction with existing `EngineShutdown` API

The explicit `engine_shutdown.shutdown()` path is preserved, and the two
paths are mutually exclusive via the `terminating` flag:

- If an RPC or test calls `engine_shutdown.shutdown()` first, it drives the
  existing `shutdown_rx` arm, owns its own `done_rx`, and sets
  `terminating = true`. A subsequent graceful signal is observed but the
  disabled arm means no double-`Terminate` is emitted.
- If the signal-driven graceful arm fires first, it sets `terminating = true`
  before the explicit arm can take effect on a later call (the
  `EngineShutdown` mutex is exclusive, so at most one RPC caller can arrive
  after; that caller's request lands in `shutdown_rx` but the arm is
  disabled; `done_rx` on the RPC side stays open and resolves `Err(_)` when
  the consensus engine task exits and drops `shutdown_rx`. This is the
  correct "already shut down" semantics — the RPC caller sees a closed
  channel).

## Failure modes and recovery

- **Engine-tree fails mid-flush** (MDBX commit error, pathdb apply error).
  `persist_until_complete` returns `Err(AdvancePersistenceError::...)`, which
  propagates out of `finish_termination`. The `pending_termination` oneshot
  is dropped without being sent. On the consensus-engine side, `done_rx.await`
  returns `Err(_)`. We `warn!` and break out of the loop. The process exits
  with whatever state reached disk; MDBX is transactional per-batch so there
  is no partial batch.
- **pathdb apply fails after MDBX commit.** This is the documented
  "MDBX ahead of pathdb" recoverable direction (`crates/engine/tree/src/persistence.rs:276-280`).
  Next startup's alignment step unwinds MDBX down to pathdb (design
  `2026-04-17-triedb-mdbx-startup-alignment-design.md`); net effect is still
  strictly better than today (some blocks persisted; the rest re-fetched via
  P2P backfill).
- **Consensus engine panics inside the flush arm.** `spawn_critical_with_graceful_shutdown_signal`
  routes the panic into `TaskEvent::CriticalTaskPanicked` and
  `graceful_shutdown` returns an error. Process exits with the panic logged;
  no state corruption because every write path is transactional.
- **Graceful timeout expires before flush finishes.** Documented above: flush
  continues on the OS thread for ≤ 5 more seconds (tokio runtime drop
  window). If it still does not finish, the remainder is lost — identical
  to today's behavior for this remainder, so at minimum no regression
  compared to the status quo.
- **Multiple signals in rapid succession** (e.g. `SIGTERM` then `SIGKILL`).
  `SIGKILL` bypasses everything as usual. `SIGTERM` followed by another
  `SIGTERM` is idempotent — the task is already in the graceful arm; the
  second signal is absorbed by `run_until_ctrl_c`'s already-returned
  `select!`, which has no effect at this point.
- **Non-triedb mode.** Persist mechanics still run; pathdb-specific steps in
  `persistence.rs` are gated by `is_triedb_active()` (`:139`, `:215`, `:276`)
  and are no-ops. MDBX side flushes as before. The `graceful` arm still
  runs and is correct.

## Testing

### Existing test extended

`crates/ethereum/node/tests/e2e/eth.rs:170-186` currently calls
`engine_shutdown.shutdown()` explicitly. Add a parallel test that **does not**
call `shutdown()` and instead relies on the graceful arm:

```rust
// Simulate SIGTERM path: trigger graceful shutdown via task manager directly.
node.task_executor().graceful_shutdown_with_timeout(Duration::from_secs(30));

// Verify the same post-condition: db_block_after == 1.
```

This exercises the new graceful arm without needing to send a real OS signal.

### New regression test — "signal-triggered flush persists canonical tip"

Place in `crates/node/builder/tests/` or a new integration test in
`crates/ethereum/node/tests/e2e/`:

1. Launch an EthereumNode with a dummy engine driver that produces N = 3
   canonical blocks.
2. Assert `last_block_number == 0` (below `persistence_threshold`).
3. Invoke `TaskManager::initiate_graceful_shutdown()` to simulate the signal
   path without needing real signal delivery inside a unit test.
4. Await node exit.
5. Assert `last_block_number == 3`.

### BSC qanet regression (operator-side)

After the patch is applied and reth-bsc bumps its pinned rev:

1. Stand up a two-validator qanet from a shared tip H₀.
2. Let one validator mine to H₀ + 200.
3. Send `SIGTERM` (e.g. `kill <pid>`, not `kill -9`).
4. Restart; verify the startup-alignment log prints `gap=0 outcome="noop"`
   **and** the new `best_block_number` equals H₀ + 200 (not H₀ + ~50).
5. Repeat with `SIGKILL` to confirm the unclean-kill regression still exists
   (this is a non-goal and must be handled separately).

### Timeout behavior test

Confirm (manually, no automated test): with
`CliRunnerConfig::with_graceful_shutdown_timeout(Duration::from_millis(1))`
the flush is interrupted and the warning is logged, and that the default
60 s path is not hit in fast paths (< 1 s flushes should not pause).

## Observability

Add these log lines (target `reth::cli` unless stated):

| Event | Level | Message (structured fields) |
|-------|-------|---------|
| Consensus-engine graceful arm entered | `info` | `Graceful shutdown: starting engine flush` (`canonical_tip`, `persisted_before`) |
| `done_rx` resolved `Ok` | `info` | `Graceful shutdown: engine flush complete` (`duration_ms`, `persisted_before`, `persisted_after`) |
| `done_rx` resolved `Err` | `warn` | `Graceful shutdown: done-channel closed before completion` (`duration_ms`) |
| Engine-tree persist loop (already exists) | `debug` (`engine::tree`) | `persistence complete, signaling termination` |

The two `info` lines form a matched bracket: every successful graceful
shutdown emits exactly one "starting" log and one "complete" log. Operators
running at default `RUST_LOG=info` can compute the number of blocks
persisted as `persisted_after - persisted_before` and the wall-clock cost
as `duration_ms`. If `duration_ms` approaches `graceful_shutdown_timeout`
across many shutdowns, that is the signal to either lower
`--engine.persistence-threshold` / `--engine.memory-block-buffer-target`
or raise the CLI timeout.

Operators monitor the `gap` field on the next startup's `Startup alignment:
...` line (already emitted by the 2026-04-17 alignment design). If a graceful
shutdown was used and `gap > 0`, the flush was interrupted — that is the
signal-to-investigate for this design.

No new metrics. `reth_startup_alignment_last_gap` (gauge, existing) already
captures the regression amount across all paths.

## Out of scope

- **Pathdb diff-layer journal / reverse-diff freezer.** Would also cover
  `SIGKILL` / panic paths. Significantly larger; belongs in a separate design.
- **Altering `PersistTarget::Head` semantics** or exposing it publicly. The
  graceful arm uses the existing internal API via `Terminate`; nothing new is
  exported.
- **Configuring per-task graceful shutdown timeouts.** A single global
  timeout is sufficient; finer control is not needed until a second
  long-running graceful task exists.
- **Cross-consumer CLI flag for the graceful timeout.** Adding
  `--graceful-shutdown-timeout` at the top-level CLI is an independent
  ergonomic improvement. Not required for this design to work.
- **Shutting down producers (miner, RPC) before the engine-tree flush.** In
  `reth-bsc` the miner keeps running briefly, can push more blocks into
  engine-tree during the flush window, and `persist_until_complete`'s
  loop-until-empty structure naturally picks them up. Explicit
  "miner-stop-first" ordering is an orthogonal reth-bsc-side concern.

## Follow-ups (separate PRs)

- Diff-layer journal on graceful shutdown (covers `SIGKILL` for the
  in-memory stack above the persistence-worker batch boundary).
- CLI flag `--engine.graceful-shutdown-timeout` to make the new 60 s default
  tunable without code changes.
- Metric: `reth_graceful_shutdown_flush_duration_seconds` (histogram) to
  track the new flush cost across deployments.
- Extending the observability table with an INFO-level "shutdown started /
  completed" bracket pair so operators have one grep-able event per
  lifecycle (today the existing log lines are scattered across targets).

## Acceptance

The design is accepted when:

1. The `consensus engine` task in `launch/engine.rs` is a
   `spawn_critical_with_graceful_shutdown_signal` task, its `select!` has a
   graceful arm that self-triggers `FromOrchestrator::Terminate` and awaits
   the engine-tree `done` oneshot, and the existing explicit `shutdown_rx`
   arm remains as a programmatic entry point.
2. `DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT` is 60 s.
3. Two e2e tests in `crates/ethereum/node/tests/e2e/eth.rs` cover the two
   paths into `finish_termination`: `test_engine_graceful_shutdown` (pre-
   existing, exercises the explicit `EngineShutdown::shutdown()` RPC path)
   and `test_engine_graceful_shutdown_via_signal` (new, exercises the
   `TaskManager::graceful_shutdown_with_timeout` signal path added here).
4. A BSC qanet `SIGTERM` restart with
   `--engine.persistence-threshold 256 --engine.memory-block-buffer-target 128`
   leaves `best_block_number` equal to pre-kill canonical head (verified by
   the operator after the patch ships).
