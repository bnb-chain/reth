//! Opt-in diagnostics for a bounded set of peers, without changing wire or cache decisions.
//!
//! `RETH_NETWORK_DIAGNOSTIC_PEERS` is a comma-separated allowlist of at most 16 node IDs.
//! Empty/unset disables the instrumentation. Labels are allocated at connection setup, never
//! from transaction hashes. Sampled hash traces use `net::diagnostics=debug` and a process-wide
//! ten-second log budget; counters remain available when logging is disabled.

use crate::{
    cache::{LruCache, LruMap},
    session::MessageKind,
};
use alloy_primitives::{map::FbBuildHasher, B256};
use reth_metrics::metrics::{Counter, Gauge, Histogram};
use reth_network_peers::PeerId;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};

const MAX_PEERS: usize = 16;
const MAX_SAMPLES: u32 = 256;
const SAMPLE_TTL: Duration = Duration::from_secs(120);
const LOG_INTERVAL_MS: u64 = 10_000;
const SLOW_SEND: Duration = Duration::from_millis(100);
const KINDS: [MessageKind; 7] = [
    MessageKind::Block,
    MessageKind::BlockHashes,
    MessageKind::Transactions,
    MessageKind::TransactionHashes,
    MessageKind::Request,
    MessageKind::Response,
    MessageKind::Other,
];
const REASONS: [&str; 4] = ["capacity", "command_full", "closed", "unsupported"];
static PEERS: LazyLock<Vec<PeerId>> = LazyLock::new(|| {
    let value = std::env::var("RETH_NETWORK_DIAGNOSTIC_PEERS").unwrap_or_default();
    match parse_peers(&value) {
        Ok(peers) => peers,
        Err(error) => {
            tracing::error!(target: "net::diagnostics", ?error,
                "Invalid diagnostic peer allowlist; diagnostics disabled");
            Vec::new()
        }
    }
});
static LOG_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static LAST_LOG: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
enum AllowlistError {
    InvalidPeer,
    TooManyPeers,
}

fn parse_peers(value: &str) -> Result<Vec<PeerId>, AllowlistError> {
    let mut peers = Vec::new();
    for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let peer = part.parse().map_err(|_| AllowlistError::InvalidPeer)?;
        if !peers.contains(&peer) {
            if peers.len() == MAX_PEERS {
                return Err(AllowlistError::TooManyPeers)
            }
            peers.push(peer);
        }
    }
    Ok(peers)
}

fn enabled(peer: PeerId) -> bool {
    PEERS.contains(&peer)
}

fn claim_log(last: &AtomicU64, now: u64) -> bool {
    let previous = last.load(Ordering::Relaxed);
    if previous != 0 && now.saturating_sub(previous) < LOG_INTERVAL_MS {
        return false
    }
    last.compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
}

fn log_allowed() -> bool {
    tracing::enabled!(target: "net::diagnostics", tracing::Level::DEBUG) &&
        claim_log(&LAST_LOG, LOG_EPOCH.elapsed().as_millis() as u64 + 1)
}

fn kind_index(kind: MessageKind) -> usize {
    match kind {
        MessageKind::Block => 0,
        MessageKind::BlockHashes => 1,
        MessageKind::Transactions => 2,
        MessageKind::TransactionHashes => 3,
        MessageKind::Request => 4,
        MessageKind::Response => 5,
        MessageKind::Other => 6,
    }
}

/// Handles are shared by the sender and session. The allowlist bounds metric cardinality even
/// across reconnects. Rejections count send attempts, not unique blocks or transactions.
#[derive(Debug)]
pub(crate) struct PeerSendDiagnostics {
    peer: String,
    rejected: [[Counter; 4]; 7],
    queued: [Histogram; 7],
    unwritable: Histogram,
}

impl PeerSendDiagnostics {
    pub(crate) fn for_peer(peer: PeerId) -> Option<Arc<Self>> {
        enabled(peer).then(|| Arc::new(Self::new(peer)))
    }

    fn new(peer: PeerId) -> Self {
        let peer = format!("{peer:#x}");
        Self {
            rejected: std::array::from_fn(|kind| {
                std::array::from_fn(|reason| {
                    metrics::counter!("network.peer_send_rejected_total", "peer" => peer.clone(),
                    "kind" => KINDS[kind].label(), "reason" => REASONS[reason])
                })
            }),
            queued: std::array::from_fn(|kind| {
                metrics::histogram!(
                "network.peer_send_queue_wait_seconds", "peer" => peer.clone(), "kind" => KINDS[kind].label())
            }),
            unwritable: metrics::histogram!("network.peer_sink_unwritable_seconds", "peer" => peer.clone()),
            peer,
        }
    }

    pub(crate) fn rejected(&self, kind: MessageKind, reason: &'static str) {
        if let Some(index) = REASONS.iter().position(|value| *value == reason) {
            self.rejected[kind_index(kind)][index].increment(1);
        }
    }

    fn handoff(&self, kind: MessageKind, elapsed: Duration) {
        self.queued[kind_index(kind)].record(elapsed.as_secs_f64());
        if elapsed >= SLOW_SEND && log_allowed() {
            tracing::debug!(target: "net::diagnostics", peer = %self.peer,
                kind = kind.label(), queue_ms = elapsed.as_secs_f64() * 1000.0,
                "Sampled slow peer transport handoff");
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(peer: PeerId) -> Arc<Self> {
        Arc::new(Self::new(peer))
    }
}

/// Measures local transport readiness only while application messages are waiting. It is not
/// RTT, TCP acknowledgement time, or remote execution time. No extra transport polls are made.
#[derive(Debug)]
pub(crate) struct SessionDiagnostics {
    pub(crate) peer: Arc<PeerSendDiagnostics>,
    blocked_since: Option<Instant>,
}

impl SessionDiagnostics {
    pub(crate) fn new(peer: Arc<PeerSendDiagnostics>) -> Self {
        Self { peer, blocked_since: None }
    }

    pub(crate) fn readiness(&mut self, ready: bool, queued: bool, now: Instant) {
        if ready || !queued {
            if let Some(start) = self.blocked_since.take() {
                self.peer.unwritable.record(now.duration_since(start).as_secs_f64());
            }
        } else if self.blocked_since.is_none() {
            self.blocked_since = Some(now);
        }
    }

    pub(crate) fn handoff(&self, kind: MessageKind, elapsed: Duration) {
        self.peer.handoff(kind, elapsed);
    }
}

impl Drop for SessionDiagnostics {
    fn drop(&mut self) {
        // Closing a blocked connection must not silently discard its last waiting interval.
        if let Some(start) = self.blocked_since.take() {
            self.peer.unwritable.record(start.elapsed().as_secs_f64());
        }
    }
}

/// Sources reflect existing cache writes. Explicit replies and forced/bootstrap sends are
/// counted for churn but are never classified as proactive echo candidates.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HistorySource {
    Announcement,
    Broadcast,
    Propagate,
    Forced,
    Reply,
    Bootstrap,
}
impl HistorySource {
    const ALL: [Self; 6] = [
        Self::Announcement,
        Self::Broadcast,
        Self::Propagate,
        Self::Forced,
        Self::Reply,
        Self::Bootstrap,
    ];
    const fn index(self) -> usize {
        self as usize
    }
    const fn label(self) -> &'static str {
        match self {
            Self::Announcement => "announcement",
            Self::Broadcast => "broadcast",
            Self::Propagate => "propagate",
            Self::Forced => "forced",
            Self::Reply => "reply",
            Self::Bootstrap => "bootstrap",
        }
    }
    const fn received(self) -> bool {
        matches!(self, Self::Announcement | Self::Broadcast)
    }
}

#[derive(Debug, Clone, Copy)]
struct HistorySample {
    received: Instant,
    evicted: Option<Instant>,
}

/// Diagnostic sidecar, owned by TransactionsManager; it never changes the real peer LRU.
/// Hash sampling is deterministic (1/256), bounded to 256 entries per allowed peer. Only
/// receive-to-candidate intervals within 120s qualify; stale entries are removed lazily.
/// Echo counters are sampled *candidates before queue admission*, not successful wire sends.
#[derive(Debug)]
pub(crate) struct HistoryDiagnostics {
    peer: String,
    samples: LruMap<B256, HistorySample, schnellru::ByLength, FbBuildHasher<32>>,
    updates: [[Counter; 3]; 6],
    counts: [[u64; 3]; 6],
    occupancy: Gauge,
    echoes: Counter,
    last_flush: Instant,
}

impl HistoryDiagnostics {
    pub(crate) fn for_peer(peer: PeerId) -> Option<Self> {
        enabled(peer).then(|| Self::new(peer, Instant::now()))
    }

    fn new(peer: PeerId, now: Instant) -> Self {
        let peer = format!("{peer:#x}");
        Self {
            updates: std::array::from_fn(|source| {
                std::array::from_fn(|action| {
                    metrics::counter!("network.peer_tx_history_updates_total", "peer" => peer.clone(),
                    "source" => HistorySource::ALL[source].label(),
                    "action" => ["new", "refresh", "capacity_evict"][action])
                })
            }),
            occupancy: metrics::gauge!("network.peer_tx_history_entries", "peer" => peer.clone()),
            echoes: metrics::counter!("network.peer_tx_history_sampled_echo_candidates_total", "peer" => peer.clone()),
            peer,
            samples: LruMap::with_hasher(MAX_SAMPLES, Default::default()),
            counts: [[0; 3]; 6],
            last_flush: now,
        }
    }

    pub(crate) fn insert(
        &mut self,
        cache: &mut LruCache<B256, FbBuildHasher<32>>,
        hash: B256,
        source: HistorySource,
    ) -> bool {
        let now = Instant::now();
        let (new, evicted) = cache.insert_with_capacity_eviction(hash);
        self.counts[source.index()][usize::from(!new)] += 1;
        if let Some(evicted) = evicted {
            self.counts[source.index()][2] += 1;
            if let Some(sample) = self.samples.get(&evicted) {
                sample.evicted = Some(now);
            }
        }
        self.observe(hash, source, new, now);
        if now.duration_since(self.last_flush) >= Duration::from_secs(1) {
            self.flush(cache.len(), now);
        }
        new
    }

    fn observe(&mut self, hash: B256, source: HistorySource, new: bool, now: Instant) {
        if hash[0] != 0 {
            return
        }
        if source.received() {
            self.samples.insert(hash, HistorySample { received: now, evicted: None });
        } else if matches!(source, HistorySource::Propagate) &&
            new &&
            let Some(sample) = self.samples.peek(&hash).copied() &&
            let Some(evicted) = sample.evicted &&
            now.duration_since(sample.received) <= SAMPLE_TTL
        {
            self.echoes.increment(1);
            if log_allowed() {
                tracing::debug!(target: "net::diagnostics", peer = %self.peer, %hash,
                    received_age_ms = now.duration_since(sample.received).as_secs_f64() * 1000.0,
                    evicted_age_ms = now.duration_since(evicted).as_secs_f64() * 1000.0,
                    "Sampled proactive transaction candidate after history eviction");
            }
            self.samples.remove(&hash);
        }
        // Eligibility expires lazily; memory stays bounded without a timer task or full scan.
        if let Some(sample) = self.samples.peek(&hash) &&
            now.duration_since(sample.received) > SAMPLE_TTL
        {
            self.samples.remove(&hash);
        }
    }

    fn flush(&mut self, entries: usize, now: Instant) {
        for (metrics, counts) in self.updates.iter().zip(&mut self.counts) {
            for (metric, count) in metrics.iter().zip(counts) {
                metric.increment(std::mem::take(count));
            }
        }
        self.occupancy.set(entries as f64);
        self.last_flush = now;
    }
}

impl Drop for HistoryDiagnostics {
    fn drop(&mut self) {
        self.flush(0, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestRecorder {
        counters: std::sync::Mutex<std::collections::HashMap<metrics::Key, Arc<AtomicU64>>>,
        gauges: std::sync::Mutex<std::collections::HashMap<metrics::Key, Arc<AtomicU64>>>,
    }

    impl metrics::Recorder for TestRecorder {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> Counter {
            Counter::from_arc(self.counters.lock().unwrap().entry(key.clone()).or_default().clone())
        }
        fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> Gauge {
            Gauge::from_arc(self.gauges.lock().unwrap().entry(key.clone()).or_default().clone())
        }
        fn register_histogram(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }

    #[test]
    fn recorded_metrics_keep_peer_kind_and_source_labels_and_flush_on_disconnect() {
        let recorder = TestRecorder::default();
        metrics::with_local_recorder(&recorder, || {
            let first = PeerSendDiagnostics::for_test(PeerId::repeat_byte(1));
            let second = PeerSendDiagnostics::for_test(PeerId::repeat_byte(2));
            first.rejected(MessageKind::Block, "capacity");
            first.rejected(MessageKind::Transactions, "command_full");
            second.rejected(MessageKind::Block, "closed");
            let mut history = HistoryDiagnostics::new(PeerId::repeat_byte(1), Instant::now());
            let mut cache = LruCache::with_hasher(0, Default::default());
            history.insert(&mut cache, B256::ZERO, HistorySource::Broadcast);
            history.insert(&mut cache, B256::ZERO, HistorySource::Announcement);
            history.insert(&mut cache, B256::with_last_byte(1), HistorySource::Announcement);
            history.insert(&mut cache, B256::ZERO, HistorySource::Propagate);
            history.flush(cache.len(), Instant::now());
            let occupancy = recorder.gauges.lock().unwrap().values().next().unwrap().clone();
            assert_eq!(f64::from_bits(occupancy.load(Ordering::Relaxed)), 1.0);
            drop(history);
            assert_eq!(f64::from_bits(occupancy.load(Ordering::Relaxed)), 0.0);
        });
        let counters = recorder.counters.lock().unwrap();
        let nonzero: Vec<_> =
            counters.iter().filter(|(_, value)| value.load(Ordering::Relaxed) > 0).collect();
        assert_eq!(nonzero.len(), 10);
        for (key, _) in nonzero {
            assert!(key.labels().any(|label| label.key() == "peer"));
            assert!(!key.labels().any(|label| label.key() == "hash"));
        }
        let echoes: Vec<_> = counters
            .iter()
            .filter(|(key, _)| key.name().ends_with("sampled_echo_candidates_total"))
            .collect();
        assert_eq!(echoes.len(), 1);
        assert_eq!(echoes[0].1.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn allowlist_is_bounded_deduplicated_and_strict() {
        assert_eq!(parse_peers(" , "), Ok(Vec::new()));
        let peer = PeerId::repeat_byte(1);
        assert_eq!(parse_peers(&format!("{peer:#x}, {peer:#x}")), Ok(vec![peer]));
        assert_eq!(parse_peers("not-a-node-id"), Err(AllowlistError::InvalidPeer));
        let value = (0..=MAX_PEERS)
            .map(|i| format!("{:#x}", PeerId::repeat_byte(i as u8)))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_peers(&value), Err(AllowlistError::TooManyPeers));
    }

    #[test]
    fn shared_log_budget_rejects_bursts_and_clock_regression() {
        let last = AtomicU64::new(0);
        assert!(claim_log(&last, 1));
        assert!(!claim_log(&last, 1));
        assert!(!claim_log(&last, 0));
        assert!(!claim_log(&last, LOG_INTERVAL_MS));
        assert!(claim_log(&last, LOG_INTERVAL_MS + 1));
    }

    #[test]
    fn observation_preserves_lru_capacity_refresh_and_order() {
        for limit in [0, 1, 16, 320, 32768] {
            let mut plain = LruCache::new(limit);
            let mut observed = LruCache::new(limit);
            for i in 0..(limit + 5) * 2 {
                let key = if i % 3 == 0 { i / 2 } else { i };
                let before = plain.len();
                let should_evict = before > limit as usize && !plain.contains(&key);
                let expected_new = plain.insert(key);
                let (new, evicted) = observed.insert_with_capacity_eviction(key);
                assert_eq!(new, expected_new);
                assert_eq!(evicted.is_some(), should_evict);
                if let Some(evicted) = evicted {
                    assert!(!plain.contains(&evicted));
                }
                assert_eq!(observed.len(), plain.len());
                // Compare the full order at small boundaries and periodically for the large LRU.
                if limit < 1000 || i % 1024 == 0 {
                    assert_eq!(
                        observed.iter().copied().collect::<Vec<_>>(),
                        plain.iter().copied().collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    #[test]
    fn history_reports_only_sampled_receive_eviction_proactive_sequences() {
        let now = Instant::now();
        let mut history = HistoryDiagnostics::new(PeerId::ZERO, now);
        let mut cache = LruCache::with_hasher(1, Default::default());
        let hash = B256::ZERO;
        assert!(history.insert(&mut cache, hash, HistorySource::Broadcast));
        assert!(!history.insert(&mut cache, hash, HistorySource::Announcement));
        assert_eq!(history.counts[HistorySource::Broadcast.index()], [1, 0, 0]);
        for i in 1..=2 {
            history.insert(&mut cache, B256::with_last_byte(i), HistorySource::Announcement);
        }
        assert!(!cache.contains(&hash));
        assert!(history.samples.peek(&hash).unwrap().evicted.is_some());
        let observed_now = Instant::now();
        history.observe(hash, HistorySource::Reply, true, observed_now);
        history.observe(hash, HistorySource::Forced, true, observed_now);
        history.observe(hash, HistorySource::Bootstrap, true, observed_now);
        assert!(history.samples.peek(&hash).is_some());
        history.observe(hash, HistorySource::Propagate, true, observed_now);
        assert!(history.samples.peek(&hash).is_none());
        assert_eq!(history.counts[HistorySource::Announcement.index()][2], 1);
    }

    #[test]
    fn history_bounds_samples_and_expires_eligibility() {
        let start = Instant::now();
        let mut history = HistoryDiagnostics::new(PeerId::ZERO, start);
        for i in 0..1024u64 {
            history.observe(
                B256::from(alloy_primitives::U256::from(i)),
                HistorySource::Broadcast,
                true,
                start,
            );
        }
        assert_eq!(history.samples.len(), MAX_SAMPLES as usize);
        let hash = B256::from(alloy_primitives::U256::from(1023));
        history.samples.get(&hash).unwrap().evicted = Some(start);
        history.observe(
            hash,
            HistorySource::Propagate,
            true,
            start + SAMPLE_TTL + Duration::from_secs(1),
        );
        assert!(history.samples.peek(&hash).is_none());
        let unsampled = B256::repeat_byte(1);
        history.observe(unsampled, HistorySource::Broadcast, true, start);
        assert!(history.samples.peek(&unsampled).is_none());
    }

    #[test]
    fn diagnostic_sidecar_keeps_receive_and_propagation_decisions_identical() {
        for limit in [1, 320, 32768] {
            let mut plain =
                LruCache::<B256, FbBuildHasher<32>>::with_hasher(limit, Default::default());
            let mut monitored = LruCache::with_hasher(limit, Default::default());
            let mut history = HistoryDiagnostics::new(PeerId::ZERO, Instant::now());
            for source in [HistorySource::Broadcast, HistorySource::Propagate, HistorySource::Reply]
            {
                for i in 0..1024u64 {
                    let hash = B256::from(alloy_primitives::U256::from(i));
                    assert_eq!(history.insert(&mut monitored, hash, source), plain.insert(hash));
                }
                assert_eq!(
                    monitored.iter().copied().collect::<Vec<_>>(),
                    plain.iter().copied().collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn readiness_wait_starts_once_and_excludes_idle_connections() {
        let peer = PeerSendDiagnostics::for_test(PeerId::ZERO);
        let mut diagnostics = SessionDiagnostics::new(peer);
        let now = Instant::now();
        diagnostics.readiness(false, false, now);
        assert_eq!(diagnostics.blocked_since, None);
        diagnostics.readiness(false, true, now);
        diagnostics.readiness(false, true, now + Duration::from_secs(1));
        assert_eq!(diagnostics.blocked_since, Some(now));
        diagnostics.readiness(true, true, now + Duration::from_secs(2));
        assert_eq!(diagnostics.blocked_since, None);
        diagnostics.readiness(false, true, now);
        diagnostics.readiness(false, false, now + Duration::from_secs(1));
        assert_eq!(diagnostics.blocked_since, None);
    }
}
