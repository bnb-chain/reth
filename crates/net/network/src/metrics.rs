use metrics::Histogram;
use reth_eth_wire::DisconnectReason;
use reth_ethereum_primitives::TxType;
use reth_metrics::{
    metrics::{Counter, Gauge},
    Metrics,
};
use reth_network_types::peers::reputation::ReputationChangeKind;

/// Scope for monitoring transactions sent from the manager to the tx manager
pub(crate) const NETWORK_POOL_TRANSACTIONS_SCOPE: &str = "network.pool.transactions";

/// Metrics for the entire network, handled by `NetworkManager`
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct NetworkMetrics {
    /// Number of currently connected peers
    pub(crate) connected_peers: Gauge,

    /// Number of currently backed off peers
    pub(crate) backed_off_peers: Gauge,

    /// Number of peers known to the node
    pub(crate) tracked_peers: Gauge,

    /// Cumulative number of failures of pending sessions
    pub(crate) pending_session_failures: Counter,

    /// Total number of sessions closed
    pub(crate) closed_sessions: Counter,

    /// Number of active incoming connections
    pub(crate) incoming_connections: Gauge,

    /// Number of active outgoing connections
    pub(crate) outgoing_connections: Gauge,

    /// Number of currently pending outgoing connections
    pub(crate) pending_outgoing_connections: Gauge,

    /// Total number of pending connections, incoming and outgoing.
    pub(crate) total_pending_connections: Gauge,

    /// Total Number of incoming connections handled
    pub(crate) total_incoming_connections: Counter,

    /// Total Number of outgoing connections established
    pub(crate) total_outgoing_connections: Counter,

    /// Number of invalid/malformed messages received from peers
    pub(crate) invalid_messages_received: Counter,

    /// Number of Eth Requests dropped due to channel being at full capacity
    pub(crate) total_dropped_eth_requests_at_full_capacity: Counter,

    /* ================ POLL DURATION ================ */

    /* -- Total poll duration of `NetworksManager` future -- */
    /// Duration in seconds of call to
    /// [`NetworkManager`](crate::NetworkManager)'s poll function.
    ///
    /// True duration of this call, should be sum of the accumulated durations of calling nested
    // items.
    pub(crate) duration_poll_network_manager: Gauge,

    /* -- Poll duration of items nested in `NetworkManager` future -- */
    /// Time spent streaming messages sent over the [`NetworkHandle`](crate::NetworkHandle), which
    /// can be cloned and shared via [`NetworkManager::handle`](crate::NetworkManager::handle), in
    /// one call to poll the [`NetworkManager`](crate::NetworkManager) future. At least
    /// [`TransactionsManager`](crate::transactions::TransactionsManager) holds this handle.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_network_handle: Gauge,
    /// Time spent polling [`Swarm`](crate::swarm::Swarm), in one call to poll the
    /// [`NetworkManager`](crate::NetworkManager) future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_swarm: Gauge,
}

/// Metrics for `SessionManager`
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct SessionManagerMetrics {
    /// Number of successful outgoing dial attempts.
    pub(crate) total_dial_successes: Counter,
    /// Number of dropped outgoing peer messages.
    pub(crate) total_outgoing_peer_messages_dropped: Counter,
    /// Number of queued outgoing messages
    pub(crate) queued_outgoing_messages: Gauge,
}

/// Metrics for the [`TransactionsManager`](crate::transactions::TransactionsManager).
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct TransactionsManagerMetrics {
    /* ================ BROADCAST ================ */
    /// Total number of propagated transactions
    pub(crate) propagated_transactions: Counter,
    /// Total number of reported bad transactions
    pub(crate) reported_bad_transactions: Counter,

    /* -- Freq txns already marked as seen by peer -- */
    /// Total number of messages from a peer, announcing transactions that have already been
    /// marked as seen by that peer.
    pub(crate) messages_with_hashes_already_seen_by_peer: Counter,
    /// Total number of messages from a peer, with transaction that have already been marked as
    /// seen by that peer.
    pub(crate) messages_with_transactions_already_seen_by_peer: Counter,
    /// Total number of occurrences, of a peer announcing a transaction that has already been
    /// marked as seen by that peer.
    pub(crate) occurrences_hash_already_seen_by_peer: Counter,
    /// Total number of times a transaction is seen from a peer, that has already been marked as
    /// seen by that peer.
    pub(crate) occurrences_of_transaction_already_seen_by_peer: Counter,

    /* -- Freq txns already in pool -- */
    /// Total number of times a hash is announced that is already in the local pool.
    pub(crate) occurrences_hashes_already_in_pool: Counter,
    /// Total number of times a transaction is sent that is already in the local pool.
    pub(crate) occurrences_transactions_already_in_pool: Counter,

    /* ================ POOL IMPORTS ================ */
    /// Number of transactions about to be imported into the pool.
    pub(crate) pending_pool_imports: Gauge,
    /// Total number of bad imports, imports that fail because the transaction is badly formed
    /// (i.e. have no chance of passing validation, unlike imports that fail due to e.g. nonce
    /// gaps).
    pub(crate) bad_imports: Counter,
    /// Number of inflight requests at which the
    /// [`TransactionPool`](reth_transaction_pool::TransactionPool) is considered to be at
    /// capacity. Note, this is not a limit to the number of inflight requests, but a health
    /// measure.
    pub(crate) capacity_pending_pool_imports: Counter,
    /// The time it took to prepare transactions for import. This is mostly sender recovery.
    pub(crate) pool_import_prepare_duration: Histogram,

    /* ================ POLL DURATION ================ */

    /* -- Total poll duration of `TransactionsManager` future -- */
    /// Duration in seconds of call to
    /// [`TransactionsManager`](crate::transactions::TransactionsManager)'s poll function.
    ///
    /// Updating metrics could take time, so the true duration of this call could
    /// be longer than the sum of the accumulated durations of polling nested items.
    pub(crate) duration_poll_tx_manager: Gauge,

    /* -- Poll duration of items nested in `TransactionsManager` future -- */
    /// Accumulated time spent streaming session updates and updating peers accordingly, in
    /// one call to poll the [`TransactionsManager`](crate::transactions::TransactionsManager)
    /// future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_network_events: Gauge,
    /// Accumulated time spent flushing the queue of batched pending pool imports into pool, in
    /// one call to poll the [`TransactionsManager`](crate::transactions::TransactionsManager)
    /// future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_pending_pool_imports: Gauge,
    /// Accumulated time spent streaming transaction and announcement broadcast, queueing for
    /// pool import or requesting respectively, in one call to poll the
    /// [`TransactionsManager`](crate::transactions::TransactionsManager) future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_transaction_events: Gauge,
    /// Accumulated time spent streaming fetch events, queueing for pool import on successful
    /// fetch, in one call to poll the
    /// [`TransactionsManager`](crate::transactions::TransactionsManager) future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_fetch_events: Gauge,
    /// Accumulated time spent streaming and propagating transactions that were successfully
    /// imported into the pool, in one call to poll the
    /// [`TransactionsManager`](crate::transactions::TransactionsManager) future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_imported_transactions: Gauge,
    /// Accumulated time spent assembling and sending requests for hashes fetching pending, in
    /// one call to poll the [`TransactionsManager`](crate::transactions::TransactionsManager)
    /// future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_fetch_pending_hashes: Gauge,
    /// Accumulated time spent streaming commands and propagating, fetching and serving
    /// transactions accordingly, in one call to poll the
    /// [`TransactionsManager`](crate::transactions::TransactionsManager) future.
    ///
    /// Duration in seconds.
    pub(crate) acc_duration_poll_commands: Gauge,
}

/// Metrics for the [`TransactionsManager`](crate::transactions::TransactionsManager).
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct TransactionFetcherMetrics {
    /// Currently active outgoing [`GetPooledTransactions`](reth_eth_wire::GetPooledTransactions)
    /// requests.
    pub(crate) inflight_transaction_requests: Gauge,
    /// Number of inflight requests at which the
    /// [`TransactionFetcher`](crate::transactions::TransactionFetcher) is considered to be at
    /// capacity. Note, this is not a limit to the number of inflight requests, but a health
    /// measure.
    pub(crate) capacity_inflight_requests: Counter,
    /// Hashes in currently active outgoing
    /// [`GetPooledTransactions`](reth_eth_wire::GetPooledTransactions) requests.
    pub(crate) hashes_inflight_transaction_requests: Gauge,
    /// How often we failed to send a request to the peer because the channel was full.
    pub(crate) egress_peer_channel_full: Counter,
    /// Total number of hashes pending fetch.
    pub(crate) hashes_pending_fetch: Gauge,
    /// Total number of fetched transactions.
    pub(crate) fetched_transactions: Counter,
    /// Total number of transactions that were received in
    /// [`PooledTransactions`](reth_eth_wire::PooledTransactions) responses, that weren't
    /// requested.
    pub(crate) unsolicited_transactions: Counter,
    /* ================ SEARCH DURATION ================ */
    /// Time spent searching for an idle peer in call to
    /// [`TransactionFetcher::find_any_idle_fallback_peer_for_any_pending_hash`](crate::transactions::TransactionFetcher::find_any_idle_fallback_peer_for_any_pending_hash).
    ///
    /// Duration in seconds.
    pub(crate) duration_find_idle_fallback_peer_for_any_pending_hash: Gauge,

    /// Time spent searching for hashes pending fetch, announced by a given peer in
    /// [`TransactionFetcher::fill_request_from_hashes_pending_fetch`](crate::transactions::TransactionFetcher::fill_request_from_hashes_pending_fetch).
    ///
    /// Duration in seconds.
    pub(crate) duration_fill_request_from_hashes_pending_fetch: Gauge,
}

/// Measures the duration of executing the given code block. The duration is added to the given
/// accumulator value passed as a mutable reference.
#[macro_export]
macro_rules! duration_metered_exec {
    ($code:expr, $acc:expr) => {{
        let start = std::time::Instant::now();

        let res = $code;

        $acc += start.elapsed();

        res
    }};
}

/// Metrics for Disconnection types
///
/// These are just counters, and ideally we would implement these metrics on a peer-by-peer basis,
/// in that we do not double-count peers for `TooManyPeers` if we make an outgoing connection and
/// get disconnected twice
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct DisconnectMetrics {
    /// Number of peer disconnects due to `DisconnectRequested` (0x00)
    pub(crate) disconnect_requested: Counter,

    /// Number of peer disconnects due to `TcpSubsystemError` (0x01)
    pub(crate) tcp_subsystem_error: Counter,

    /// Number of peer disconnects due to `ProtocolBreach` (0x02)
    pub(crate) protocol_breach: Counter,

    /// Number of peer disconnects due to `UselessPeer` (0x03)
    pub(crate) useless_peer: Counter,

    /// Number of peer disconnects due to `TooManyPeers` (0x04)
    pub(crate) too_many_peers: Counter,

    /// Number of peer disconnects due to `AlreadyConnected` (0x05)
    pub(crate) already_connected: Counter,

    /// Number of peer disconnects due to `IncompatibleP2PProtocolVersion` (0x06)
    pub(crate) incompatible: Counter,

    /// Number of peer disconnects due to `NullNodeIdentity` (0x07)
    pub(crate) null_node_identity: Counter,

    /// Number of peer disconnects due to `ClientQuitting` (0x08)
    pub(crate) client_quitting: Counter,

    /// Number of peer disconnects due to `UnexpectedHandshakeIdentity` (0x09)
    pub(crate) unexpected_identity: Counter,

    /// Number of peer disconnects due to `ConnectedToSelf` (0x0a)
    pub(crate) connected_to_self: Counter,

    /// Number of peer disconnects due to `PingTimeout` (0x0b)
    pub(crate) ping_timeout: Counter,

    /// Number of peer disconnects due to `SubprotocolSpecific` (0x10)
    pub(crate) subprotocol_specific: Counter,
}

impl DisconnectMetrics {
    /// Increments the proper counter for the given disconnect reason
    pub(crate) fn increment(&self, reason: DisconnectReason) {
        match reason {
            DisconnectReason::DisconnectRequested => self.disconnect_requested.increment(1),
            DisconnectReason::TcpSubsystemError => self.tcp_subsystem_error.increment(1),
            DisconnectReason::ProtocolBreach => self.protocol_breach.increment(1),
            DisconnectReason::UselessPeer => self.useless_peer.increment(1),
            DisconnectReason::TooManyPeers => self.too_many_peers.increment(1),
            DisconnectReason::AlreadyConnected => self.already_connected.increment(1),
            DisconnectReason::IncompatibleP2PProtocolVersion => self.incompatible.increment(1),
            DisconnectReason::NullNodeIdentity => self.null_node_identity.increment(1),
            DisconnectReason::ClientQuitting => self.client_quitting.increment(1),
            DisconnectReason::UnexpectedHandshakeIdentity => self.unexpected_identity.increment(1),
            DisconnectReason::ConnectedToSelf => self.connected_to_self.increment(1),
            DisconnectReason::PingTimeout => self.ping_timeout.increment(1),
            DisconnectReason::SubprotocolSpecific => self.subprotocol_specific.increment(1),
        }
    }
}

/// Metrics for reputation-driven peer-management actions, broken out by
/// [`ReputationChangeKind`] (the reason a change was applied) and by outcome
/// (whether the change crossed a banning threshold).
///
/// Without these counters, a peer-pool drain induced by reputation bans is
/// invisible at the metrics layer: ban events go to the `ban_list`, which is
/// not exposed as a gauge, and the `DisconnectMetrics` counters cannot tell
/// whether a `DisconnectRequested` was a graceful close or a rep-driven kick.
/// Tracking change-kind frequency and ban outcomes makes it possible to
/// correlate peer-count drops with their root cause (e.g. a flood of
/// `BadBlock` penalties from a network-layer issue).
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct ReputationMetrics {
    /// Reputation change applied due to an unspecific bad message.
    pub(crate) reputation_changes_bad_message: Counter,
    /// Reputation change applied due to a bad block (pre-merge / `PoW` only).
    pub(crate) reputation_changes_bad_block: Counter,
    /// Reputation change applied due to a bad transaction.
    pub(crate) reputation_changes_bad_transactions: Counter,
    /// Reputation change applied due to a bad announcement.
    pub(crate) reputation_changes_bad_announcement: Counter,
    /// Reputation change applied due to a peer re-announcing a hash/tx already
    /// known to be seen by that peer.
    pub(crate) reputation_changes_already_seen_transaction: Counter,
    /// Reputation change applied due to a peer response timeout.
    pub(crate) reputation_changes_timeout: Counter,
    /// Reputation change applied due to a peer protocol violation.
    pub(crate) reputation_changes_bad_protocol: Counter,
    /// Reputation change applied due to a failed connection attempt.
    pub(crate) reputation_changes_failed_to_connect: Counter,
    /// Reputation change applied due to the peer dropping the connection.
    pub(crate) reputation_changes_dropped: Counter,
    /// Reputation reset (back to the default value).
    pub(crate) reputation_changes_reset: Counter,
    /// Custom reputation change applied via [`ReputationChangeKind::Other`].
    pub(crate) reputation_changes_other: Counter,

    /// Reputation changes that crossed the banning threshold and resulted in
    /// the peer being added to the ban list (without an explicit disconnect).
    pub(crate) bans_total: Counter,
    /// Reputation changes that resulted in both an enqueued `Disconnect` and
    /// a ban — the dominant ban path in practice.
    pub(crate) disconnect_and_bans_total: Counter,
    /// Reputation resets that crossed the un-ban threshold.
    pub(crate) unbans_total: Counter,
}

impl ReputationMetrics {
    /// Increments the per-`ReputationChangeKind` counter.
    pub(crate) fn increment_kind(&self, kind: ReputationChangeKind) {
        match kind {
            ReputationChangeKind::BadMessage => self.reputation_changes_bad_message.increment(1),
            ReputationChangeKind::BadBlock => self.reputation_changes_bad_block.increment(1),
            ReputationChangeKind::BadTransactions => {
                self.reputation_changes_bad_transactions.increment(1)
            }
            ReputationChangeKind::BadAnnouncement => {
                self.reputation_changes_bad_announcement.increment(1)
            }
            ReputationChangeKind::AlreadySeenTransaction => {
                self.reputation_changes_already_seen_transaction.increment(1)
            }
            ReputationChangeKind::Timeout => self.reputation_changes_timeout.increment(1),
            ReputationChangeKind::BadProtocol => self.reputation_changes_bad_protocol.increment(1),
            ReputationChangeKind::FailedToConnect => {
                self.reputation_changes_failed_to_connect.increment(1)
            }
            ReputationChangeKind::Dropped => self.reputation_changes_dropped.increment(1),
            ReputationChangeKind::Reset => self.reputation_changes_reset.increment(1),
            ReputationChangeKind::Other(_) => self.reputation_changes_other.increment(1),
        }
    }
}

/// Metrics for the `EthRequestHandler`
#[derive(Metrics)]
#[metrics(scope = "network")]
pub struct EthRequestHandlerMetrics {
    /// Number of `GetBlockHeaders` requests received
    pub(crate) eth_headers_requests_received_total: Counter,

    /// Number of `GetReceipts` requests received
    pub(crate) eth_receipts_requests_received_total: Counter,

    /// Number of `GetBlockBodies` requests received
    pub(crate) eth_bodies_requests_received_total: Counter,

    /// Number of `GetNodeData` requests received
    pub(crate) eth_node_data_requests_received_total: Counter,

    /// Duration in seconds of call to poll
    /// [`EthRequestHandler`](crate::eth_requests::EthRequestHandler).
    pub(crate) acc_duration_poll_eth_req_handler: Gauge,
}

/// Eth67 announcement metrics, track entries by `TxType`
#[derive(Metrics)]
#[metrics(scope = "network.transaction_fetcher")]
pub struct AnnouncedTxTypesMetrics {
    /// Histogram for tracking frequency of legacy transaction type
    pub(crate) legacy: Histogram,

    /// Histogram for tracking frequency of EIP-2930 transaction type
    pub(crate) eip2930: Histogram,

    /// Histogram for tracking frequency of EIP-1559 transaction type
    pub(crate) eip1559: Histogram,

    /// Histogram for tracking frequency of EIP-4844 transaction type
    pub(crate) eip4844: Histogram,

    /// Histogram for tracking frequency of EIP-7702 transaction type
    pub(crate) eip7702: Histogram,
}

/// Counts the number of transactions by their type in a block or collection.
///
/// This struct keeps track of the count of different transaction types
/// as defined by various Ethereum Improvement Proposals (EIPs).
#[derive(Debug, Default)]
pub struct TxTypesCounter {
    /// Count of legacy transactions (pre-EIP-2718).
    pub(crate) legacy: usize,

    /// Count of transactions conforming to EIP-2930 (Optional access lists).
    pub(crate) eip2930: usize,

    /// Count of transactions conforming to EIP-1559 (Fee market change).
    pub(crate) eip1559: usize,

    /// Count of transactions conforming to EIP-4844 (Shard Blob Transactions).
    pub(crate) eip4844: usize,

    /// Count of transactions conforming to EIP-7702 (Restricted Storage Windows).
    pub(crate) eip7702: usize,
}

impl TxTypesCounter {
    pub(crate) const fn increase_by_tx_type(&mut self, tx_type: TxType) {
        match tx_type {
            TxType::Legacy => {
                self.legacy += 1;
            }
            TxType::Eip2930 => {
                self.eip2930 += 1;
            }
            TxType::Eip1559 => {
                self.eip1559 += 1;
            }
            TxType::Eip4844 => {
                self.eip4844 += 1;
            }
            TxType::Eip7702 => {
                self.eip7702 += 1;
            }
        }
    }
}

impl AnnouncedTxTypesMetrics {
    /// Update metrics during announcement validation, by examining each announcement entry based on
    /// `TxType`
    pub(crate) fn update_eth68_announcement_metrics(&self, tx_types_counter: TxTypesCounter) {
        self.legacy.record(tx_types_counter.legacy as f64);
        self.eip2930.record(tx_types_counter.eip2930 as f64);
        self.eip1559.record(tx_types_counter.eip1559 as f64);
        self.eip4844.record(tx_types_counter.eip4844 as f64);
        self.eip7702.record(tx_types_counter.eip7702 as f64);
    }
}
