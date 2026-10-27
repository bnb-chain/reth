//! Bounded, advisory proof work which never enters the authoritative root task's queues.

use crate::{error::StateRootTaskError, state_root_task::StateAccessHint};
use alloy_primitives::{
    map::{HashMap, HashSet},
    B256,
};
use alloy_rlp::Encodable;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use reth_provider::DatabaseProviderROFactory;
use reth_trie::{
    hashed_cursor::HashedCursorFactory, proof::Proof, trie_cursor::TrieCursorFactory,
    DecodedMultiProofV2, MultiProofTargetsV2,
};
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Instant,
};

const QUEUE_CAPACITY: usize = 8;
const MAX_KEYS: usize = 128;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PROOF_NODES: usize = 128;
const MAX_LOOKUP_NODES: usize = 256;
static QUEUE: OnceLock<Option<Sender<Job>>> = OnceLock::new();

/// An attempt-scoped producer for independent proof prefetching.
///
/// A process-wide single worker has a bounded queue. Neither admission nor cache consumption
/// waits for that worker or for a contended cache lock. Only completed proofs can be reused by
/// formal proof requests, and speculative targets never become sparse-trie updates.
pub struct IsolatedProofPrefetch {
    parent: B256,
    closed: AtomicBool,
    cancel: Receiver<()>,
    queue: Sender<Job>,
    compute: Box<dyn Fn(Key) -> Result<DecodedMultiProofV2, StateRootTaskError> + Send + Sync>,
    cache: Arc<ReadyProofCache>,
}

impl fmt::Debug for IsolatedProofPrefetch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IsolatedProofPrefetch")
            .field("parent", &self.parent)
            .finish_non_exhaustive()
    }
}

impl IsolatedProofPrefetch {
    /// Creates an attempt using an already initialized worker and the same parent provider
    /// factory as its formal proof tasks. Returns `None` when startup initialization failed
    /// or was not requested; it never creates a thread on the mining path.
    pub fn new<F>(parent: B256, factory: F, cancel: Receiver<()>) -> Option<Arc<Self>>
    where
        F: DatabaseProviderROFactory<Provider: TrieCursorFactory + HashedCursorFactory>
            + Send
            + Sync
            + 'static,
    {
        let queue = QUEUE.get()?.as_ref()?.clone();
        Some(Self::with_compute(parent, cancel, queue, move |key| {
            let provider = factory.database_provider_ro()?;
            Ok(Proof::new(&provider, &provider).multiproof_v2(key.targets())?)
        }))
    }

    fn with_compute(
        parent: B256,
        cancel: Receiver<()>,
        queue: Sender<Job>,
        compute: impl Fn(Key) -> Result<DecodedMultiProofV2, StateRootTaskError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            parent,
            closed: AtomicBool::new(false),
            cancel,
            queue,
            compute: Box::new(compute),
            cache: Arc::default(),
        })
    }

    /// Shares completed results with formal proof workers for this attempt only.
    pub fn cache(&self) -> Arc<ReadyProofCache> {
        self.cache.clone()
    }

    /// Discards new work immediately. Does not join a worker, acquire its locks, clear a large
    /// cache or wait for an in-flight provider call. Already completed proofs remain reusable.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn stopped(&self) -> bool {
        self.closed.load(Ordering::Acquire) ||
            matches!(self.cancel.try_recv(), Err(TryRecvError::Disconnected))
    }

    /// Attempts bounded admission, dropping hints when busy. The caller is a speculative
    /// execution worker, never the authoritative update producer.
    pub fn submit(self: &Arc<Self>, hint: StateAccessHint) {
        for key in hint
            .accounts
            .into_iter()
            .map(Key::Account)
            .chain(hint.storages.into_iter().flat_map(|(account, slots)| {
                slots.into_iter().map(move |slot| Key::Storage(account, slot))
            }))
            .take(MAX_KEYS)
        {
            if self.stopped() {
                break
            }
            let Ok(mut data) = self.cache.data.try_lock() else {
                event("admission_busy");
                break
            };
            if data.requested.len() == MAX_KEYS {
                event("key_limit");
                break
            }
            if !data.requested.insert(key) {
                continue
            }
            if self.queue.try_send(Job { prefetch: self.clone(), key }).is_err() {
                data.requested.remove(&key);
                event("queue_full");
                break
            }
            event("queued");
        }
    }

    fn run(&self, key: Key) {
        if self.stopped() {
            event("cancelled");
            return
        }
        let start = Instant::now();
        let result = (self.compute)(key);
        #[cfg(feature = "metrics")]
        metrics::histogram!("trie.prefetch.compute_seconds").record(start.elapsed().as_secs_f64());
        #[cfg(not(feature = "metrics"))]
        let _ = start;
        if self.stopped() {
            event("cancelled");
            return
        }
        let Ok(proof) = result else {
            event("error");
            return
        };
        let nodes = node_count(&proof);
        if nodes == 0 || nodes > MAX_PROOF_NODES {
            event("proof_limit");
            return
        }
        let bytes = proof
            .account_proofs
            .iter()
            .chain(proof.storage_proofs.values().flatten())
            .map(|node| std::mem::size_of_val(node) + node.node.length() + 256)
            .sum::<usize>() +
            256;
        let Ok(mut data) = self.cache.data.try_lock() else {
            event("publish_busy");
            return
        };
        if self.stopped() {
            return
        }
        if data.bytes.saturating_add(bytes) > MAX_CACHE_BYTES {
            event("byte_limit");
            return
        }
        data.bytes += bytes;
        data.ready.insert(key, Arc::new(CachedProof { proof, nodes }));
        event("ready");
    }
}

/// Starts the optional process-wide prefetch worker during node setup.
///
/// Failure disables advisory work. No fallback sends speculative targets to the formal queue.
pub fn initialize_isolated_proof_prefetch() -> bool {
    QUEUE
        .get_or_init(|| {
            let (tx, rx) = crossbeam_channel::bounded::<Job>(QUEUE_CAPACITY);
            std::thread::Builder::new()
                .name("proof-prefetch".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        // A failed speculative calculation must not kill the shared worker or
                        // formal root.
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            job.prefetch.run(job.key)
                        }))
                        .is_err()
                        {
                            event("panic");
                        }
                    }
                })
                .ok()
                .map(|_| tx)
        })
        .is_some()
}

/// Completed proofs for one immutable parent and attempt, with nonblocking lookup.
///
/// Only full-root targets are served initially. A request with an already-revealed parent
/// keeps the normal calculator path rather than splicing a broader, stale ancestor proof.
#[derive(Debug, Default)]
pub struct ReadyProofCache {
    data: Mutex<CacheData>,
}

impl ReadyProofCache {
    pub(crate) fn take_ready(&self, targets: &mut MultiProofTargetsV2) -> DecodedMultiProofV2 {
        let Ok(data) = self.data.try_lock() else {
            event("lookup_busy");
            return DecodedMultiProofV2::default()
        };
        let mut ready = Vec::new();
        let mut nodes = 0;
        let mut accept = |key, known_parent| {
            if known_parent {
                return false
            }
            let Some(proof) = data.ready.get(&key) else {
                event("miss");
                return false
            };
            if nodes + proof.nodes > MAX_LOOKUP_NODES {
                return false
            }
            nodes += proof.nodes;
            ready.push(proof.clone());
            event("hit");
            true
        };
        targets
            .account_targets
            .retain(|target| !accept(Key::Account(target.key()), target.parent.is_known()));
        for (account, slots) in &mut targets.storage_targets {
            slots.retain(|target| {
                !accept(Key::Storage(*account, target.key()), target.parent.is_known())
            });
        }
        targets.storage_targets.retain(|_, slots| !slots.is_empty());
        drop(data);
        let mut result = DecodedMultiProofV2::default();
        for proof in ready {
            result.extend(proof.proof.clone());
        }
        result
    }
}

#[derive(Debug, Default)]
struct CacheData {
    requested: HashSet<Key>,
    ready: HashMap<Key, Arc<CachedProof>>,
    bytes: usize,
}
#[derive(Debug)]
struct CachedProof {
    proof: DecodedMultiProofV2,
    nodes: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Account(B256),
    Storage(B256, B256),
}
impl Key {
    fn targets(self) -> MultiProofTargetsV2 {
        let mut targets = MultiProofTargetsV2::default();
        match self {
            Self::Account(account) => targets.account_targets.push(account.into()),
            Self::Storage(account, slot) => {
                targets.storage_targets.insert(account, vec![slot.into()]);
            }
        }
        targets
    }
}
struct Job {
    prefetch: Arc<IsolatedProofPrefetch>,
    key: Key,
}
fn node_count(proof: &DecodedMultiProofV2) -> usize {
    proof.account_proofs.len() + proof.storage_proofs.values().map(Vec::len).sum::<usize>()
}
fn event(result: &'static str) {
    #[cfg(feature = "metrics")]
    metrics::counter!("trie.prefetch.events_total", "result" => result).increment(1);
    #[cfg(not(feature = "metrics"))]
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use reth_trie::{test_utils::TrieTestHarness, ProofV2Target};
    use std::{
        collections::BTreeMap,
        sync::{atomic::AtomicUsize, mpsc},
        time::Duration,
    };

    fn hint(key: B256) -> StateAccessHint {
        StateAccessHint { accounts: vec![key], storages: Default::default() }
    }
    fn setup(
        capacity: usize,
        compute: impl Fn(Key) -> Result<DecodedMultiProofV2, StateRootTaskError> + Send + Sync + 'static,
    ) -> (Arc<IsolatedProofPrefetch>, Receiver<Job>, Sender<()>) {
        let (queue, jobs) = crossbeam_channel::bounded(capacity);
        let (cancel, rx) = crossbeam_channel::bounded(0);
        (IsolatedProofPrefetch::with_compute(B256::ZERO, rx, queue, compute), jobs, cancel)
    }

    #[test]
    fn ready_storage_proof_matches_normal_calculation_and_partial_requests_fall_back() {
        let harness = TrieTestHarness::new(
            (1..=32).map(|i| (B256::with_last_byte(i), U256::from(i))).collect::<BTreeMap<_, _>>(),
        );
        let trie = harness.trie_cursor_factory();
        let hashed = harness.hashed_cursor_factory();
        let slot = B256::with_last_byte(9);
        let expected = Proof::new(trie.clone(), hashed.clone())
            .multiproof_v2(Key::Storage(B256::ZERO, slot).targets())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let (prefetch, jobs, _cancel) = setup(1, move |key| {
            invoked.fetch_add(1, Ordering::Relaxed);
            Ok(Proof::new(trie.clone(), hashed.clone()).multiproof_v2(key.targets())?)
        });
        prefetch.submit(StateAccessHint {
            accounts: vec![],
            storages: std::iter::once((B256::ZERO, vec![slot])).collect(),
        });
        let job = jobs.recv().unwrap();
        job.prefetch.run(job.key);
        let mut targets = Key::Storage(B256::ZERO, slot).targets();
        assert_eq!(prefetch.cache.take_ready(&mut targets), expected);
        assert!(targets.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        prefetch.close();
        let mut targets = Key::Storage(B256::ZERO, slot).targets();
        assert_eq!(
            prefetch.cache.take_ready(&mut targets),
            expected,
            "completed proofs remain usable after close"
        );
        let mut partial = MultiProofTargetsV2 {
            account_targets: vec![],
            storage_targets: std::iter::once((
                B256::ZERO,
                vec![ProofV2Target::new(slot).with_parent(reth_trie::ProofV2TargetParent::new(0))],
            ))
            .collect(),
        };
        assert!(prefetch.cache.take_ready(&mut partial).is_empty());
        assert_eq!(partial.chunking_length(), 1);
    }

    #[cfg(feature = "trie-debug")]
    #[test]
    fn mixed_ready_and_fallback_proofs_produce_the_same_updated_storage_root() {
        use reth_trie_sparse::{
            ArenaParallelSparseTrie, LeafUpdate, SparseStateTrie, SparseTrie, TrieNodeEpoch,
        };
        let harness = TrieTestHarness::new(
            (1..=32).map(|i| (B256::with_last_byte(i), U256::from(i))).collect(),
        );
        let trie = harness.trie_cursor_factory();
        let hashed = harness.hashed_cursor_factory();
        let (prefetch, jobs, _cancel) = setup(2, move |key| {
            Ok(Proof::new(trie.clone(), hashed.clone()).multiproof_v2(key.targets())?)
        });
        let slots = [B256::with_last_byte(9), B256::with_last_byte(17), B256::repeat_byte(0xab)];
        prefetch.submit(StateAccessHint {
            accounts: vec![],
            storages: std::iter::once((B256::ZERO, vec![slots[0], slots[2]])).collect(),
        });
        for job in jobs.try_iter() {
            job.prefetch.run(job.key);
        }
        let mut targets = MultiProofTargetsV2::default();
        targets.storage_targets.insert(B256::ZERO, slots.iter().copied().map(Into::into).collect());
        let mut proof = prefetch.cache.take_ready(&mut targets);
        assert_eq!(targets.chunking_length(), 1);
        proof.extend(
            Proof::new(harness.trie_cursor_factory(), harness.hashed_cursor_factory())
                .multiproof_v2(targets)
                .unwrap(),
        );
        let mut sparse = SparseStateTrie::<ArenaParallelSparseTrie>::default();
        sparse.reveal_decoded_multiproof_v2(proof).unwrap();
        let changes =
            [(slots[0], U256::ZERO), (slots[1], U256::from(700)), (slots[2], U256::from(800))]
                .into_iter()
                .collect::<BTreeMap<_, _>>();
        let mut updates = changes
            .iter()
            .map(|(key, value)| {
                (
                    *key,
                    LeafUpdate::Changed(if value.is_zero() {
                        vec![]
                    } else {
                        alloy_rlp::encode_fixed_size(value).to_vec()
                    }),
                )
            })
            .collect();
        sparse
            .storage_trie_mut(&B256::ZERO)
            .unwrap()
            .update_leaves(&mut updates, |_, _| {
                panic!("completed proof should reveal changed paths")
            })
            .unwrap();
        assert!(updates.is_empty());
        assert_eq!(
            sparse.storage_root(&B256::ZERO, TrieNodeEpoch::new(1)).unwrap(),
            harness.get_root_with_updates(&changes).0
        );
    }

    #[test]
    fn queue_saturation_drops_advisory_work_instead_of_blocking() {
        let (prefetch, jobs, _cancel) = setup(1, |_| unreachable!("not running jobs"));
        prefetch.submit(hint(B256::with_last_byte(1)));
        prefetch.submit(hint(B256::with_last_byte(2)));
        assert_eq!(jobs.len(), 1);
        assert_eq!(prefetch.cache.data.lock().unwrap().requested.len(), 1);
    }

    #[test]
    fn slow_prefetch_does_not_hold_authoritative_update_or_finish_stream() {
        use crate::state_root_task::{StateRootHandle, StateRootMessage, StateRootTaskCancelGuard};
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release = Mutex::new(release_rx);
        let (prefetch, jobs, _cancel) = setup(1, move |_| {
            entered_tx.send(()).unwrap();
            let _ = release.lock().unwrap().recv_timeout(Duration::from_secs(5));
            Ok(DecodedMultiProofV2::default())
        });
        let (updates_tx, updates_rx) = crossbeam_channel::unbounded();
        let (guard, _cancel_rx) = StateRootTaskCancelGuard::channel();
        let mut handle = StateRootHandle::new(
            B256::ZERO,
            updates_tx,
            guard,
            mpsc::channel().1,
            mpsc::channel().1,
        )
        .with_isolated_prefetch(prefetch.clone());
        let stream = handle.take_isolated_hint_stream().unwrap();
        stream.on_access_hint(hint(B256::ZERO));
        let job = jobs.recv().unwrap();
        let worker = std::thread::spawn(move || job.prefetch.run(job.key));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        stream.close();
        let formal = handle.take_hashed_update_stream();
        formal.on_hashed_state_update(reth_trie::HashedPostState::default());
        formal.finish();
        assert!(matches!(
            updates_rx.recv_timeout(Duration::from_secs(1)),
            Ok(StateRootMessage::HashedStateUpdate(_))
        ));
        assert!(matches!(
            updates_rx.recv_timeout(Duration::from_secs(1)),
            Ok(StateRootMessage::FinishedStateUpdates)
        ));
        assert!(updates_rx.try_recv().is_err(), "no speculative messages in the formal queue");
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(prefetch.cache.data.lock().unwrap().ready.is_empty(), "late result discarded");
    }

    #[test]
    fn cancellation_skips_queued_provider_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let (prefetch, jobs, cancel) = setup(1, move |_| {
            invoked.fetch_add(1, Ordering::Relaxed);
            Ok(DecodedMultiProofV2::default())
        });
        prefetch.submit(hint(B256::ZERO));
        drop(cancel);
        let job = jobs.recv().unwrap();
        job.prefetch.run(job.key);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(prefetch.cache.data.lock().unwrap().ready.is_empty());
    }

    #[test]
    fn contended_cache_never_delays_formal_lookup() {
        let cache = Arc::new(ReadyProofCache::default());
        let guard = cache.data.lock().unwrap();
        let other = cache.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut targets = Key::Account(B256::ZERO).targets();
            let result = other.take_ready(&mut targets);
            tx.send((result.is_empty(), targets.chunking_length())).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(guard);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), (true, 1));
    }

    #[test]
    fn admission_and_completed_results_are_bounded() {
        let (prefetch, jobs, _cancel) = setup(MAX_KEYS + 1, |_| {
            Ok(DecodedMultiProofV2 {
                account_proofs: vec![reth_trie::ProofTrieNodeV2::empty(); MAX_PROOF_NODES + 1],
                storage_proofs: Default::default(),
            })
        });
        prefetch.submit(StateAccessHint {
            accounts: (0..256).map(|i| B256::from(U256::from(i))).collect(),
            storages: Default::default(),
        });
        assert_eq!(jobs.len(), MAX_KEYS);
        let job = jobs.recv().unwrap();
        job.prefetch.run(job.key);
        assert!(prefetch.cache.data.lock().unwrap().ready.is_empty());
        prefetch.close();
        prefetch.submit(hint(B256::repeat_byte(0xff)));
        assert_eq!(jobs.len(), MAX_KEYS - 1);
    }

    #[test]
    fn attempts_do_not_share_ready_proofs_and_failure_keeps_fallback_targets() {
        let (first, jobs, _cancel1) = setup(1, |_| Ok(DecodedMultiProofV2::default()));
        let (other, other_jobs, _cancel2) =
            setup(1, |_| Err(StateRootTaskError::Other("speculative failure".into())));
        first.submit(hint(B256::ZERO));
        let job = jobs.recv().unwrap();
        job.prefetch.run(job.key);
        let mut targets = Key::Account(B256::ZERO).targets();
        assert!(other.cache.take_ready(&mut targets).is_empty());
        assert_eq!(targets.chunking_length(), 1);
        other.submit(hint(B256::ZERO));
        let job = other_jobs.recv().unwrap();
        job.prefetch.run(job.key);
        assert!(other.cache.take_ready(&mut targets).is_empty());
        assert_eq!(targets.chunking_length(), 1);
    }
}
