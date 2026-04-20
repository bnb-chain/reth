# Engine Graceful-Shutdown Flush Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the existing but dormant `EngineShutdown` flush machinery to the graceful-shutdown signal path so that `SIGTERM`/`Ctrl-C` persists every in-memory canonical block before exit. No regression of `best_block_number` on the next restart.

**Architecture:** Two-file change. (1) Convert the `consensus engine` task from `spawn_critical` to `spawn_critical_with_graceful_shutdown_signal` and add a `tokio::select!` arm that self-sends `FromOrchestrator::Terminate` when the graceful signal fires, then awaits engine-tree's `done_rx`. (2) Raise `DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT` from 5 s to 60 s so the new flush path has room to complete.

**Tech Stack:** Rust, tokio, reth-tasks (`GracefulShutdown` / `GracefulShutdownGuard`), reth-engine-tree (`FromOrchestrator::Terminate` + `finish_termination` + `persist_until_complete`), reth-engine-service, reth-cli-runner.

**Related spec:** `docs/superpowers/specs/2026-04-20-engine-graceful-shutdown-flush-design.md`

---

## File Structure

Two files are modified. No new files.

| Path | Role | Change |
|---|---|---|
| `crates/cli/runner/src/lib.rs` | Runner default timeout | Change one `const`: 5 s → 60 s |
| `crates/node/builder/src/launch/engine.rs` | Consensus engine task spawn + body | Switch to `spawn_critical_with_graceful_shutdown_signal`; add graceful arm; add `terminating` guard to both graceful and existing `shutdown_rx` arms |
| `crates/ethereum/node/tests/e2e/eth.rs` | Integration test | Add new test `test_engine_graceful_shutdown_via_signal` beside the existing `test_engine_graceful_shutdown` |

No new public API surface. `EngineShutdown` remains exactly as-is for RPC / test callers.

---

## Ordering

The tasks are ordered so each commit is **individually correct and reviewable**:

1. **Task 1** (timeout bump) is independent. If this were reverted, the rest still works — you'd just see timeouts if flush takes > 5 s. Commit first so the "correctness headroom" is in place before the behavioral change lands.
2. **Task 2** adds a failing test for the signal path. Commit together with Task 3.
3. **Task 3** implements the graceful arm; the Task 2 test now passes. Commit Task 2+3 together (the test and its fix are one logical change).
4. **Task 4** is a clippy/fmt sanity pass before wrapping up.

---

## Preconditions (verify before starting)

- [ ] **Repo is on the intended branch in a clean worktree.** Run `git status` — it should show a clean tree. If you need to work in an isolated worktree, see `superpowers:using-git-worktrees`.
- [ ] **Baseline builds.** Run `cargo check -p reth-node-builder -p reth-cli-runner` — should succeed. Baseline for comparing later builds.
- [ ] **Baseline test passes.** Run `cargo test -p reth-ethereum-node --test=it test_engine_graceful_shutdown -- --nocapture`. This is the existing e2e test that exercises the explicit `engine_shutdown.shutdown()` path (not the graceful signal path). Expected: 1 test passes.

If any precondition fails, stop and fix before writing the implementation. Do not modify code to "skip" a failing baseline.

---

### Task 1: Raise `DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT` from 5 s to 60 s

**Files:**
- Modify: `crates/cli/runner/src/lib.rs:221`

**Rationale (read before editing):** Graceful shutdown now has to wait for engine-tree's `persist_until_complete` which flushes up to `persistence_threshold + 1` canonical blocks of MDBX `save_blocks` commits plus pathdb apply. With BSC validator config (`--engine.persistence-threshold 256 --engine.memory-block-buffer-target 128`) this can be 128–257 blocks. On debug builds / slow disks, 5 s is not enough. 60 s is safely above realistic worst cases and is still overrideable via `CliRunnerConfig::with_graceful_shutdown_timeout()`.

- [ ] **Step 1.1: Inspect the current constant**

Run:
```bash
sed -n '218,223p' crates/cli/runner/src/lib.rs
```

Expected output (must see exactly this — if different, re-read the file and abort):
```rust
/// Default timeout for graceful shutdown of tasks.
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
```

- [ ] **Step 1.2: Edit the constant**

In `crates/cli/runner/src/lib.rs`, replace:
```rust
/// Default timeout for graceful shutdown of tasks.
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
```
with:
```rust
/// Default timeout for graceful shutdown of tasks.
///
/// This bounds how long the runner waits for graceful tasks (including the
/// consensus engine's final `persist_until_complete` flush) to finish after
/// a `SIGTERM` / `Ctrl-C` is received. It must be large enough to let the
/// engine-tree flush every in-memory canonical block up to the head; with
/// `--engine.persistence-threshold` / `--engine.memory-block-buffer-target`
/// set to production-validator values (hundreds of blocks in-memory at any
/// time), 5 s is not enough on debug builds or slow disks. 60 s is a
/// conservative default; operators can still override via
/// `CliRunnerConfig::with_graceful_shutdown_timeout`.
const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
```

- [ ] **Step 1.3: Verify it compiles**

Run:
```bash
cargo check -p reth-cli-runner
```
Expected: `Finished`, no errors, no new warnings.

- [ ] **Step 1.4: Commit**

```bash
git add crates/cli/runner/src/lib.rs
git commit -m "cli/runner: raise default graceful shutdown timeout to 60s

The consensus engine task will soon perform a final persist_until_complete
flush on graceful shutdown. With production validator settings, that flush
can take tens of seconds. 5 s is too tight and would force the process to
exit before the flush finished. 60 s is safely above realistic worst cases
and remains overrideable via CliRunnerConfig."
```

---

### Task 2: Add failing e2e test for graceful-signal path

**Files:**
- Modify: `crates/ethereum/node/tests/e2e/eth.rs` (add new test beside `test_engine_graceful_shutdown` at line 137)

**Why this test:** The existing `test_engine_graceful_shutdown` at `eth.rs:137-187` exercises the **explicit** RPC path (`engine_shutdown.shutdown()`). We need a second test that exercises the **signal path** (what happens on `SIGTERM`). The two paths converge on `finish_termination`, but they come at it differently — the signal path relies on the new graceful arm we add in Task 3. If we only test the explicit path, we have no automated regression coverage of the signal wiring.

**How this test simulates `SIGTERM`:** In production, `run_until_ctrl_c` catches the signal, its `tokio::select!` returns `Ok(())`, and the runner then calls `task_manager.graceful_shutdown_with_timeout(timeout)`. That last call is what fires the `Shutdown` signal all graceful tasks are awaiting. We emulate the equivalent by calling `graceful_shutdown_with_timeout` directly on the `TaskManager` returned from `setup(...)`. Because that call is synchronous and spins (see `tasks/src/lib.rs:242-255`), we run it on a dedicated OS thread so the tokio test runtime stays live.

- [ ] **Step 2.1: Confirm baseline — the existing test passes**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful_shutdown -- --nocapture
```

Expected: `test_engine_graceful_shutdown ... ok` (1 test passes; the test name-prefix-matches the existing test only).

- [ ] **Step 2.2: Add the new test**

In `crates/ethereum/node/tests/e2e/eth.rs`, directly after the closing `}` of `test_engine_graceful_shutdown` (which ends around line 187) and before the next `#[tokio::test]` (around line 189), insert:

```rust
#[tokio::test]
async fn test_engine_graceful_shutdown_via_signal() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, tasks, wallet) = setup::<EthereumNode>(
        1,
        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
                .cancun_activated()
                .build(),
        ),
        false,
        eth_payload_attributes,
    )
    .await?;

    let mut node = nodes.pop().unwrap();

    let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let tx_hash = node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    node.assert_new_block(tx_hash, payload.block().hash(), payload.block().number).await?;

    // Precondition: block is in-memory but not yet persisted.
    assert_eq!(
        node.inner.provider.best_block_number()?,
        1,
        "expected 1 block before shutdown"
    );
    assert_eq!(
        node.inner.provider.last_block_number()?,
        0,
        "block should not be persisted yet"
    );

    // Simulate the production SIGTERM path: TaskManager::graceful_shutdown_with_timeout.
    // This is what `run_until_ctrl_c` causes the runner to invoke after catching
    // SIGTERM/Ctrl-C (see `cli/runner/src/lib.rs:83`). It fires the Shutdown signal
    // that every `GracefulShutdown` future in the node is awaiting, which in turn
    // should drive the consensus engine's graceful arm added by this change.
    //
    // `graceful_shutdown_with_timeout` consumes `self` and spins synchronously, so
    // we move it to a dedicated OS thread and keep the tokio test runtime alive to
    // drive the actual flush work.
    let shutdown_thread = std::thread::Builder::new()
        .name("test-graceful-shutdown".into())
        .spawn(move || {
            tasks.graceful_shutdown_with_timeout(std::time::Duration::from_secs(30))
        })
        .expect("failed to spawn shutdown thread");

    // Poll the database for up to 30 s until the pre-kill head lands on disk.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut db_block = 0u64;
    while std::time::Instant::now() < deadline {
        db_block = node.inner.provider.last_block_number()?;
        if db_block == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let shutdown_completed = shutdown_thread
        .join()
        .expect("shutdown thread panicked");

    assert_eq!(
        db_block, 1,
        "database should have persisted block 1 via graceful signal path (shutdown_completed={shutdown_completed})",
    );

    Ok(())
}
```

- [ ] **Step 2.3: Verify it compiles (it should)**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful_shutdown_via_signal --no-run
```

Expected: compiles cleanly. If it does not compile, read the error and fix the test; do not proceed to Task 3 yet.

- [ ] **Step 2.4: Run the test and confirm it FAILS**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful_shutdown_via_signal -- --nocapture
```

Expected: test **FAILS** with an assertion about `db_block == 1`. The reason is that today the `consensus engine` task is a plain `spawn_critical` (non-graceful); when `graceful_shutdown_with_timeout` runs, the task is dropped before it can process `Terminate`, so the flush never happens and `last_block_number()` stays at 0.

Document the exact failure message verbatim in your commit summary or the review notes — it is evidence the test is actually exercising the intended behavior.

- [ ] **Step 2.5: Do not commit yet**

This test is paired with Task 3's implementation; commit them together in Task 3.

---

### Task 3: Implement graceful arm in consensus engine task

**Files:**
- Modify: `crates/node/builder/src/launch/engine.rs:326-418` (consensus engine spawn site + its closure body)

**Rationale:** This is the core change. We need the `consensus engine` task to:

1. Register itself as a **graceful task** (so `graceful_shutdown_with_timeout` waits for it instead of dropping it on sight).
2. Observe the `GracefulShutdown` future as a new arm of its existing `tokio::select!`.
3. When that arm fires, synthesise a local `oneshot::channel()` for the termination completion signal, send `FromOrchestrator::Terminate { tx: done_tx }` into engine-tree (same entry point the explicit RPC path uses), await `done_rx`, then `break` so the task function returns and the `GracefulShutdownGuard` drops.
4. Preserve the existing explicit `shutdown_rx` arm as-is (so the RPC / test path keeps working), but gate both arms with `terminating` so a late-arriving signal after a manual trigger is a no-op.

**Key correctness points (read before editing):**

- `GracefulShutdown: Future<Output = GracefulShutdownGuard>` (see `crates/tasks/src/shutdown.rs:35-42`) — polling after it resolves panics via `.expect("Future polled after completion")`. Our design polls `&mut graceful` repeatedly in the loop but `break`s on its first ready, so re-poll never happens. Do **not** remove the `break`.
- The `_guard` binding from the arm pattern is the `GracefulShutdownGuard`. It drops when the arm's scope ends (i.e. at `break`). This is the signal to `TaskManager` that this task has finished graceful shutdown. Do **not** rename it to drop early, and do **not** `drop(_guard)` manually before the `done_rx.await`.
- The `terminating` flag prevents double-`Terminate` if both a programmatic `EngineShutdown::shutdown()` and a graceful signal arrive. The explicit-arm side-effect (`on_event(Terminate)`) already hands ownership of `req.done_tx` away, so double-send would be a use-after-move bug. Keep the `if !terminating` guards on **both** arms.
- `engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(...)` is synchronous (`fn on_event(&mut self, event: FromOrchestrator);` in `crates/engine/tree/src/chain.rs:204`). Do **not** add `.await`.
- The `.into()` on `FromOrchestrator::Terminate { ... }.into()` is required by the last `handler_mut().on_event` signature — keep it exactly as the existing arm uses it.

- [ ] **Step 3.1: Read the current code you are about to replace**

Run:
```bash
sed -n '326,418p' crates/node/builder/src/launch/engine.rs
```

Confirm you see:
- Line 326: `info!(target: "reth::cli", "Starting consensus engine");`
- Line 327: `let consensus_engine = async move {`
- Line 337: `let mut shutdown_rx = shutdown_rx.fuse();`
- Line 343-350: the existing `shutdown_req = &mut shutdown_rx => { ... on_event(FromOrchestrator::Terminate { tx: req.done_tx }.into()); ... }` arm
- Line 416: `let _ = exit.send(res);`
- Line 417: closing `};`
- Line 418: `ctx.task_executor().spawn_critical("consensus engine", Box::pin(consensus_engine));`

If the structure differs, **stop** and re-check which revision you are on; do not proceed.

- [ ] **Step 3.2: Replace the `let consensus_engine = async move { ... }; spawn_critical(...);` block**

Replace the entire region from line 327 (`let consensus_engine = async move {`) through line 418 (`ctx.task_executor().spawn_critical("consensus engine", Box::pin(consensus_engine));`) with the following. Keep line 326 (`info!(target: "reth::cli", "Starting consensus engine");`) and all lines before/after the replaced region unchanged.

```rust
        ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
            "consensus engine",
            move |graceful| async move {
                if let Some(initial_target) = initial_target {
                    debug!(target: "reth::cli", %initial_target,  "start backfill sync");
                    // network_handle's sync state is already initialized at Syncing
                    engine_service.orchestrator_mut().start_backfill_sync(initial_target);
                } else if startup_sync_state_idle {
                    network_handle.update_sync_state(SyncState::Idle);
                }

                let mut res = Ok(());
                let mut shutdown_rx = shutdown_rx.fuse();
                let mut graceful = std::pin::pin!(graceful);
                let mut terminating = false;

                // advance the chain and await payloads built locally to add into the engine api
                // tree handler to prevent re-execution if that block is received as payload from
                // the CL
                loop {
                    tokio::select! {
                        // New arm: TaskManager graceful-shutdown signal (SIGTERM/Ctrl-C path).
                        //
                        // This arm self-sends FromOrchestrator::Terminate and awaits the
                        // engine-tree's finish_termination() completion signal so every
                        // in-memory canonical block above `last_persisted_block` is flushed
                        // before the task exits. `_guard` holds the GracefulShutdownGuard
                        // across the await; it drops on `break`, decrementing TaskManager's
                        // graceful_tasks counter so graceful_shutdown_with_timeout can return.
                        //
                        // `if !terminating` prevents re-poll after a prior explicit
                        // EngineShutdown::shutdown() already handed ownership of its own
                        // done_tx to engine-tree.
                        _guard = &mut graceful, if !terminating => {
                            debug!(target: "reth::cli", "received graceful shutdown signal; triggering engine terminate");
                            terminating = true;
                            let (done_tx, done_rx) = oneshot::channel();
                            engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(
                                FromOrchestrator::Terminate { tx: done_tx }.into()
                            );
                            match done_rx.await {
                                Ok(()) => debug!(target: "reth::cli", "engine flush complete; exiting consensus loop"),
                                Err(err) => warn!(target: "reth::cli", %err, "engine shutdown done-channel closed before completion"),
                            }
                            break;
                        }
                        shutdown_req = &mut shutdown_rx, if !terminating => {
                            if let Ok(req) = shutdown_req {
                                debug!(target: "reth::cli", "received engine shutdown request");
                                terminating = true;
                                engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(
                                    FromOrchestrator::Terminate { tx: req.done_tx }.into()
                                );
                            }
                        }
                        payload = built_payloads.select_next_some() => {
                            if let Some(executed_block) = payload.executed_block() {
                                debug!(target: "reth::cli", block=?executed_block.recovered_block.num_hash(),  "inserting built payload");
                                engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(EngineApiRequest::InsertExecutedBlock(executed_block.into_executed_payload()).into());
                            }
                        }
                        req = engine_api_rx.recv() => {
                            if let Some(req) = req {
                                engine_service.orchestrator_mut().handler_mut().handler_mut().on_event(req.into());
                            }
                        }
                        event = engine_service.next() => {
                            let Some(event) = event else { break };
                            debug!(target: "reth::cli", "Event: {event}");
                            match event {
                                ChainEvent::BackfillSyncFinished => {
                                    if terminate_after_backfill {
                                        debug!(target: "reth::cli", "Terminating after initial backfill");
                                        break
                                    }
                                    if startup_sync_state_idle {
                                        network_handle.update_sync_state(SyncState::Idle);
                                    }
                                }
                                ChainEvent::BackfillSyncStarted => {
                                    network_handle.update_sync_state(SyncState::Syncing);
                                }
                                ChainEvent::FatalError => {
                                    error!(target: "reth::cli", "Fatal error in consensus engine");
                                    res = Err(eyre::eyre!("Fatal error in consensus engine"));
                                    break
                                }
                                ChainEvent::Handler(ev) => {
                                    if let Some(head) = ev.canonical_header() {
                                        // Once we're progressing via live sync, we can consider the node is not syncing anymore
                                        network_handle.update_sync_state(SyncState::Idle);
                                        let head_block = Head {
                                            number: head.number(),
                                            hash: head.hash(),
                                            difficulty: head.difficulty(),
                                            timestamp: head.timestamp(),
                                            total_difficulty: chainspec.final_paris_total_difficulty()
                                                .filter(|_| chainspec.is_paris_active_at_block(head.number()))
                                                .or_else(|| {
                                                    provider.header_td_by_number(head.number()).ok().flatten()
                                                })
                                                .unwrap_or_default(),
                                        };
                                        network_handle.update_status(head_block);

                                        let updated = BlockRangeUpdate {
                                            earliest: provider.earliest_block_number().unwrap_or_default(),
                                            latest: head.number(),
                                            latest_hash: head.hash(),
                                        };
                                        network_handle.update_block_range(updated);
                                    }
                                    event_sender.notify(ev);
                                }
                            }
                        }
                    }
                }

                let _ = exit.send(res);
                // `_guard` (the GracefulShutdownGuard captured by the graceful arm, if that
                // arm fired) drops here or earlier at its `break`; either way TaskManager's
                // graceful_tasks counter decrements and graceful_shutdown_with_timeout can
                // return.
            },
        );
```

- [ ] **Step 3.3: Verify the indentation and visual diff**

Run:
```bash
git diff crates/node/builder/src/launch/engine.rs | head -180
```

Inspect visually:
- The closing `};` at the old line 417 becomes `            },\n        );` (closing the `async move` block, then the closure, then the function call).
- No stray `let consensus_engine = async move {` or `Box::pin(consensus_engine)` text remains.
- The `info!(target: "reth::cli", "Starting consensus engine");` line (old 326) is unchanged and sits immediately above the new `spawn_critical_with_graceful_shutdown_signal` call.

If any of these look off, fix before compiling.

- [ ] **Step 3.4: Check compilation**

Run:
```bash
cargo check -p reth-node-builder
```

Expected: `Finished`, no errors. If you see errors:
- **`cannot find type GracefulShutdown in this scope`** — unlikely (closure type is inferred), but if it surfaces, add `use reth_tasks::GracefulShutdown;` to the imports block at the top of `engine.rs`.
- **`future cannot be sent between threads safely`** — check that all captures (`initial_target`, `engine_service`, `built_payloads`, etc.) are `Send`. These already were `Send` under the previous `spawn_critical` + `Box::pin` path, so this should not regress; if it does, re-read the error and fix the offending capture.
- **Pattern / type errors on `_guard = &mut graceful`** — re-read the `tokio::select!` documentation; the pattern must be a valid irrefutable pattern. `_guard` is fine; rename to `_g` if `_guard` is shadowed anywhere (it is not in the current body).

Do not proceed until `cargo check` is clean.

- [ ] **Step 3.5: Run the existing test to confirm no regression**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful_shutdown -- --nocapture
```

Expected: the existing `test_engine_graceful_shutdown` still passes. This exercises the **explicit** path (`engine_shutdown.shutdown()` → `shutdown_rx` arm) which we left intact apart from setting `terminating = true`.

If it fails, the most likely causes are:
- The `terminating = true` assignment landed in the wrong arm or was misordered relative to the `on_event` call. Confirm both arms set `terminating = true` **before** they `on_event`-send, so a simultaneously-arriving graceful signal sees the flag set.
- Accidentally removed the `debug!` / `on_event` logic from the `shutdown_req` arm. Compare carefully with Step 3.1's original.

Fix and re-run until it passes. Do not proceed with a red bar.

- [ ] **Step 3.6: Run the new test and confirm it PASSES**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful_shutdown_via_signal -- --nocapture
```

Expected: `test_engine_graceful_shutdown_via_signal ... ok`. The test now passes because the graceful arm added in this task drives the same `finish_termination` flow that the explicit test relies on.

If it times out (30 s polling loop exhausted):
- Inspect the test log — did the `"received graceful shutdown signal; triggering engine terminate"` debug line appear? If not, the graceful arm is not firing; re-check the `spawn_critical_with_graceful_shutdown_signal` call-site (wrong spawn function, wrong closure signature).
- Did `"engine flush complete; exiting consensus loop"` appear? If the first line appeared but this one did not, `done_rx.await` timed out — inspect engine-tree's `finish_termination` log target `engine::tree` for errors.

- [ ] **Step 3.7: Run both tests together to check for ordering flakiness**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful -- --nocapture
```

Expected: both tests pass (2 tests matched by prefix). Running together surfaces any accidental shared global state (e.g. static `OnceLock` in BSC `shared` module) — none is expected in this path, but running both catches it.

- [ ] **Step 3.8: Commit**

```bash
git add crates/node/builder/src/launch/engine.rs crates/ethereum/node/tests/e2e/eth.rs
git commit -m "engine: flush in-memory canonical blocks on graceful shutdown signal

Convert the consensus engine task from spawn_critical to
spawn_critical_with_graceful_shutdown_signal and add a tokio::select!
arm that self-sends FromOrchestrator::Terminate when the
GracefulShutdown future fires, then awaits engine-tree's done_rx so the
full persist_until_complete loop runs before the task exits.

The existing engine_shutdown.shutdown() RPC path is preserved via a
terminating flag on both arms, so it remains usable for programmatic
callers (and for the existing e2e test).

Adds test_engine_graceful_shutdown_via_signal which simulates SIGTERM
by driving TaskManager::graceful_shutdown_with_timeout on a dedicated
OS thread and asserts the pre-kill canonical tip is persisted.

Fixes engine-tree in-memory canonical-block loss on every SIGTERM /
Ctrl-C for validators running with non-default persistence-threshold
and memory-block-buffer-target.

See docs/superpowers/specs/2026-04-20-engine-graceful-shutdown-flush-design.md"
```

---

### Task 4: Final review — clippy, fmt, and full workspace build

**Files:** none (verification only)

- [ ] **Step 4.1: Run clippy on the touched crates**

Run:
```bash
cargo clippy -p reth-cli-runner -p reth-node-builder -p reth-ethereum-node --all-targets -- -D warnings
```

Expected: no new warnings (existing warnings in unrelated code are fine; but the changed files must be warning-clean with `-D warnings`).

If there are warnings in the new code:
- `unused_variables` on `_guard` — it is already `_`-prefixed; confirm the prefix.
- `unused_mut` on `terminating` / `graceful` — both are mutated/pinned in scope; should not trigger.
- Other lints: fix inline; do not suppress.

- [ ] **Step 4.2: Run fmt**

Run:
```bash
cargo fmt -p reth-cli-runner -p reth-node-builder -p reth-ethereum-node --check
```

If it reports diffs, re-run without `--check` to apply and amend the last commit (`git commit --amend`). Do not skip formatting.

- [ ] **Step 4.3: Re-run the two engine graceful-shutdown tests**

Run:
```bash
cargo test -p reth-ethereum-node --test=it test_engine_graceful -- --nocapture
```

Expected: both pass. This is a final safety net after any fmt / clippy tweaks.

- [ ] **Step 4.4: Build the whole workspace**

Run:
```bash
cargo check --workspace
```

Expected: `Finished`, no errors. This catches any downstream crate that picks up the timeout change or the spawn form in ways the two tests missed.

---

## Post-implementation notes for reviewers

- The **timeout change** (Task 1) is observable to every reth operator, not just BSC validators. The design doc's "Non-goals" section and this plan's rationale for Task 1 frame it as "raise because graceful shutdown now has real work to do". If upstream review objects to the default, the fallback is to keep `DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT` at 5 s and document the need to override it — that is a pure revert of Task 1, independent of Task 3.
- The **graceful arm** (Task 3) is a strict superset of the existing `shutdown_rx` arm: explicit `EngineShutdown::shutdown()` callers keep working. No existing test needs changes beyond what is covered here.
- **Non-triedb mode** (`is_triedb_active() == false`) still hits `finish_termination` / `persist_until_complete`. Persistence-side pathdb steps are gated by `is_triedb_active()` (`crates/engine/tree/src/persistence.rs:139`, `:215`, `:276`) and become no-ops. The mdbx batch flush still runs. The graceful arm behaves identically in both modes.
- **BSC qanet regression** (per the spec's Acceptance §4) is an operator-side verification step, not part of this plan. After this plan ships on the fork and reth-bsc bumps its pinned rev, run the scenario in spec §Testing.

## Self-review checklist (for the implementer, not a subagent)

- [ ] Spec sections that produce a task here:
  - Design Part 1 (consensus engine graceful arm) → Task 3
  - Design Part 2 (default timeout) → Task 1
  - Testing → Task 2 (new test), Task 3.5 (existing test passes)
  - Observability log lines → Task 3.2 (baked into the arm body)
  - Failure modes → covered by `done_rx.await` `Err` branch in Task 3.2
- [ ] No `TODO` / `TBD` / "add error handling" / "similar to Task N" in any step.
- [ ] Types match throughout: `GracefulShutdown` future, `GracefulShutdownGuard`, `oneshot::Sender<()>` on `FromOrchestrator::Terminate`, `bool` return of `graceful_shutdown_with_timeout`.
- [ ] Every edited file path is absolute-from-repo-root and line numbers match the current HEAD at the time of writing (`29d5f21`). If the branch has moved, re-derive line numbers before editing.
