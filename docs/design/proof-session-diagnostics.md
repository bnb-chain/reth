# Proof session queue diagnostics

An account/storage proof session occupies its named scheduling thread until its worker
broadcast finishes. Workers process many proof jobs in parallel within that session.
A later session on the same Runtime/name can queue behind it, even while the earlier
session waits for more jobs or for its senders to close. Import, miner and miner-empty
have independent Runtime worker maps.

This instrumentation preserves scheduling, cancellation and ownership. It retains only
small identity/timing records, never a channel, Runtime or provider. No per-proof logs.

| Metric | Meaning |
| --- | --- |
| `reth_executor_named_session_queue_wait_seconds{runtime,worker,origin}` | Submission to actual named closure start; not time since root creation |
| `reth_executor_named_session_run_seconds{runtime,worker,origin}` | Named closure lifetime including broadcast/teardown; not CPU time |
| `reth_trie_diagnostics_proof_worker_dispatch_seconds{origin,kind}` | Named session start to each worker's run entry, including broadcast scheduling |
| `reth_trie_diagnostics_phase_seconds{origin,phase,outcome}` | `storage_init`/`account_init`: per-worker provider/cursor initialization; `final_root`: root_with_updates. Completed or interrupted, including early errors/unwind |
| `reth_trie_diagnostics_finish_marker_seconds{origin,phase}` | `hash_input`: finish sent to hashing task consuming it; `root_input`: hashing task consuming it to root task consuming it |
| `reth_trie_diagnostics_proofs_pending_at_finish{origin}` | Sent-minus-received proof batches when root consumes finish marker, not unique keys or disk reads |
| `reth_trie_diagnostics_finish_drain_seconds{origin}` | Root consumed finish marker to proof/trie updates fully drained; completed drain only |
| `reth_trie_diagnostics_post_finish_seconds{origin,result}` | Finish sent to root computation returning success/cancelled/failed, before result publication |

These clocks have different sample populations. Do not sum their independent means as
an exact per-task decomposition. A missing finish marker produces no post-finish sample.
Existing `reth_executor_named_worker_queued_tasks` and `active_tasks` remain useful for
sessions still queued: queue-wait histograms receive a sample only when a session starts.

WARN target `reth::tasks::session` emits `Sampled named session queued/started/exited`.
A busy enqueue can select one sample per worker/Runtime per 5 seconds; a slow start
(>=20ms) can also select a sample. A selected queued session and its current running
owner emit related exit/start records without further throttling. This is sampled,
not an exhaustive task audit; later queued tasks can also be ahead.

Fields: root_task_id, origin, block_number, runtime, worker; queued events also include
blocking_root_task_id/origin/block_number, blocking_age_ms and pending_tasks. These
identify the owner at observation time, not necessarily the only source of wait.
Started events include actual queue_ms and previous_root_task_id/block_number (the last
completed traced session). IDs/block numbers are log-only, never metric labels.
Metrics remain exhaustive for observed sessions, even when their logs are suppressed.
Unstarted/dropped submissions do not get a fake queue/run sample; unwind clears ownership.

Example queue average in milliseconds:

```promql
1000 * sum by (runtime,worker,origin) (rate(reth_executor_named_session_queue_wait_seconds_sum[2m]))
/ sum by (runtime,worker,origin) (rate(reth_executor_named_session_queue_wait_seconds_count[2m]))
```
