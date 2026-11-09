//! Represents an established session.

use core::sync::atomic::Ordering;
use std::{
    collections::VecDeque,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize},
        Arc,
    },
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};

use crate::{
    message::{NewBlockMessage, PeerMessage, PeerResponse, PeerResponseResult},
    session::{
        conn::EthRlpxConnection,
        handle::{ActiveSessionMessage, QueuedSessionCommand, SessionCommand},
        BlockRangeInfo, EthVersion, SessionId,
    },
};
use alloy_eips::merge::{EPOCH_DURATION, EPOCH_SLOTS};
use alloy_primitives::Sealable;
use futures::{stream::Fuse, SinkExt, StreamExt};
use metrics::{Counter, Gauge};
use reth_eth_wire::{
    errors::{EthHandshakeError, EthStreamError},
    message::{EthBroadcastMessage, MessageError},
    Capabilities, DisconnectP2P, DisconnectReason, EthMessage, EthSnapMessage, NetworkPrimitives,
    NewBlockPayload,
};
use reth_eth_wire_types::{
    message::RequestPair, snap::SnapProtocolMessage, EthMessageID, NewPooledTransactionHashes,
    RawCapabilityMessage,
};
use reth_metrics::common::mpsc::MeteredPollSender;
use reth_network_api::{PeerRequest, RequestMessage};
use reth_network_p2p::{error::RequestError, snap::client::SnapResponse};
use reth_network_peers::PeerId;
use reth_network_types::session::config::INITIAL_REQUEST_TIMEOUT;
use reth_primitives_traits::Block;
use rustc_hash::FxHashMap;
use tokio::{
    sync::{mpsc, mpsc::error::TrySendError, oneshot},
    time::Interval,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;
use tracing::{debug, trace};

/// The recommended interval at which to check if a new range update should be sent to the remote
/// peer.
///
/// Updates are only sent when the block height has advanced by at least one epoch (32 blocks)
/// since the last update. The interval is set to one epoch duration in seconds.
pub(super) const RANGE_UPDATE_INTERVAL: Duration = EPOCH_DURATION;

// Constants for timeout updating.

/// Minimum timeout value
const MINIMUM_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum timeout value
const MAXIMUM_TIMEOUT: Duration = INITIAL_REQUEST_TIMEOUT;
/// How much the new measurements affect the current timeout (X percent)
const SAMPLE_IMPACT: f64 = 0.1;
/// Amount of RTTs before timeout
const TIMEOUT_SCALING: u32 = 3;

/// Restricts the number of queued outgoing messages for larger responses:
///  - Block Bodies
///  - Receipts
///  - Headers
///  - `PooledTransactions`
///
/// With proper softlimits in place (2MB) this targets 10MB (4+1 * 2MB) of outgoing response data.
///
/// This parameter serves as backpressure for reading additional requests from the remote.
/// Once we've queued up more responses than this, the session should prioritize message flushing
/// before reading any more messages from the remote peer, throttling the peer.
const MAX_QUEUED_OUTGOING_RESPONSES: usize = 4;

/// Capacity above which the drained outgoing message queue is shrunk back to its steady-state
/// size, see [`QueuedOutgoingMessages::shrink_to_fit`].
const SHRINK_CAPACITY_THRESHOLD: usize = 64;

/// Maximum number of messages read from the connection per session poll before the task yields
/// back to the scheduler, see the receive loop in the session's `Future` impl.
///
/// Message decoding is CPU intensive, so the budget bounds how long a single busy session can
/// occupy the executor thread. Small tx gossip messages dominate under load and are cheap to
/// decode individually, so the budget is sized such that their per-poll fixed costs (draining
/// command channels, advancing the sink, flushing the transport) amortize over a larger batch.
const RECEIVE_MESSAGE_BUDGET: usize = 16;

/// Transaction gossip and block announcements reserve independent end-to-end budgets.
const MAX_QUEUED_BROADCAST_ITEMS: usize = 4096;
pub(super) const MAX_QUEUED_BLOCKS: usize = 16;
pub(super) const MAX_QUEUED_BLOCK_HASHES: usize = 16;
/// Drain work is bounded even if another task continuously refills a command channel.
const SESSION_COMMAND_BUDGET: usize = 24;
/// After this many block messages, serve an ordinary queued message (including responses).
const BLOCK_SEND_BURST: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageKind {
    Block,
    BlockHashes,
    Transactions,
    TransactionHashes,
    Request,
    Response,
    Other,
}

impl MessageKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Block => "new_block",
            Self::BlockHashes => "new_block_hashes",
            Self::Transactions => "transactions",
            Self::TransactionHashes => "transaction_hashes",
            Self::Request => "request",
            Self::Response => "response",
            Self::Other => "other",
        }
    }

    pub(crate) const fn is_block(self) -> bool {
        matches!(self, Self::Block | Self::BlockHashes)
    }

    pub(crate) const fn of_peer<N: NetworkPrimitives>(msg: &PeerMessage<N>) -> Self {
        match msg {
            PeerMessage::NewBlock(_) => Self::Block,
            PeerMessage::NewBlockHashes(_) => Self::BlockHashes,
            PeerMessage::SendTransactions(_) | PeerMessage::SendBroadcastPoolTransactions(_) => {
                Self::Transactions
            }
            PeerMessage::PooledTransactions(_) => Self::TransactionHashes,
            PeerMessage::EthRequest(_) => Self::Request,
            _ => Self::Other,
        }
    }

    pub(crate) fn dropped(self, reason: &'static str) {
        metrics::counter!("network.outgoing_peer_messages_dropped_by_kind_total", "kind" => self.label(), "reason" => reason).increment(1);
    }
}

#[derive(Debug, Default)]
struct BroadcastCounts {
    transactions: AtomicUsize,
    blocks: AtomicUsize,
    hashes: AtomicUsize,
}

/// Shared quotas across command channels and the outgoing queue. Permits release automatically
/// on rejection, unsupported messages, session teardown, or handoff to the transport sink.
#[derive(Debug, Clone, Default)]
pub(crate) struct BroadcastItemCounter(Arc<BroadcastCounts>);

impl BroadcastItemCounter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self) -> usize {
        self.0.transactions.load(Ordering::Relaxed) +
            self.0.blocks.load(Ordering::Relaxed) +
            self.0.hashes.load(Ordering::Relaxed)
    }

    fn counter(&self, kind: MessageKind) -> (&AtomicUsize, usize) {
        match kind {
            MessageKind::Block => (&self.0.blocks, MAX_QUEUED_BLOCKS),
            MessageKind::BlockHashes => (&self.0.hashes, MAX_QUEUED_BLOCK_HASHES),
            _ => (&self.0.transactions, MAX_QUEUED_BROADCAST_ITEMS),
        }
    }

    pub(crate) fn try_acquire(&self, kind: MessageKind, items: usize) -> Option<BroadcastPermit> {
        // Empty batches still occupy a command slot and must not bypass the bounded quota.
        let items = items.max(1);
        let (counter, limit) = self.counter(kind);
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(items).filter(|next| *next <= limit)
            })
            .ok()?;
        Some(BroadcastPermit { counter: self.clone(), kind, items })
    }
}

#[derive(Debug)]
pub(crate) struct BroadcastPermit {
    counter: BroadcastItemCounter,
    kind: MessageKind,
    items: usize,
}

impl BroadcastPermit {
    fn merge(&mut self, mut other: Self) {
        debug_assert_eq!(self.kind, other.kind);
        debug_assert!(Arc::ptr_eq(&self.counter.0, &other.counter.0));
        self.items += other.items;
        other.items = 0;
    }
}

impl Drop for BroadcastPermit {
    fn drop(&mut self) {
        self.counter.counter(self.kind).0.fetch_sub(self.items, Ordering::Relaxed);
    }
}

/// Preserves the first admission timestamp through command overflow and hash coalescing.
#[derive(Debug)]
pub(crate) struct MessageMetadata {
    pub(crate) admitted: Instant,
    pub(crate) kind: MessageKind,
    pub(crate) permit: Option<BroadcastPermit>,
}

impl MessageMetadata {
    fn merge(&mut self, other: Self) {
        self.admitted = self.admitted.min(other.admitted);
        match (&mut self.permit, other.permit) {
            (Some(existing), Some(incoming)) => existing.merge(incoming),
            (slot @ None, incoming) => *slot = incoming,
            _ => {}
        }
    }
}

/// The type that advances an established session by listening for incoming messages (from local
/// node or read from connection) and emitting events back to the
/// [`SessionManager`](super::SessionManager).
///
/// It listens for
///    - incoming commands from the [`SessionManager`](super::SessionManager)
///    - incoming _internal_ requests/broadcasts via the request/command channel
///    - incoming requests/broadcasts _from remote_ via the connection
///    - responses for handled ETH requests received from the remote peer.
#[expect(dead_code)]
pub(crate) struct ActiveSession<N: NetworkPrimitives> {
    /// Keeps track of request ids.
    pub(crate) next_id: u64,
    /// The underlying connection.
    pub(crate) conn: EthRlpxConnection<N>,
    /// Identifier of the node we're connected to.
    pub(crate) remote_peer_id: PeerId,
    /// The address we're connected to.
    pub(crate) remote_addr: SocketAddr,
    /// All capabilities the peer announced
    pub(crate) remote_capabilities: Arc<Capabilities>,
    /// Internal identifier of this session
    pub(crate) session_id: SessionId,
    /// Incoming commands from the manager
    pub(crate) commands_rx: ReceiverStream<QueuedSessionCommand<N>>,
    /// Dedicated bounded block command lane, independent of transaction gossip.
    pub(crate) block_commands_rx: ReceiverStream<QueuedSessionCommand<N>>,
    /// Unbounded channel for commands that couldn't fit in the bounded channel (broadcast
    /// overflow) and for disconnect commands that must never be dropped.
    pub(crate) unbounded_rx: mpsc::UnboundedReceiver<QueuedSessionCommand<N>>,
    /// Counter for broadcast messages received via the unbounded overflow channel.
    pub(crate) unbounded_broadcast_msgs: Counter,
    /// Sink to send messages to the [`SessionManager`](super::SessionManager).
    pub(crate) to_session_manager: MeteredPollSender<ActiveSessionMessage<N>>,
    /// A message that needs to be delivered to the session manager
    pub(crate) pending_message_to_session: Option<ActiveSessionMessage<N>>,
    /// Incoming internal requests which are delegated to the remote peer.
    pub(crate) internal_request_rx: Fuse<ReceiverStream<PeerRequest<N>>>,
    /// All requests sent to the remote peer we're waiting on a response for, including `snap/2`
    /// requests ([`PeerRequest::GetSnap`]).
    pub(crate) inflight_requests: FxHashMap<u64, InflightRequest<PeerRequest<N>>>,
    /// All requests that were sent by the remote peer and we're waiting on an internal response
    pub(crate) received_requests_from_remote: Vec<ReceivedRequest<N>>,
    /// Buffered messages that should be handled and sent to the peer.
    pub(crate) queued_outgoing: QueuedOutgoingMessages<N>,
    /// The maximum time we wait for a response from a peer.
    pub(crate) internal_request_timeout: Arc<AtomicU64>,
    /// Interval when to check for timed out requests.
    pub(crate) internal_request_timeout_interval: Interval,
    /// If an [`ActiveSession`] does not receive a response at all within this duration then it is
    /// considered a protocol violation and the session will initiate a drop.
    pub(crate) protocol_breach_request_timeout: Duration,
    /// Used to reserve a slot to guarantee that the termination message is delivered
    pub(crate) terminate_message:
        Option<(PollSender<ActiveSessionMessage<N>>, ActiveSessionMessage<N>)>,
    /// The eth69 range info for the remote peer.
    /// This is `None` for peers negotiating versions below `eth/69`.
    pub(crate) range_info: Option<BlockRangeInfo>,
    /// The eth69 range info for the local node (this node).
    /// This represents the range of blocks that this node can serve to other peers.
    pub(crate) local_range_info: BlockRangeInfo,
    /// Optional interval for sending periodic range updates to the remote peer (eth69+)
    /// The interval is set to one epoch duration (~6.4 minutes), but updates are only sent when
    /// the block height has advanced by at least one epoch (32 blocks) since the last update
    pub(crate) range_update_interval: Option<Interval>,
    /// The last latest block number we sent in a range update
    /// Used to avoid sending unnecessary updates when block height hasn't changed significantly
    pub(crate) last_sent_latest_block: Option<u64>,
}

impl<N: NetworkPrimitives> ActiveSession<N> {
    /// Returns `true` if the session is currently in the process of disconnecting
    fn is_disconnecting(&self) -> bool {
        self.conn.inner().is_disconnecting()
    }

    /// Returns the next request id
    const fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Shrinks the capacity of the outgoing message queue once it is drained.
    ///
    /// The buffered incoming requests need no shrinking: the receive loop stops reading from the
    /// wire while more than [`MAX_QUEUED_OUTGOING_RESPONSES`] of them are pending, which keeps
    /// that buffer's capacity small.
    pub fn shrink_to_fit(&mut self) {
        self.queued_outgoing.shrink_to_fit();
    }

    /// Drains messages queued for sending into the connection's sink as long as the connection
    /// can accept more, without flushing the underlying transport.
    ///
    /// This always advances the sink at least once, even with nothing queued, so connection
    /// keepalive (ping) timers embedded in the sink's readiness logic are polled every session
    /// poll.
    ///
    /// Returns `true` if at least one message was handed to the connection.
    fn poll_send_queued(&mut self, cx: &mut Context<'_>) -> Result<bool, EthStreamError> {
        let mut progress = false;
        while self.conn.poll_ready_unpin(cx).is_ready() {
            let Some(QueuedOutgoingMessage { message, metadata }) =
                self.queued_outgoing.pop_front()
            else {
                break
            };
            progress = true;
            metrics::histogram!("network.outgoing_peer_queue_wait_seconds", "kind" => metadata.kind.label())
                .record(metadata.admitted.elapsed().as_secs_f64());
            let res = match message {
                OutgoingMessage::Snap(msg) => self.conn.start_send_snap(msg),
                OutgoingMessage::Eth(msg) => self.conn.start_send_unpin(msg),
                OutgoingMessage::Broadcast(msg) => self.conn.start_send_broadcast(msg),
                OutgoingMessage::Raw(msg) => self.conn.start_send_raw(msg),
            };
            // Release only after start_send accepted or rejected the message, never while it
            // remains in either command queue or the outgoing queue. This is not a TCP ACK.
            drop(metadata);
            res?;
        }
        Ok(progress)
    }

    /// Handle a message read from the connection.
    ///
    /// Returns an error if the message is considered to be in violation of the protocol.
    fn on_incoming_message(&mut self, msg: EthMessage<N>) -> OnIncomingMessageOutcome<N> {
        /// A macro that handles an incoming request
        /// This creates a new channel and tries to send the sender half to the session while
        /// storing the receiver half internally so the pending response can be polled.
        macro_rules! on_request {
            ($req:ident, $resp_item:ident, $req_item:ident) => {{
                let RequestPair { request_id, message: request } = $req;
                let (tx, response) = oneshot::channel();
                let received = ReceivedRequest {
                    request_id,
                    rx: PeerResponse::$resp_item { response },
                    received: Instant::now(),
                };
                self.received_requests_from_remote.push(received);
                self.try_emit_request(PeerMessage::EthRequest(PeerRequest::$req_item {
                    request,
                    response: tx,
                }))
                .into()
            }};
        }

        /// Processes a response received from the peer
        macro_rules! on_response {
            ($resp:ident, $item:ident) => {{
                let RequestPair { request_id, message } = $resp;
                if let Some(req) = self.inflight_requests.remove(&request_id) {
                    match req.request {
                        RequestState::Waiting(PeerRequest::$item { response, .. }) => {
                            trace!(peer_id=?self.remote_peer_id, ?request_id, "received response from peer");
                            let _ = response.send(Ok(message));
                            self.update_request_timeout(req.timestamp, Instant::now());
                        }
                        RequestState::Waiting(request) => {
                            // The peer replied to a request id we handed out, but with the wrong
                            // response type. This cancels the pending request, so it must cost the
                            // peer reputation. Without the penalty the peer can kill any request we
                            // send it, repeatedly and for free.
                            debug!(target: "net::session", ?request_id, remote_peer_id=?self.remote_peer_id, "received response of wrong type");
                            self.on_bad_message();
                            request.send_bad_response();
                        }
                        RequestState::TimedOut => {
                            // request was already timed out internally
                            self.update_request_timeout(req.timestamp, Instant::now());
                        }
                    }
                } else {
                    trace!(peer_id=?self.remote_peer_id, ?request_id, "received response to unknown request");
                    // we received a response to a request we never sent
                    self.on_bad_message();
                }

                OnIncomingMessageOutcome::Ok
            }};
        }

        match msg {
            message @ EthMessage::Status(_) => OnIncomingMessageOutcome::BadMessage {
                error: EthStreamError::EthHandshakeError(EthHandshakeError::StatusNotInHandshake),
                message,
            },
            EthMessage::NewBlockHashes(msg) => {
                self.try_emit_broadcast(PeerMessage::NewBlockHashes(msg)).into()
            }
            EthMessage::NewBlock(msg) => {
                let block = NewBlockMessage {
                    hash: msg.block().header().hash_slow(),
                    block: Arc::new(*msg),
                };
                self.try_emit_broadcast(PeerMessage::NewBlock(block)).into()
            }
            EthMessage::Transactions(msg) => {
                self.try_emit_broadcast(PeerMessage::ReceivedTransaction(msg)).into()
            }
            EthMessage::NewPooledTransactionHashes66(msg) => {
                self.try_emit_broadcast(PeerMessage::PooledTransactions(msg.into())).into()
            }
            EthMessage::NewPooledTransactionHashes68(msg) => {
                self.try_emit_broadcast(PeerMessage::PooledTransactions(msg.into())).into()
            }
            EthMessage::NewPooledTransactionHashes72(msg) => {
                self.try_emit_broadcast(PeerMessage::PooledTransactions(msg.into())).into()
            }
            EthMessage::GetBlockHeaders(req) => {
                on_request!(req, BlockHeaders, GetBlockHeaders)
            }
            EthMessage::BlockHeaders(resp) => {
                on_response!(resp, GetBlockHeaders)
            }
            EthMessage::GetBlockBodies(req) => {
                on_request!(req, BlockBodies, GetBlockBodies)
            }
            EthMessage::BlockBodies(resp) => {
                on_response!(resp, GetBlockBodies)
            }
            EthMessage::GetPooledTransactions(req) => {
                on_request!(req, PooledTransactions, GetPooledTransactions)
            }
            EthMessage::PooledTransactions(resp) => {
                on_response!(resp, GetPooledTransactions)
            }
            EthMessage::GetNodeData(req) => {
                on_request!(req, NodeData, GetNodeData)
            }
            EthMessage::NodeData(resp) => {
                on_response!(resp, GetNodeData)
            }
            EthMessage::GetReceipts(req) => {
                if self.conn.version() >= EthVersion::Eth69 {
                    on_request!(req, Receipts69, GetReceipts69)
                } else {
                    on_request!(req, Receipts, GetReceipts)
                }
            }
            EthMessage::GetReceipts70(req) => {
                on_request!(req, Receipts70, GetReceipts70)
            }
            EthMessage::Receipts(resp) => {
                on_response!(resp, GetReceipts)
            }
            EthMessage::Receipts69(resp) => {
                on_response!(resp, GetReceipts69)
            }
            EthMessage::Receipts70(resp) => {
                on_response!(resp, GetReceipts70)
            }
            EthMessage::GetBlockAccessLists(req) => {
                on_request!(req, BlockAccessLists, GetBlockAccessLists)
            }
            EthMessage::BlockAccessLists(resp) => {
                on_response!(resp, GetBlockAccessLists)
            }
            EthMessage::Cells(resp) => {
                on_response!(resp, GetCells)
            }
            EthMessage::BlockRangeUpdate(msg) => {
                // Validate that earliest <= latest according to the spec
                if msg.earliest > msg.latest {
                    return OnIncomingMessageOutcome::BadMessage {
                        error: EthStreamError::InvalidMessage(MessageError::Other(format!(
                            "invalid block range: earliest ({}) > latest ({})",
                            msg.earliest, msg.latest
                        ))),
                        message: EthMessage::BlockRangeUpdate(msg),
                    };
                }

                // Validate that the latest hash is not zero
                if msg.latest_hash.is_zero() {
                    return OnIncomingMessageOutcome::BadMessage {
                        error: EthStreamError::InvalidMessage(MessageError::Other(
                            "invalid block range: latest_hash cannot be zero".to_string(),
                        )),
                        message: EthMessage::BlockRangeUpdate(msg),
                    };
                }

                if let Some(range_info) = self.range_info.as_ref() {
                    range_info.update(msg.earliest, msg.latest, msg.latest_hash);
                }

                OnIncomingMessageOutcome::Ok
            }
            EthMessage::GetCells(resp) => {
                on_request!(resp, Cells, GetCells)
            }
            EthMessage::Other(bytes) => self.try_emit_broadcast(PeerMessage::Other(bytes)).into(),
        }
    }

    /// Handles an inbound `snap/2` message.
    ///
    /// Responses are correlated to the in-flight [`PeerRequest::GetSnap`] by `request_id` (shared
    /// with eth requests in [`Self::inflight_requests`]) and type-checked against the originally
    /// sent request kind; unsolicited or mismatched ones count as bad messages. Inbound requests
    /// are routed upward as [`PeerRequest::GetSnap`], same as any other eth request.
    fn on_incoming_snap_message(
        &mut self,
        mut msg: SnapProtocolMessage,
    ) -> OnIncomingMessageOutcome<N> {
        let request_id = msg.request_id();
        if !msg.is_response() {
            let (tx, response) = oneshot::channel();
            self.received_requests_from_remote.push(ReceivedRequest {
                request_id,
                rx: PeerResponse::Snap { response },
                received: Instant::now(),
            });
            return self
                .try_emit_request(PeerMessage::EthRequest(PeerRequest::GetSnap {
                    request: msg,
                    response: tx,
                }))
                .into()
        }

        let Some(req) = self.inflight_requests.remove(&request_id) else {
            trace!(target: "net::session", ?request_id, remote_peer_id=?self.remote_peer_id, "received snap response to unknown request");
            self.on_bad_message();
            return OnIncomingMessageOutcome::Ok
        };

        match req.request {
            RequestState::Waiting(PeerRequest::GetSnap { request, response }) => {
                if Some(msg.message_id()) != request.message_id().response() {
                    debug!(target: "net::session", ?request_id, msg_id=?msg.message_id(), remote_peer_id=?self.remote_peer_id, "received snap response of wrong type");
                    self.on_bad_message();
                    let _ = response.send(Err(RequestError::BadResponse));
                    return OnIncomingMessageOutcome::Ok
                }
                // Restore the caller's original request id, not the wire-assigned one.
                msg.set_request_id(request.request_id());
                match SnapResponse::try_from(msg) {
                    Ok(snap_response) => {
                        trace!(target: "net::session", ?request_id, remote_peer_id=?self.remote_peer_id, "received snap response from peer");
                        let _ = response.send(Ok(snap_response));
                        self.update_request_timeout(req.timestamp, Instant::now());
                    }
                    Err(_) => {
                        let _ = response.send(Err(RequestError::BadResponse));
                    }
                }
            }
            RequestState::Waiting(request) => {
                // A different PeerRequest kind was pending for this id: the peer answered an `eth`
                // request with a `snap` message. Same as the wrong-type case above, this cancels
                // the pending request and must be penalized.
                debug!(target: "net::session", ?request_id, msg_id=?msg.message_id(), remote_peer_id=?self.remote_peer_id, "received snap response for non-snap request");
                self.on_bad_message();
                request.send_bad_response();
            }
            RequestState::TimedOut => {
                self.update_request_timeout(req.timestamp, Instant::now());
            }
        }
        OnIncomingMessageOutcome::Ok
    }

    /// Handle an internal peer request that will be sent to the remote.
    fn on_internal_peer_request(&mut self, request: PeerRequest<N>, deadline: Instant) {
        self.on_internal_peer_request_at(request, deadline, Instant::now());
    }

    fn on_internal_peer_request_at(
        &mut self,
        request: PeerRequest<N>,
        deadline: Instant,
        admitted: Instant,
    ) {
        let version = self.conn.version();
        if !Self::is_request_supported_for_version(&request, version) {
            debug!(
                target: "net",
                ?request,
                peer_id=?self.remote_peer_id,
                ?version,
                "Request not supported for negotiated eth version",
            );
            request.send_err_response(RequestError::UnsupportedCapability);
            return;
        }

        // `GetSnap` isn't covered by the eth-version check above, and a connection that never
        // negotiated `snap/2` can't send one without erroring the whole session.
        if matches!(request, PeerRequest::GetSnap { .. }) && !self.conn.supports_snap() {
            request.send_err_response(RequestError::UnsupportedCapability);
            return;
        }

        let request_id = self.next_id();
        trace!(?request, peer_id=?self.remote_peer_id, ?request_id, "sending request to peer");
        let msg = match request.create_request_message(request_id) {
            RequestMessage::Eth(msg) => msg.map_versioned(version).into(),
            RequestMessage::Snap(msg) => OutgoingMessage::Snap(msg),
        };

        self.queued_outgoing.push_back_with_metadata(
            msg,
            MessageMetadata { admitted, kind: MessageKind::Request, permit: None },
        );
        let req = InflightRequest {
            request: RequestState::Waiting(request),
            timestamp: Instant::now(),
            deadline,
        };
        self.inflight_requests.insert(request_id, req);
    }

    #[inline]
    fn is_request_supported_for_version(request: &PeerRequest<N>, version: EthVersion) -> bool {
        request.is_supported_by_eth_version(version)
    }

    /// Handle a message received from the internal network
    fn on_internal_peer_message(&mut self, msg: PeerMessage<N>, metadata: MessageMetadata) {
        match msg {
            PeerMessage::NewBlockHashes(msg) => {
                self.queued_outgoing
                    .push_back_with_metadata(EthMessage::NewBlockHashes(msg).into(), metadata);
            }
            PeerMessage::NewBlock(msg) => {
                self.queued_outgoing.push_back_with_metadata(
                    EthBroadcastMessage::NewBlock(msg.block).into(),
                    metadata,
                );
            }
            PeerMessage::PooledTransactions(msg) => {
                if msg.is_valid_for_version(self.conn.version()) {
                    self.queued_outgoing.push_pooled_hashes(msg, metadata);
                } else {
                    metadata.kind.dropped("unsupported");
                    debug!(target: "net", ?msg,  version=?self.conn.version(), "Message is invalid for connection version, skipping");
                }
            }
            PeerMessage::EthRequest(req) => {
                let deadline = self.request_deadline();
                self.on_internal_peer_request_at(req, deadline, metadata.admitted);
            }
            PeerMessage::SendTransactions(msg) => {
                self.queued_outgoing.push_back_with_metadata(
                    EthBroadcastMessage::Transactions(msg).into(),
                    metadata,
                );
            }
            PeerMessage::SendBroadcastPoolTransactions(msg) => {
                self.queued_outgoing.push_back_with_metadata(
                    EthBroadcastMessage::BroadcastPoolTransactions(msg).into(),
                    metadata,
                );
            }
            PeerMessage::BlockRangeUpdated(_) => {}
            PeerMessage::ReceivedTransaction(_) => {
                unreachable!("Not emitted by network")
            }
            PeerMessage::Other(other) => {
                self.queued_outgoing.push_back_with_metadata(OutgoingMessage::Raw(other), metadata);
            }
        }
    }

    fn on_queued_command(
        &mut self,
        queued: QueuedSessionCommand<N>,
        cx: &mut Context<'_>,
    ) -> Option<Poll<()>> {
        match queued.command {
            SessionCommand::Message(msg) => {
                self.on_internal_peer_message(msg, queued.metadata);
                None
            }
            SessionCommand::Disconnect { reason } => Some(
                self.try_disconnect(reason.unwrap_or(DisconnectReason::DisconnectRequested), cx),
            ),
        }
    }

    /// Returns the deadline timestamp at which the request times out
    fn request_deadline(&self) -> Instant {
        Instant::now() +
            Duration::from_millis(self.internal_request_timeout.load(Ordering::Relaxed))
    }

    /// Handle a Response to the peer
    ///
    /// This will queue the response to be sent to the peer
    fn handle_outgoing_response(&mut self, id: u64, resp: PeerResponseResult<N>) {
        match resp.try_into_message(id) {
            Ok(RequestMessage::Eth(msg)) => {
                self.queued_outgoing.push_back(msg.into());
            }
            Ok(RequestMessage::Snap(msg)) => {
                self.queued_outgoing.push_back(OutgoingMessage::Snap(msg));
            }
            Err(err) => {
                debug!(target: "net", %err, "Failed to respond to received request");
            }
        }
    }

    /// Send a message back to the [`SessionManager`](super::SessionManager).
    ///
    /// Returns the message if the bounded channel is currently unable to handle this message.
    #[expect(clippy::result_large_err)]
    fn try_emit_broadcast(&self, message: PeerMessage<N>) -> Result<(), ActiveSessionMessage<N>> {
        let Some(sender) = self.to_session_manager.inner().get_ref() else { return Ok(()) };

        match sender
            .try_send(ActiveSessionMessage::ValidMessage { peer_id: self.remote_peer_id, message })
        {
            Ok(_) => Ok(()),
            Err(err) => {
                trace!(
                    target: "net",
                    %err,
                    "no capacity for incoming broadcast",
                );
                match err {
                    TrySendError::Full(msg) => Err(msg),
                    TrySendError::Closed(_) => Ok(()),
                }
            }
        }
    }

    /// Send a message back to the [`SessionManager`](super::SessionManager)
    /// covering both broadcasts and incoming requests.
    ///
    /// Returns the message if the bounded channel is currently unable to handle this message.
    #[expect(clippy::result_large_err)]
    fn try_emit_request(&self, message: PeerMessage<N>) -> Result<(), ActiveSessionMessage<N>> {
        let Some(sender) = self.to_session_manager.inner().get_ref() else { return Ok(()) };

        match sender
            .try_send(ActiveSessionMessage::ValidMessage { peer_id: self.remote_peer_id, message })
        {
            Ok(_) => Ok(()),
            Err(err) => {
                trace!(
                    target: "net",
                    %err,
                    "no capacity for incoming request",
                );
                match err {
                    TrySendError::Full(msg) => Err(msg),
                    TrySendError::Closed(_) => {
                        // Note: this would mean the `SessionManager` was dropped, which is already
                        // handled by checking if the command receiver channel has been closed.
                        Ok(())
                    }
                }
            }
        }
    }

    /// Notify the manager that the peer sent a bad message
    fn on_bad_message(&self) {
        let Some(sender) = self.to_session_manager.inner().get_ref() else { return };
        let _ = sender.try_send(ActiveSessionMessage::BadMessage { peer_id: self.remote_peer_id });
    }

    /// Report back that this session has been closed.
    fn emit_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        trace!(target: "net::session", remote_peer_id=?self.remote_peer_id, "emitting disconnect");
        let msg = ActiveSessionMessage::Disconnected {
            peer_id: self.remote_peer_id,
            remote_addr: self.remote_addr,
        };

        self.terminate_message = Some((self.to_session_manager.inner().clone(), msg));
        self.poll_terminate_message(cx).expect("message is set")
    }

    /// Report back that this session has been closed due to an error
    fn close_on_error(&mut self, error: EthStreamError, cx: &mut Context<'_>) -> Poll<()> {
        let msg = ActiveSessionMessage::ClosedOnConnectionError {
            peer_id: self.remote_peer_id,
            remote_addr: self.remote_addr,
            error,
        };
        self.terminate_message = Some((self.to_session_manager.inner().clone(), msg));
        self.poll_terminate_message(cx).expect("message is set")
    }

    /// Starts the disconnect process
    fn start_disconnect(&mut self, reason: DisconnectReason) -> Result<(), EthStreamError> {
        Ok(self.conn.inner_mut().start_disconnect(reason)?)
    }

    /// Flushes the disconnect message and emits the corresponding message
    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        debug_assert!(self.is_disconnecting(), "not disconnecting");

        // try to close the flush out the remaining Disconnect message
        let _ = ready!(self.conn.poll_close_unpin(cx));
        self.emit_disconnect(cx)
    }

    /// Attempts to disconnect by sending the given disconnect reason
    fn try_disconnect(&mut self, reason: DisconnectReason, cx: &mut Context<'_>) -> Poll<()> {
        match self.start_disconnect(reason) {
            Ok(()) => {
                // we're done
                self.poll_disconnect(cx)
            }
            Err(err) => {
                debug!(target: "net::session", %err, remote_peer_id=?self.remote_peer_id, "could not send disconnect");
                self.close_on_error(err, cx)
            }
        }
    }

    /// Checks for _internally_ timed out requests.
    ///
    /// If a requests misses its deadline, then it is timed out internally.
    /// If a request misses the `protocol_breach_request_timeout` then this session is considered in
    /// protocol violation and will close.
    ///
    /// Returns `true` if a peer missed the `protocol_breach_request_timeout`, in which case the
    /// session should be terminated.
    #[must_use]
    fn check_timed_out_requests(&mut self, now: Instant) -> bool {
        for (id, req) in &mut self.inflight_requests {
            if req.is_timed_out(now) {
                if req.is_waiting() {
                    debug!(target: "net::session", ?id, remote_peer_id=?self.remote_peer_id, "timed out outgoing request");
                    req.timeout();
                } else if now - req.timestamp > self.protocol_breach_request_timeout {
                    return true
                }
            }
        }

        false
    }

    /// Updates the request timeout with a request's timestamps
    fn update_request_timeout(&mut self, sent: Instant, received: Instant) {
        let elapsed = received.saturating_duration_since(sent);

        let current = Duration::from_millis(self.internal_request_timeout.load(Ordering::Relaxed));
        let request_timeout = calculate_new_timeout(current, elapsed);
        self.internal_request_timeout.store(request_timeout.as_millis() as u64, Ordering::Relaxed);
        self.internal_request_timeout_interval = request_timeout_interval(request_timeout);
    }

    /// If a termination message is queued this will try to send it
    fn poll_terminate_message(&mut self, cx: &mut Context<'_>) -> Option<Poll<()>> {
        let (mut tx, msg) = self.terminate_message.take()?;
        match tx.poll_reserve(cx) {
            Poll::Pending => {
                self.terminate_message = Some((tx, msg));
                return Some(Poll::Pending)
            }
            Poll::Ready(Ok(())) => {
                let _ = tx.send_item(msg);
            }
            Poll::Ready(Err(_)) => {
                // channel closed
            }
        }
        // terminate the task
        Some(Poll::Ready(()))
    }
}

impl<N: NetworkPrimitives> Future for ActiveSession<N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // if the session is terminate we have to send the termination message before we can close
        if let Some(terminate) = this.poll_terminate_message(cx) {
            return terminate
        }

        if this.is_disconnecting() {
            return this.poll_disconnect(cx)
        }

        // The receive loop can be CPU intensive since it involves message decoding which could take
        // up a lot of resources and increase latencies for other sessions if not yielded manually.
        // If the budget is exhausted we manually yield back control to the (coop) scheduler. This
        // manual yield point should prevent situations where polling appears to be frozen. See also <https://tokio.rs/blog/2020-04-preemption>
        // And tokio's docs on cooperative scheduling <https://docs.rs/tokio/latest/tokio/task/#cooperative-scheduling>
        let mut budget = RECEIVE_MESSAGE_BUDGET;
        let mut command_budgets = [SESSION_COMMAND_BUDGET; 3];

        // The main poll loop that drives the session
        'main: loop {
            let mut progress = false;
            let mut receive_pending = false;

            // A separate block ingress lane is visible even when ordinary command traffic is
            // saturated. Bound work for this entire poll, not merely each outer-loop pass.
            while command_budgets[0] > 0 {
                let Poll::Ready(Some(cmd)) = this.block_commands_rx.poll_next_unpin(cx) else {
                    break
                };
                command_budgets[0] -= 1;
                progress = true;
                if let Some(done) = this.on_queued_command(cmd, cx) {
                    return done;
                }
            }
            while command_budgets[1] > 0 {
                match this.commands_rx.poll_next_unpin(cx) {
                    Poll::Pending => break,
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Ready(Some(cmd)) => {
                        command_budgets[1] -= 1;
                        progress = true;
                        if let Some(done) = this.on_queued_command(cmd, cx) {
                            return done;
                        }
                    }
                }
            }
            while command_budgets[2] > 0 {
                let Poll::Ready(Some(cmd)) = this.unbounded_rx.poll_recv(cx) else { break };
                command_budgets[2] -= 1;
                progress = true;
                if matches!(cmd.command, SessionCommand::Message(_)) {
                    this.unbounded_broadcast_msgs.increment(1);
                }
                if let Some(done) = this.on_queued_command(cmd, cx) {
                    return done;
                }
            }

            let deadline = this.request_deadline();

            while let Poll::Ready(Some(req)) = this.internal_request_rx.poll_next_unpin(cx) {
                progress = true;
                this.on_internal_peer_request(req, deadline);
            }

            // Advance all active requests.
            // We remove each request one by one and add them back.
            for idx in (0..this.received_requests_from_remote.len()).rev() {
                let mut req = this.received_requests_from_remote.swap_remove(idx);
                match req.rx.poll(cx) {
                    Poll::Pending => {
                        // not ready yet
                        this.received_requests_from_remote.push(req);
                    }
                    Poll::Ready(resp) => {
                        this.handle_outgoing_response(req.request_id, resp);
                    }
                }
            }

            // Send messages by advancing the sink and queuing in buffered messages. The sink only
            // buffers sent messages; the explicit flush happens once per poll after the main
            // loop, so messages queued across the loop's passes batch up (the sink still writes
            // out on its own for control messages and when its write buffer runs full).
            match this.poll_send_queued(cx) {
                Ok(sent) => progress |= sent,
                Err(err) => {
                    debug!(target: "net::session", %err, remote_peer_id=?this.remote_peer_id, "failed to send message");
                    // notify the manager
                    return this.close_on_error(err, cx)
                }
            }

            // read incoming messages from the wire
            'receive: loop {
                // ensure we still have enough budget for another iteration
                budget -= 1;
                if budget == 0 {
                    // make sure we're woken up again
                    cx.waker().wake_by_ref();
                    break 'main
                }

                // try to resend the pending message that we could not send because the channel was
                // full. [`PollSender`] will ensure that we're woken up again when the channel is
                // ready to receive the message, and will only error if the channel is closed.
                if let Some(msg) = this.pending_message_to_session.take() {
                    match this.to_session_manager.poll_reserve(cx) {
                        Poll::Ready(Ok(_)) => {
                            let _ = this.to_session_manager.send_item(msg);
                        }
                        Poll::Ready(Err(_)) => return Poll::Ready(()),
                        Poll::Pending => {
                            this.pending_message_to_session = Some(msg);
                            break 'receive
                        }
                    };
                }

                // check whether we should throttle incoming messages
                if this.received_requests_from_remote.len() > MAX_QUEUED_OUTGOING_RESPONSES {
                    // we're currently waiting for the responses to the peer's requests which aren't
                    // queued as outgoing yet
                    //
                    // Note: we don't need to register the waker here because we polled the requests
                    // above
                    break 'receive
                }

                // we also need to check if we have multiple responses queued up
                if this.queued_outgoing.response_count() > MAX_QUEUED_OUTGOING_RESPONSES {
                    // if we've queued up more responses than allowed, we don't poll for new
                    // messages and break the receive loop early
                    //
                    // Note: we don't need to register the waker here because we still have
                    // queued messages and the sink impl registered the waker because we've
                    // already advanced it to `Pending` earlier
                    break 'receive
                }

                match this.conn.poll_next_unpin(cx) {
                    Poll::Pending => {
                        receive_pending = true;
                        break
                    }
                    Poll::Ready(None) => {
                        if this.is_disconnecting() {
                            break
                        }
                        debug!(target: "net::session", remote_peer_id=?this.remote_peer_id, "eth stream completed");
                        return this.emit_disconnect(cx)
                    }
                    Poll::Ready(Some(res)) => {
                        match res {
                            Ok(msg) => {
                                let outcome = match msg {
                                    EthSnapMessage::Eth(msg) => {
                                        trace!(target: "net::session", msg_id=?msg.message_id(), remote_peer_id=?this.remote_peer_id, "received eth message");
                                        // decode and handle message
                                        this.on_incoming_message(msg)
                                    }
                                    EthSnapMessage::Snap(msg) => this.on_incoming_snap_message(msg),
                                };
                                match outcome {
                                    OnIncomingMessageOutcome::Ok => {
                                        // handled successfully
                                        progress = true;
                                    }
                                    OnIncomingMessageOutcome::BadMessage { error, message } => {
                                        debug!(target: "net::session", %error, msg=?message, remote_peer_id=?this.remote_peer_id, "received invalid protocol message");
                                        this.on_bad_message();
                                        return this
                                            .try_disconnect(DisconnectReason::ProtocolBreach, cx)
                                    }
                                    OnIncomingMessageOutcome::NoCapacity(msg) => {
                                        // failed to send due to lack of capacity
                                        this.pending_message_to_session = Some(msg);
                                    }
                                }
                            }
                            Err(err) => {
                                debug!(target: "net::session", %err, remote_peer_id=?this.remote_peer_id, "failed to receive message");
                                if err.is_protocol_breach() {
                                    this.on_bad_message();
                                    return this.try_disconnect(DisconnectReason::ProtocolBreach, cx)
                                }
                                return this.close_on_error(err, cx)
                            }
                        }
                    }
                }
            }

            // Each ingress lane gets its own finite share, including overflow/control. Wake
            // before the idle-wire fast exit: buffered commands may not register another waker.
            if command_budgets.contains(&0) {
                cx.waker().wake_by_ref();
                break 'main
            }

            // Avoid one extra empty outer-loop pass after the wire is pending, unless the receive
            // pass produced work that should be driven immediately.
            if receive_pending &&
                this.queued_outgoing.is_empty() &&
                this.pending_message_to_session.is_none() &&
                this.received_requests_from_remote.is_empty()
            {
                break 'main
            }

            if !progress {
                break 'main
            }
        }

        if let Some(interval) = &mut this.range_update_interval {
            // Check if we should send a range update based on block height changes
            while interval.poll_tick(cx).is_ready() {
                let current_latest = this.local_range_info.latest();
                let should_send = if let Some(last_sent) = this.last_sent_latest_block {
                    // Only send if block height has advanced by at least one epoch (32 blocks)
                    current_latest.saturating_sub(last_sent) >= EPOCH_SLOTS
                } else {
                    true // First update, always send
                };

                if should_send {
                    this.queued_outgoing.push_back(
                        EthMessage::BlockRangeUpdate(this.local_range_info.to_message()).into(),
                    );
                    this.last_sent_latest_block = Some(current_latest);
                }
            }
        }

        if !this.inflight_requests.is_empty() {
            while this.internal_request_timeout_interval.poll_tick(cx).is_ready() {
                // check for timed out requests
                if this.check_timed_out_requests(Instant::now()) &&
                    let Poll::Ready(Ok(_)) = this.to_session_manager.poll_reserve(cx)
                {
                    let msg = ActiveSessionMessage::ProtocolBreach { peer_id: this.remote_peer_id };
                    this.pending_message_to_session = Some(msg);
                }
            }
        }

        // Send anything the interval handlers above queued, then flush the transport for
        // everything buffered during this poll. This also resumes a flush that returned pending
        // on an earlier poll; a no-op if nothing is buffered.
        if let Err(err) = this.poll_send_queued(cx) {
            debug!(target: "net::session", %err, remote_peer_id=?this.remote_peer_id, "failed to send message");
            return this.close_on_error(err, cx)
        }
        match this.conn.poll_flush_unpin(cx) {
            Poll::Pending | Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(err)) => {
                debug!(target: "net::session", %err, remote_peer_id=?this.remote_peer_id, "failed to flush connection");
                return this.close_on_error(err, cx)
            }
        }

        this.shrink_to_fit();

        Poll::Pending
    }
}

/// Tracks a request received from the peer
pub(crate) struct ReceivedRequest<N: NetworkPrimitives> {
    /// Protocol Identifier
    request_id: u64,
    /// Receiver half of the channel that's supposed to receive the proper response.
    rx: PeerResponse<N>,
    /// Timestamp when we read this msg from the wire.
    #[expect(dead_code)]
    received: Instant,
}

/// A request that waits for a response from the peer
pub(crate) struct InflightRequest<R> {
    /// Request we sent to peer and the internal response channel
    request: RequestState<R>,
    /// Instant when the request was sent
    timestamp: Instant,
    /// Time limit for the response
    deadline: Instant,
}

impl<N: NetworkPrimitives> InflightRequest<PeerRequest<N>> {
    /// Returns true if the request is timedout
    #[inline]
    fn is_timed_out(&self, now: Instant) -> bool {
        now > self.deadline
    }

    /// Returns true if we're still waiting for a response
    #[inline]
    const fn is_waiting(&self) -> bool {
        matches!(self.request, RequestState::Waiting(_))
    }

    /// This will timeout the request by sending an error response to the internal channel
    fn timeout(&mut self) {
        let mut req = RequestState::TimedOut;
        std::mem::swap(&mut self.request, &mut req);

        if let RequestState::Waiting(req) = req {
            req.send_err_response(RequestError::Timeout);
        }
    }
}

/// All outcome variants when handling an incoming message
enum OnIncomingMessageOutcome<N: NetworkPrimitives> {
    /// Message successfully handled.
    Ok,
    /// Message is considered to be in violation of the protocol
    BadMessage { error: EthStreamError, message: EthMessage<N> },
    /// Currently no capacity to handle the message
    NoCapacity(ActiveSessionMessage<N>),
}

impl<N: NetworkPrimitives> From<Result<(), ActiveSessionMessage<N>>>
    for OnIncomingMessageOutcome<N>
{
    fn from(res: Result<(), ActiveSessionMessage<N>>) -> Self {
        match res {
            Ok(_) => Self::Ok,
            Err(msg) => Self::NoCapacity(msg),
        }
    }
}

enum RequestState<R> {
    /// Waiting for the response
    Waiting(R),
    /// Request already timed out
    TimedOut,
}

/// Outgoing messages that can be sent over the wire.
#[derive(Debug)]
pub(crate) enum OutgoingMessage<N: NetworkPrimitives> {
    /// A message that is owned.
    Eth(EthMessage<N>),
    /// A message that may be shared by multiple sessions.
    Broadcast(EthBroadcastMessage<N>),
    /// A raw capability message
    Raw(RawCapabilityMessage),
    /// A `snap/2` message to send over the dedicated `eth`+`snap` stream.
    Snap(SnapProtocolMessage),
}

impl<N: NetworkPrimitives> OutgoingMessage<N> {
    fn kind(&self) -> MessageKind {
        match self {
            Self::Broadcast(EthBroadcastMessage::NewBlock(_)) => MessageKind::Block,
            Self::Broadcast(_) => MessageKind::Transactions,
            Self::Eth(message) if message.is_response() => MessageKind::Response,
            Self::Eth(message) => match message.message_id() {
                EthMessageID::NewBlock => MessageKind::Block,
                EthMessageID::NewBlockHashes => MessageKind::BlockHashes,
                EthMessageID::Transactions => MessageKind::Transactions,
                EthMessageID::NewPooledTransactionHashes => MessageKind::TransactionHashes,
                EthMessageID::GetBlockHeaders |
                EthMessageID::GetBlockBodies |
                EthMessageID::GetPooledTransactions |
                EthMessageID::GetNodeData |
                EthMessageID::GetReceipts |
                EthMessageID::GetBlockAccessLists |
                EthMessageID::GetCells => MessageKind::Request,
                _ => MessageKind::Other,
            },
            Self::Snap(message) if message.is_response() => MessageKind::Response,
            Self::Snap(_) => MessageKind::Request,
            Self::Raw(_) => MessageKind::Other,
        }
    }

    /// Returns true if this is a response.
    const fn is_response(&self) -> bool {
        match self {
            Self::Eth(msg) => msg.is_response(),
            // Served snap responses count toward response backpressure; outbound snap requests do
            // not. `SnapProtocolMessage::is_response` distinguishes the two.
            Self::Snap(msg) => msg.is_response(),
            Self::Broadcast(_) | Self::Raw(_) => false,
        }
    }

    /// Tries to merge pooled transaction hash announcements into this message, consuming the
    /// incoming hashes. Returns `Some(incoming)` back if the variants don't match.
    fn try_merge_hashes(
        &mut self,
        incoming: NewPooledTransactionHashes,
    ) -> Option<NewPooledTransactionHashes> {
        let Self::Eth(eth) = self else { return Some(incoming) };
        match (eth, incoming) {
            (
                EthMessage::NewPooledTransactionHashes66(existing),
                NewPooledTransactionHashes::Eth66(inc),
            ) => {
                existing.extend(inc);
                None
            }
            (
                EthMessage::NewPooledTransactionHashes68(existing),
                NewPooledTransactionHashes::Eth68(inc),
            ) => {
                existing.hashes.extend(inc.hashes);
                existing.sizes.extend(inc.sizes);
                existing.types.extend(inc.types);
                None
            }
            (
                EthMessage::NewPooledTransactionHashes72(existing),
                NewPooledTransactionHashes::Eth72(inc),
            ) => {
                // One mask describes every blob transaction in the message.
                if existing.cell_mask != inc.cell_mask {
                    return Some(inc.into())
                }
                existing.hashes.extend(inc.hashes);
                existing.sizes.extend(inc.sizes);
                existing.types.extend(inc.types);
                None
            }
            (_, incoming) => Some(incoming),
        }
    }
}

impl<N: NetworkPrimitives> From<EthMessage<N>> for OutgoingMessage<N> {
    fn from(value: EthMessage<N>) -> Self {
        Self::Eth(value)
    }
}

impl<N: NetworkPrimitives> From<EthBroadcastMessage<N>> for OutgoingMessage<N> {
    fn from(value: EthBroadcastMessage<N>) -> Self {
        Self::Broadcast(value)
    }
}

/// Returns the interval used to check for timed out requests.
///
/// Uses delayed missed-tick behavior because the interval is only polled while requests are in
/// flight, so ticks that elapsed while the session was idle must not fire in a burst.
pub(super) fn request_timeout_interval(timeout: Duration) -> Interval {
    let mut interval = tokio::time::interval(timeout);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Calculates a new timeout using an updated estimation of the RTT
#[inline]
fn calculate_new_timeout(current_timeout: Duration, estimated_rtt: Duration) -> Duration {
    let new_timeout = estimated_rtt.mul_f64(SAMPLE_IMPACT) * TIMEOUT_SCALING;

    // this dampens sudden changes by taking a weighted mean of the old and new values
    let smoothened_timeout = current_timeout.mul_f64(1.0 - SAMPLE_IMPACT) + new_timeout;

    smoothened_timeout.clamp(MINIMUM_TIMEOUT, MAXIMUM_TIMEOUT)
}

/// A message and its admission lifetime, retained until transport handoff or cancellation.
#[derive(Debug)]
pub(crate) struct QueuedOutgoingMessage<N: NetworkPrimitives> {
    message: OutgoingMessage<N>,
    metadata: MessageMetadata,
}

/// Block announcements have a separate priority lane. After a bounded block burst, the normal
/// FIFO advances, including request/response and transaction traffic. Already serialized RLPx
/// data cannot be preempted by this queue.
pub(crate) struct QueuedOutgoingMessages<N: NetworkPrimitives> {
    messages: VecDeque<QueuedOutgoingMessage<N>>,
    blocks: VecDeque<QueuedOutgoingMessage<N>>,
    block_burst: usize,
    queued_responses: usize,
    count: Gauge,
}

impl<N: NetworkPrimitives> QueuedOutgoingMessages<N> {
    pub(crate) const fn new(metric: Gauge) -> Self {
        Self {
            messages: VecDeque::new(),
            blocks: VecDeque::new(),
            block_burst: 0,
            queued_responses: 0,
            count: metric,
        }
    }

    pub(crate) const fn response_count(&self) -> usize {
        self.queued_responses
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.blocks.is_empty()
    }

    pub(crate) fn push_back(&mut self, message: OutgoingMessage<N>) {
        let kind = message.kind();
        self.push_back_with_metadata(
            message,
            MessageMetadata { admitted: Instant::now(), kind, permit: None },
        );
    }

    fn push_back_with_metadata(&mut self, message: OutgoingMessage<N>, metadata: MessageMetadata) {
        self.queued_responses += message.is_response() as usize;
        let block = metadata.kind.is_block();
        let queue = if block { &mut self.blocks } else { &mut self.messages };
        queue.push_back(QueuedOutgoingMessage { message, metadata });
        self.count.increment(1);
    }

    pub(crate) fn pop_front(&mut self) -> Option<QueuedOutgoingMessage<N>> {
        let next = if !self.blocks.is_empty() &&
            (self.block_burst < BLOCK_SEND_BURST || self.messages.is_empty())
        {
            self.block_burst = (self.block_burst + 1).min(BLOCK_SEND_BURST);
            self.blocks.pop_front()
        } else {
            self.block_burst = 0;
            self.messages.pop_front()
        };
        next.inspect(|msg| {
            self.count.decrement(1);
            self.queued_responses -= msg.message.is_response() as usize;
        })
    }

    /// Coalesced hashes retain the oldest admission timestamp and every reserved item.
    fn push_pooled_hashes(&mut self, msg: NewPooledTransactionHashes, metadata: MessageMetadata) {
        let msg = if let Some(last) = self.messages.back_mut() {
            match last.message.try_merge_hashes(msg) {
                None => {
                    last.metadata.merge(metadata);
                    return;
                }
                Some(msg) => msg,
            }
        } else {
            msg
        };
        self.push_back_with_metadata(EthMessage::from(msg).into(), metadata);
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        if self.messages.is_empty() && self.messages.capacity() > SHRINK_CAPACITY_THRESHOLD {
            self.messages.shrink_to(MAX_QUEUED_OUTGOING_RESPONSES);
        }
    }
}

impl<N: NetworkPrimitives> Drop for QueuedOutgoingMessages<N> {
    fn drop(&mut self) {
        self.count.decrement((self.messages.len() + self.blocks.len()) as f64);
        // Message metadata drops its permits after this; channels do likewise on teardown.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        handle::{PendingSessionEvent, SessionCommandSender},
        start_pending_incoming_session,
    };
    use alloy_eips::eip2124::ForkFilter;
    use alloy_primitives::B256;
    use futures::task::noop_waker;
    use reth_chainspec::MAINNET;
    use reth_ecies::stream::ECIESStream;
    use reth_eth_wire::{
        handshake::EthHandshake, protocol::Protocol, BlockBodies, BlockHeaders,
        EthNetworkPrimitives, EthStream, GetBlockAccessLists, GetBlockBodies,
        HelloMessageWithProtocols, P2PStream, StatusBuilder, UnauthedEthStream, UnauthedP2PStream,
        UnifiedStatus,
    };
    use reth_eth_wire_types::{
        message::MAX_MESSAGE_SIZE,
        snap::{
            AccountRangeMessage, BlockAccessListsMessage, GetAccountRangeMessage,
            GetBlockAccessListsMessage,
        },
        BlockAccessLists, NewPooledTransactionHashes72,
    };
    use reth_ethereum_forks::EthereumHardfork;
    use reth_network_p2p::error::RequestResult;
    use reth_network_peers::pk2id;
    use reth_network_types::session::config::PROTOCOL_BREACH_REQUEST_TIMEOUT;
    use secp256k1::{SecretKey, SECP256K1};
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::mpsc,
    };

    /// Returns a testing `HelloMessage` and new secretkey
    fn eth_hello(server_key: &SecretKey) -> HelloMessageWithProtocols {
        HelloMessageWithProtocols::builder(pk2id(&server_key.public_key(SECP256K1))).build()
    }

    struct SessionBuilder<N: NetworkPrimitives = EthNetworkPrimitives> {
        _remote_capabilities: Arc<Capabilities>,
        active_session_tx: mpsc::Sender<ActiveSessionMessage<N>>,
        active_session_rx: ReceiverStream<ActiveSessionMessage<N>>,
        to_sessions: Vec<mpsc::Sender<QueuedSessionCommand<N>>>,
        secret_key: SecretKey,
        local_peer_id: PeerId,
        hello: HelloMessageWithProtocols,
        status: UnifiedStatus,
        fork_filter: ForkFilter,
        next_id: usize,
    }

    impl<N: NetworkPrimitives> SessionBuilder<N> {
        fn next_id(&mut self) -> SessionId {
            let id = self.next_id;
            self.next_id += 1;
            SessionId(id)
        }

        /// Connects a new Eth stream and executes the given closure with that established stream
        fn with_client_stream<F, O>(
            &self,
            local_addr: SocketAddr,
            f: F,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>>
        where
            F: FnOnce(EthStream<P2PStream<ECIESStream<TcpStream>>, N>) -> O + Send + 'static,
            O: Future<Output = ()> + Send + Sync,
        {
            let mut status = self.status;
            let fork_filter = self.fork_filter.clone();
            let local_peer_id = self.local_peer_id;
            let mut hello = self.hello.clone();
            let key = SecretKey::new(&mut rand_08::thread_rng());
            hello.id = pk2id(&key.public_key(SECP256K1));
            Box::pin(async move {
                let outgoing = TcpStream::connect(local_addr).await.unwrap();
                let sink = ECIESStream::connect(outgoing, key, local_peer_id).await.unwrap();

                let (p2p_stream, _) = UnauthedP2PStream::new(sink).handshake(hello).await.unwrap();

                let eth_version = p2p_stream.shared_capabilities().eth_version().unwrap();
                status.set_eth_version(eth_version);

                let (client_stream, _) = UnauthedEthStream::new(p2p_stream)
                    .handshake(status, fork_filter)
                    .await
                    .unwrap();
                f(client_stream).await
            })
        }

        async fn connect_incoming(&mut self, stream: TcpStream) -> ActiveSession<N> {
            let remote_addr = stream.local_addr().unwrap();
            let session_id = self.next_id();
            let (_disconnect_tx, disconnect_rx) = oneshot::channel();
            let (pending_sessions_tx, pending_sessions_rx) = mpsc::channel(1);

            tokio::task::spawn(start_pending_incoming_session(
                Arc::new(EthHandshake::default()),
                MAX_MESSAGE_SIZE,
                disconnect_rx,
                session_id,
                stream,
                pending_sessions_tx,
                remote_addr,
                self.secret_key,
                self.hello.clone(),
                self.status,
                self.fork_filter.clone(),
                Default::default(),
            ));

            let mut stream = ReceiverStream::new(pending_sessions_rx);

            match stream.next().await.unwrap() {
                PendingSessionEvent::Established {
                    session_id,
                    remote_addr,
                    peer_id,
                    capabilities,
                    conn,
                    ..
                } => {
                    let (_to_session_tx, messages_rx) = mpsc::channel(10);
                    let (commands_to_session, commands_rx) = mpsc::channel(10);
                    let (_block_tx, block_commands_rx) =
                        mpsc::channel(MAX_QUEUED_BLOCKS + MAX_QUEUED_BLOCK_HASHES);
                    let (_unbounded_tx, unbounded_rx) = mpsc::unbounded_channel();
                    let poll_sender = PollSender::new(self.active_session_tx.clone());

                    self.to_sessions.push(commands_to_session);

                    ActiveSession {
                        next_id: 0,
                        remote_peer_id: peer_id,
                        remote_addr,
                        remote_capabilities: Arc::clone(&capabilities),
                        session_id,
                        commands_rx: ReceiverStream::new(commands_rx),
                        block_commands_rx: ReceiverStream::new(block_commands_rx),
                        unbounded_rx,
                        unbounded_broadcast_msgs: Counter::noop(),
                        to_session_manager: MeteredPollSender::new(
                            poll_sender,
                            "network_active_session",
                        ),
                        pending_message_to_session: None,
                        internal_request_rx: ReceiverStream::new(messages_rx).fuse(),
                        inflight_requests: Default::default(),
                        conn,
                        queued_outgoing: QueuedOutgoingMessages::new(Gauge::noop()),
                        received_requests_from_remote: Default::default(),
                        internal_request_timeout_interval: request_timeout_interval(
                            INITIAL_REQUEST_TIMEOUT,
                        ),
                        internal_request_timeout: Arc::new(AtomicU64::new(
                            INITIAL_REQUEST_TIMEOUT.as_millis() as u64,
                        )),
                        protocol_breach_request_timeout: PROTOCOL_BREACH_REQUEST_TIMEOUT,
                        terminate_message: None,
                        range_info: None,
                        local_range_info: BlockRangeInfo::new(
                            0,
                            1000,
                            alloy_primitives::B256::ZERO,
                        ),
                        range_update_interval: None,
                        last_sent_latest_block: None,
                    }
                }
                ev => {
                    panic!("unexpected message {ev:?}")
                }
            }
        }
    }

    impl Default for SessionBuilder {
        fn default() -> Self {
            let (active_session_tx, active_session_rx) = mpsc::channel(100);

            let (secret_key, pk) = SECP256K1.generate_keypair(&mut rand_08::thread_rng());
            let local_peer_id = pk2id(&pk);

            Self {
                next_id: 0,
                _remote_capabilities: Arc::new(Capabilities::from(vec![])),
                active_session_tx,
                active_session_rx: ReceiverStream::new(active_session_rx),
                to_sessions: vec![],
                hello: eth_hello(&secret_key),
                secret_key,
                local_peer_id,
                status: StatusBuilder::default().build(),
                fork_filter: MAINNET
                    .hardfork_fork_filter(EthereumHardfork::Frontier)
                    .expect("The Frontier fork filter should exist on mainnet"),
            }
        }
    }

    /// Returns a [`SessionBuilder`] whose hello also advertises `snap/2`, so the negotiated
    /// session ends up on an [`EthSnapStream`](reth_eth_wire::EthSnapStream) connection instead
    /// of a plain `eth`-only one.
    fn snap_session_builder() -> SessionBuilder {
        let mut builder = SessionBuilder::default();
        builder.hello.try_add_protocol(Protocol::snap_2()).unwrap();
        builder
    }

    /// Dispatches a `snap/2` request via [`ActiveSession::on_internal_peer_request`] and returns
    /// the session-assigned request id plus the caller's response receiver.
    fn dispatch_snap_request(
        session: &mut ActiveSession<EthNetworkPrimitives>,
        caller_request_id: u64,
    ) -> (u64, oneshot::Receiver<RequestResult<SnapResponse>>) {
        let (response, rx) = oneshot::channel();
        let request = SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
            request_id: caller_request_id,
            block_hashes: Vec::new(),
            response_bytes: 0,
        });
        let deadline = session.request_deadline();
        session.on_internal_peer_request(PeerRequest::GetSnap { request, response }, deadline);
        let id = *session.inflight_requests.keys().next().expect("snap request tracked");
        (id, rx)
    }

    /// Dispatches an `eth` `GetBlockBodies` request via
    /// [`ActiveSession::on_internal_peer_request`] and returns the session-assigned request id plus
    /// the caller's response receiver.
    fn dispatch_block_bodies_request(
        session: &mut ActiveSession<EthNetworkPrimitives>,
    ) -> (u64, oneshot::Receiver<RequestResult<BlockBodies>>) {
        let (response, rx) = oneshot::channel();
        let deadline = session.request_deadline();
        session.on_internal_peer_request(
            PeerRequest::GetBlockBodies { request: GetBlockBodies(Vec::new()), response },
            deadline,
        );
        let id = *session.inflight_requests.keys().next().expect("eth request tracked");
        (id, rx)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_disconnect() {
        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let expected_disconnect = DisconnectReason::UselessPeer;

        let fut = builder.with_client_stream(local_addr, async move |mut client_stream| {
            let msg = client_stream.next().await.unwrap().unwrap_err();
            assert_eq!(msg.as_disconnected().unwrap(), expected_disconnect);
        });

        tokio::task::spawn(async move {
            let (incoming, _) = listener.accept().await.unwrap();
            let mut session = builder.connect_incoming(incoming).await;

            session.start_disconnect(expected_disconnect).unwrap();
            session.await
        });

        fut.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_invalid_message_disconnects_with_protocol_breach() {
        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let fut = builder.with_client_stream(local_addr, async move |mut client_stream| {
            client_stream
                .start_send_raw(RawCapabilityMessage::eth(
                    EthMessageID::PooledTransactions,
                    vec![0xc0].into(),
                ))
                .unwrap();
            client_stream.flush().await.unwrap();

            let msg = client_stream.next().await.unwrap().unwrap_err();
            assert_eq!(msg.as_disconnected(), Some(DisconnectReason::ProtocolBreach));
        });

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn(async move {
            let (incoming, _) = listener.accept().await.unwrap();
            let session = builder.connect_incoming(incoming).await;
            session.await;

            tx.send(()).unwrap();
        });

        fut.await;
        rx.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_dropped_stream() {
        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            drop(client_stream);
            tokio::time::sleep(Duration::from_secs(1)).await
        });

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn(async move {
            let (incoming, _) = listener.accept().await.unwrap();
            let session = builder.connect_incoming(incoming).await;
            session.await;

            tx.send(()).unwrap();
        });

        tokio::task::spawn(fut);

        rx.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_many_messages() {
        reth_tracing::init_test_tracing();
        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let num_messages = 100;

        let fut = builder.with_client_stream(local_addr, async move |mut client_stream| {
            for _ in 0..num_messages {
                client_stream
                    .send(EthMessage::NewPooledTransactionHashes66(Vec::new().into()))
                    .await
                    .unwrap();
            }
        });

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn(async move {
            let (incoming, _) = listener.accept().await.unwrap();
            let session = builder.connect_incoming(incoming).await;
            session.await;

            tx.send(()).unwrap();
        });

        tokio::task::spawn(fut);

        rx.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_request_timeout() {
        reth_tracing::init_test_tracing();

        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let request_timeout = Duration::from_millis(100);
        let drop_timeout = Duration::from_millis(1500);

        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(drop_timeout * 60).await;
        });
        tokio::task::spawn(fut);

        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;
        session
            .internal_request_timeout
            .store(request_timeout.as_millis() as u64, Ordering::Relaxed);
        session.protocol_breach_request_timeout = drop_timeout;
        session.internal_request_timeout_interval =
            tokio::time::interval_at(tokio::time::Instant::now(), request_timeout);
        session
            .internal_request_timeout_interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let (tx, rx) = oneshot::channel();
        let req = PeerRequest::GetBlockBodies { request: GetBlockBodies(vec![]), response: tx };
        session.on_internal_peer_request(req, Instant::now());
        tokio::spawn(session);

        let err = rx.await.unwrap().unwrap_err();
        assert_eq!(err, RequestError::Timeout);

        // wait for protocol breach error
        let msg = builder.active_session_rx.next().await.unwrap();
        match msg {
            ActiveSessionMessage::ProtocolBreach { .. } => {}
            ev => unreachable!("{ev:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snap_request_is_assigned_unique_id_and_response_correlated() {
        let mut builder = snap_session_builder();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        // The session assigns its own request id (not the caller's sentinel) and tracks it.
        let (id, rx) = dispatch_snap_request(&mut session, u64::MAX);
        assert_ne!(id, u64::MAX, "session must assign its own request id");

        // A response carrying that id is correlated back to the caller's future.
        let outcome = session.on_incoming_snap_message(SnapProtocolMessage::BlockAccessLists(
            BlockAccessListsMessage {
                request_id: id,
                block_access_lists: BlockAccessLists(Vec::new()),
            },
        ));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(!session.inflight_requests.contains_key(&id));

        // The delivered response carries the caller's original id again, not the session's.
        let response = rx.await.unwrap().unwrap();
        assert!(matches!(
            response,
            SnapResponse::BlockAccessLists(m) if m.request_id == u64::MAX
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_type_snap_response_is_rejected() {
        let mut builder = snap_session_builder();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        let (id, rx) = dispatch_snap_request(&mut session, 0);

        // Answering a GetBlockAccessLists with an AccountRange under the same id is a bad message.
        let outcome = session.on_incoming_snap_message(SnapProtocolMessage::AccountRange(
            AccountRangeMessage { request_id: id, accounts: Vec::new(), proof: Vec::new() },
        ));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(!session.inflight_requests.contains_key(&id));
        assert_eq!(rx.await.unwrap().unwrap_err(), RequestError::BadResponse);
        assert!(matches!(
            builder.active_session_rx.next().await,
            Some(ActiveSessionMessage::BadMessage { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn late_snap_response_is_consumed_without_penalty() {
        let mut builder = snap_session_builder();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        session.internal_request_timeout.store(1, Ordering::Relaxed);
        let (id, rx) = dispatch_snap_request(&mut session, 0);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!session.check_timed_out_requests(Instant::now()));
        assert_eq!(rx.await.unwrap().unwrap_err(), RequestError::Timeout);

        // A response arriving after the timeout clears the entry without a bad-message report.
        let outcome = session.on_incoming_snap_message(SnapProtocolMessage::BlockAccessLists(
            BlockAccessListsMessage {
                request_id: id,
                block_access_lists: BlockAccessLists(Vec::new()),
            },
        ));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(!session.inflight_requests.contains_key(&id));
        assert!(futures::FutureExt::now_or_never(builder.active_session_rx.next())
            .flatten()
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_snap_response_is_penalized() {
        let mut builder = snap_session_builder();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        // A response for a request we never sent is dropped and reported as a bad message.
        let outcome = session.on_incoming_snap_message(SnapProtocolMessage::BlockAccessLists(
            BlockAccessListsMessage {
                request_id: 999,
                block_access_lists: BlockAccessLists(Vec::new()),
            },
        ));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(session.inflight_requests.is_empty());
        assert!(session.queued_outgoing.pop_front().is_none());
        assert!(matches!(
            builder.active_session_rx.next().await,
            Some(ActiveSessionMessage::BadMessage { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_type_eth_response_is_penalized() {
        let mut builder = SessionBuilder::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        let (id, rx) = dispatch_block_bodies_request(&mut session);

        // The peer knows this id because we sent it. Answering the GetBlockBodies with BlockHeaders
        // cancels our request, so it must cost the peer reputation, otherwise the peer can kill
        // every request we make for free.
        let outcome = session.on_incoming_message(EthMessage::BlockHeaders(RequestPair {
            request_id: id,
            message: BlockHeaders(Vec::new()),
        }));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(!session.inflight_requests.contains_key(&id));
        assert_eq!(rx.await.unwrap().unwrap_err(), RequestError::BadResponse);
        assert!(matches!(
            futures::FutureExt::now_or_never(builder.active_session_rx.next()).flatten(),
            Some(ActiveSessionMessage::BadMessage { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_eth_response_is_not_penalized() {
        let mut builder = SessionBuilder::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        let (id, rx) = dispatch_block_bodies_request(&mut session);

        let outcome = session.on_incoming_message(EthMessage::BlockBodies(RequestPair {
            request_id: id,
            message: BlockBodies(Vec::new()),
        }));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(!session.inflight_requests.contains_key(&id));
        assert!(rx.await.unwrap().is_ok());
        assert!(futures::FutureExt::now_or_never(builder.active_session_rx.next())
            .flatten()
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snap_response_to_eth_request_is_penalized() {
        let mut builder = snap_session_builder();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        // `eth` and `snap/2` requests share one id space, so a snap response can land on a pending
        // eth request. It cancels that request just the same and is penalized the same way.
        let (id, rx) = dispatch_block_bodies_request(&mut session);

        let outcome = session.on_incoming_snap_message(SnapProtocolMessage::AccountRange(
            AccountRangeMessage { request_id: id, accounts: Vec::new(), proof: Vec::new() },
        ));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert!(!session.inflight_requests.contains_key(&id));
        assert_eq!(rx.await.unwrap().unwrap_err(), RequestError::BadResponse);
        assert!(matches!(
            futures::FutureExt::now_or_never(builder.active_session_rx.next()).flatten(),
            Some(ActiveSessionMessage::BadMessage { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_snap_request_rejected_without_negotiated_snap() {
        // A plain `eth`-only session: no `snap/2` was negotiated.
        let mut builder = SessionBuilder::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;
        assert!(!session.conn.supports_snap());

        let (response, rx) = oneshot::channel();
        let request = SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
            request_id: 0,
            block_hashes: Vec::new(),
            response_bytes: 0,
        });
        let deadline = session.request_deadline();
        session.on_internal_peer_request(PeerRequest::GetSnap { request, response }, deadline);

        // Rejected immediately instead of being queued for a connection that can't send it.
        assert!(session.inflight_requests.is_empty());
        assert!(session.queued_outgoing.pop_front().is_none());
        assert_eq!(rx.await.unwrap().unwrap_err(), RequestError::UnsupportedCapability);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_snap_request_round_trips_to_a_response() {
        let mut builder = snap_session_builder();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let fut = builder.with_client_stream(local_addr, async move |client_stream| {
            let _client_stream = client_stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::task::spawn(fut);
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;

        // The peer sends an inbound GetAccountRange request.
        let outcome = session.on_incoming_snap_message(SnapProtocolMessage::GetAccountRange(
            GetAccountRangeMessage {
                request_id: 7,
                root_hash: B256::ZERO,
                starting_hash: B256::ZERO,
                limit_hash: B256::ZERO,
                response_bytes: 1024,
            },
        ));
        assert!(matches!(outcome, OnIncomingMessageOutcome::Ok));
        assert_eq!(session.received_requests_from_remote.len(), 1);

        // It's routed upward instead of being served inline.
        let Some(ActiveSessionMessage::ValidMessage {
            message: PeerMessage::EthRequest(PeerRequest::GetSnap { request, response }),
            ..
        }) = builder.active_session_rx.next().await
        else {
            panic!("expected an outbound GetSnap request")
        };
        assert!(matches!(request, SnapProtocolMessage::GetAccountRange(_)));

        // The handler answers with an empty-but-valid range.
        let _ = response.send(Ok(SnapResponse::AccountRange(AccountRangeMessage {
            request_id: 7,
            accounts: Vec::new(),
            proof: Vec::new(),
        })));

        // Drive the same conversion the session's main poll loop would.
        let mut req = session.received_requests_from_remote.pop().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let Poll::Ready(resp) = req.rx.poll(&mut cx) else { panic!("response should be ready") };
        session.handle_outgoing_response(req.request_id, resp);

        // The reply goes out as a snap/2 message carrying the original request id, not an eth
        // message.
        let msg = session.queued_outgoing.pop_front().expect("response queued for send").message;
        assert!(matches!(
            msg,
            OutgoingMessage::Snap(SnapProtocolMessage::AccountRange(AccountRangeMessage {
                request_id: 7,
                ..
            }))
        ));
    }

    #[test]
    fn eth72_coalescing_preserves_cell_masks() {
        use alloy_primitives::B128;

        let masks =
            [None, Some(B128::repeat_byte(0xff)), Some(B128::from(1u128)), Some(B128::from(2u128))];
        for existing_mask in masks {
            for incoming_mask in masks {
                let announcement = |mask: Option<B128>, byte| NewPooledTransactionHashes72 {
                    types: vec![if mask.is_some() { 3 } else { 2 }],
                    sizes: vec![100],
                    hashes: vec![B256::repeat_byte(byte)],
                    cell_mask: mask,
                };
                let existing = announcement(existing_mask, 1);
                let incoming = announcement(incoming_mask, 2);
                let mut message: OutgoingMessage<EthNetworkPrimitives> =
                    EthMessage::NewPooledTransactionHashes72(existing.clone()).into();

                let remainder = message.try_merge_hashes(incoming.clone().into());

                let OutgoingMessage::Eth(EthMessage::NewPooledTransactionHashes72(merged)) =
                    message
                else {
                    panic!("expected eth72 announcement");
                };
                if existing_mask == incoming_mask {
                    assert!(remainder.is_none());
                    assert_eq!(merged.cell_mask, existing_mask);
                    assert_eq!(merged.hashes, vec![B256::repeat_byte(1), B256::repeat_byte(2)]);
                    assert_eq!(merged.types, [existing.types, incoming.types].concat());
                    assert_eq!(merged.sizes, vec![100, 100]);
                } else {
                    assert_eq!(remainder, Some(incoming.into()));
                    assert_eq!(merged, existing);
                }
            }
        }
    }

    #[test]
    fn eth72_pooled_hashes_count_broadcast_items() {
        let hashes =
            vec![alloy_primitives::B256::repeat_byte(1), alloy_primitives::B256::repeat_byte(2)];
        let msg = PeerMessage::<EthNetworkPrimitives>::PooledTransactions(
            NewPooledTransactionHashes72 {
                types: vec![0; hashes.len()],
                sizes: vec![1; hashes.len()],
                hashes,
                cell_mask: None,
            }
            .into(),
        );
        assert_eq!(2, msg.message_item_count());
        let counter = BroadcastItemCounter::new();
        let permit =
            counter.try_acquire(MessageKind::of_peer(&msg), msg.message_item_count()).unwrap();
        assert_eq!(counter.get(), 2);
        drop(permit);
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn test_reject_bal_request_for_eth70() {
        let (tx, _rx) = oneshot::channel();
        let request: PeerRequest<EthNetworkPrimitives> =
            PeerRequest::GetBlockAccessLists { request: GetBlockAccessLists(vec![]), response: tx };

        assert!(!ActiveSession::<EthNetworkPrimitives>::is_request_supported_for_version(
            &request,
            EthVersion::Eth70
        ));
        assert!(ActiveSession::<EthNetworkPrimitives>::is_request_supported_for_version(
            &request,
            EthVersion::Eth71
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_keep_alive() {
        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let fut = builder.with_client_stream(local_addr, async move |mut client_stream| {
            let _ = tokio::time::timeout(Duration::from_secs(1), client_stream.next()).await;
            client_stream.into_inner().disconnect(DisconnectReason::UselessPeer).await.unwrap();
        });

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn(async move {
            let (incoming, _) = listener.accept().await.unwrap();
            let session = builder.connect_incoming(incoming).await;
            session.await;

            tx.send(()).unwrap();
        });

        tokio::task::spawn(fut);

        rx.await.unwrap();
    }

    // This tests that incoming messages are delivered when there's capacity.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_at_capacity() {
        let mut builder = SessionBuilder::default();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        let fut = builder.with_client_stream(local_addr, async move |mut client_stream| {
            client_stream
                .send(EthMessage::NewPooledTransactionHashes68(Default::default()))
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(100), client_stream.next()).await;
        });
        tokio::task::spawn(fut);

        let (incoming, _) = listener.accept().await.unwrap();
        let session = builder.connect_incoming(incoming).await;

        // fill the entire message buffer with an unrelated message
        let mut num_fill_messages = 0;
        loop {
            if builder
                .active_session_tx
                .try_send(ActiveSessionMessage::ProtocolBreach { peer_id: PeerId::random() })
                .is_err()
            {
                break
            }
            num_fill_messages += 1;
        }

        tokio::task::spawn(async move {
            session.await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        for _ in 0..num_fill_messages {
            let message = builder.active_session_rx.next().await.unwrap();
            match message {
                ActiveSessionMessage::ProtocolBreach { .. } => {}
                ev => unreachable!("{ev:?}"),
            }
        }

        let message = builder.active_session_rx.next().await.unwrap();
        match message {
            ActiveSessionMessage::ValidMessage {
                message: PeerMessage::PooledTransactions(_),
                ..
            } => {}
            _ => unreachable!(),
        }
    }

    #[test]
    fn timeout_calculation_sanity_tests() {
        let rtt = Duration::from_secs(5);
        // timeout for an RTT of `rtt`
        let timeout = rtt * TIMEOUT_SCALING;

        // if rtt hasn't changed, timeout shouldn't change
        assert_eq!(calculate_new_timeout(timeout, rtt), timeout);

        // if rtt changed, the new timeout should change less than it
        assert!(calculate_new_timeout(timeout, rtt / 2) < timeout);
        assert!(calculate_new_timeout(timeout, rtt / 2) > timeout / 2);
        assert!(calculate_new_timeout(timeout, rtt * 2) > timeout);
        assert!(calculate_new_timeout(timeout, rtt * 2) < timeout * 2);
    }

    #[test]
    fn coalesced_hashes_hold_permits_and_original_admission_time() {
        let counter = BroadcastItemCounter::new();
        let mut queue = QueuedOutgoingMessages::<EthNetworkPrimitives>::new(Gauge::noop());
        let admitted = Instant::now() - Duration::from_secs(2);
        for (items, at) in [(2, admitted), (3, Instant::now())] {
            queue.push_pooled_hashes(
                reth_eth_wire::NewPooledTransactionHashes66(vec![B256::ZERO; items]).into(),
                MessageMetadata {
                    admitted: at,
                    kind: MessageKind::TransactionHashes,
                    permit: counter.try_acquire(MessageKind::TransactionHashes, items),
                },
            );
        }
        assert_eq!(counter.get(), 5);
        let outgoing = queue.pop_front().unwrap();
        assert_eq!(outgoing.metadata.admitted, admitted);
        assert_eq!(counter.get(), 5, "dequeue is not transport handoff");
        assert!(queue.is_empty());
        drop(outgoing);
        assert_eq!(counter.get(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn actual_session_prioritizes_blocks_and_advances_requests_responses_and_transactions() {
        let mut builder = SessionBuilder::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = oneshot::channel();
        let client =
            tokio::spawn(builder.with_client_stream(local_addr, async move |mut stream| {
                let mut ids = Vec::new();
                for _ in 0..16 {
                    ids.push(stream.next().await.unwrap().unwrap().message_id());
                }
                seen_tx.send(ids).unwrap();
            }));
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;
        let (tx, rx) = mpsc::channel(16);
        let (block_tx, block_rx) = mpsc::channel(32);
        let (overflow_tx, overflow_rx) = mpsc::unbounded_channel();
        let counter = BroadcastItemCounter::new();
        let sender = SessionCommandSender::new(tx, block_tx, overflow_tx, counter.clone());
        session.commands_rx = ReceiverStream::new(rx);
        session.block_commands_rx = ReceiverStream::new(block_rx);
        session.unbounded_rx = overflow_rx;
        for _ in 0..2 {
            assert!(sender.send_message(PeerMessage::SendTransactions(
                reth_eth_wire::SharedTransactions(Vec::new())
            )));
        }
        let (response, _response_rx) = oneshot::channel();
        assert!(sender.send_message(PeerMessage::EthRequest(PeerRequest::GetBlockBodies {
            request: GetBlockBodies(Vec::new()),
            response
        })));
        for _ in 0..12 {
            assert!(sender.send_message(PeerMessage::NewBlockHashes(Default::default())));
        }
        let (response_tx, response_rx) = oneshot::channel();
        response_tx.send(Ok(BlockHeaders(Vec::new()))).unwrap();
        session.received_requests_from_remote.push(ReceivedRequest {
            request_id: 42,
            rx: PeerResponse::BlockHeaders { response: response_rx },
            received: Instant::now(),
        });
        assert!(Pin::new(&mut session).poll(&mut Context::from_waker(&noop_waker())).is_pending());
        let ids = tokio::time::timeout(Duration::from_secs(5), seen_rx).await.unwrap().unwrap();
        assert_eq!(&ids[..4], &[EthMessageID::NewBlockHashes; 4]);
        assert_eq!(ids[4], EthMessageID::Transactions);
        assert_eq!(ids[9], EthMessageID::Transactions);
        assert_eq!(ids[14], EthMessageID::GetBlockBodies);
        assert_eq!(ids[15], EthMessageID::BlockHeaders);
        assert_eq!(counter.get(), 0, "all reservations end at transport handoff");
        client.await.unwrap();
    }

    struct WakeCounter(AtomicUsize);
    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saturated_bounded_ingress_cannot_starve_overflow_or_disconnect_and_self_wakes() {
        let mut builder = SessionBuilder::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let client =
            tokio::spawn(builder.with_client_stream(local_addr, async move |mut stream| {
                while let Some(Ok(_)) = stream.next().await {}
            }));
        let (incoming, _) = listener.accept().await.unwrap();
        let mut session = builder.connect_incoming(incoming).await;
        let (tx, rx) = mpsc::channel(128);
        let (block_tx, block_rx) = mpsc::channel(32);
        let (overflow_tx, overflow_rx) = mpsc::unbounded_channel();
        let counter = BroadcastItemCounter::new();
        let sender =
            SessionCommandSender::new(tx.clone(), block_tx, overflow_tx.clone(), counter.clone());
        session.commands_rx = ReceiverStream::new(rx);
        session.block_commands_rx = ReceiverStream::new(block_rx);
        session.unbounded_rx = overflow_rx;
        let wakes = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        for _ in 0..3 {
            while tx.capacity() > 0 {
                assert!(sender.send_message(PeerMessage::SendTransactions(
                    reth_eth_wire::SharedTransactions(Vec::new())
                )));
            }
            assert!(sender.send_message(PeerMessage::PooledTransactions(
                reth_eth_wire::NewPooledTransactionHashes66(vec![B256::ZERO]).into()
            )));
            assert!(!session.unbounded_rx.is_empty());
            wakes.0.store(0, Ordering::Relaxed);
            assert!(Pin::new(&mut session).poll(&mut cx).is_pending());
            assert!(
                session.unbounded_rx.is_empty(),
                "overflow advances while bounded commands stay ready"
            );
            assert!(tx.capacity() < 128, "ordinary commands remain buffered");
            assert!(
                wakes.0.load(Ordering::Relaxed) > 0,
                "budget exhaustion wakes even with idle receive"
            );
        }
        sender.disconnect(Some(DisconnectReason::UselessPeer));
        let _ = Pin::new(&mut session).poll(&mut cx);
        assert!(
            session.is_disconnecting() || session.terminate_message.is_some(),
            "control command is observed with bounded backlog"
        );
        drop(session);
        assert_eq!(counter.get(), 0, "shutdown releases buffered command permits");
        client.abort();
    }
}
