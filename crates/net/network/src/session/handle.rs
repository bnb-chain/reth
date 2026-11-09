//! Session handles.

use crate::{
    message::PeerMessage,
    session::{
        active::{BroadcastItemCounter, MessageKind, MessageMetadata},
        conn::EthRlpxConnection,
        Direction, SessionId,
    },
    PendingSessionHandshakeError,
};
use alloy_primitives::U256;
use reth_ecies::ECIESError;
use reth_eth_wire::{
    errors::EthStreamError, Capabilities, DisconnectReason, EthVersion, NetworkPrimitives,
    UnifiedStatus,
};
use reth_network_api::PeerInfo;
use reth_network_peers::{NodeRecord, PeerId};
use reth_network_types::PeerKind;
use std::{io, net::SocketAddr, sync::Arc, time::Instant};
use tokio::sync::{
    mpsc::{self, error::SendError},
    oneshot,
};

/// A handler attached to a peer session that's not authenticated yet, pending Handshake and hello
/// message which exchanges the `capabilities` of the peer.
///
/// This session needs to wait until it is authenticated.
#[derive(Debug)]
pub struct PendingSessionHandle {
    /// Can be used to tell the session to disconnect the connection/abort the handshake process.
    pub(crate) disconnect_tx: Option<oneshot::Sender<()>>,
    /// The direction of the session
    pub(crate) direction: Direction,
}

// === impl PendingSessionHandle ===

impl PendingSessionHandle {
    /// Sends a disconnect command to the pending session.
    pub fn disconnect(&mut self) {
        if let Some(tx) = self.disconnect_tx.take() {
            let _ = tx.send(());
        }
    }

    /// Returns the direction of the pending session (inbound or outbound).
    pub const fn direction(&self) -> Direction {
        self.direction
    }
}

/// An established session with a remote peer.
///
/// Within an active session that supports the `Ethereum Wire Protocol`, three high-level tasks can
/// be performed: chain synchronization, block propagation and transaction exchange.
#[derive(Debug)]
pub struct ActiveSessionHandle<N: NetworkPrimitives> {
    /// The direction of the session
    pub(crate) direction: Direction,
    /// The assigned id for this session
    pub(crate) session_id: SessionId,
    /// negotiated eth version
    pub(crate) version: EthVersion,
    /// The identifier of the remote peer
    pub(crate) remote_id: PeerId,
    /// The timestamp when the session has been established.
    pub(crate) established: Instant,
    /// Announced capabilities of the peer.
    pub(crate) capabilities: Arc<Capabilities>,
    /// Sender for commands to the spawned session with broadcast-aware backpressure.
    pub(crate) commands: SessionCommandSender<N>,
    /// The client's name and version
    pub(crate) client_version: Arc<str>,
    /// The address we're connected to
    pub(crate) remote_addr: SocketAddr,
    /// The local address of the connection.
    pub(crate) local_addr: Option<SocketAddr>,
    /// The TCP listening port the peer announced in its `Hello` message, if non-zero.
    ///
    /// This is effectively deprecated, but we still keep it around if a peer announced it as it's
    /// likely still more useful than the ephemeral source port.
    pub(crate) peer_listen_port: Option<u16>,
    /// The Status message the peer sent for the `eth` handshake
    pub(crate) status: Arc<UnifiedStatus>,
    /// Most recently announced total difficulty, initially taken from the handshake.
    pub(crate) current_td: Option<U256>,
}

// === impl ActiveSessionHandle ===

impl<N: NetworkPrimitives> ActiveSessionHandle<N> {
    /// Sends a disconnect command to the session.
    pub fn disconnect(&self, reason: Option<DisconnectReason>) {
        self.commands.disconnect(reason);
    }

    /// Sends a disconnect command to the session via the unbounded channel.
    pub fn try_disconnect(
        &self,
        reason: Option<DisconnectReason>,
    ) -> Result<(), SendError<SessionCommand<N>>> {
        self.commands.try_disconnect(reason)
    }

    /// Returns the direction of the active session (inbound or outbound).
    pub const fn direction(&self) -> Direction {
        self.direction
    }

    /// Returns the assigned session id for this session.
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns the negotiated eth version for this session.
    pub const fn version(&self) -> EthVersion {
        self.version
    }

    /// Returns the identifier of the remote peer.
    pub const fn remote_id(&self) -> PeerId {
        self.remote_id
    }

    /// Returns the timestamp when the session has been established.
    pub const fn established(&self) -> Instant {
        self.established
    }

    /// Returns the announced capabilities of the peer.
    pub fn capabilities(&self) -> Arc<Capabilities> {
        self.capabilities.clone()
    }

    /// Returns the client's name and version.
    pub fn client_version(&self) -> Arc<str> {
        self.client_version.clone()
    }

    /// Returns the address we're connected to.
    pub const fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Returns the current number of in-flight broadcast items.
    pub fn queued_broadcast_items(&self) -> usize {
        self.commands.queued_broadcast_items()
    }

    /// Extracts the [`PeerInfo`] from the session handle.
    pub(crate) fn peer_info(&self, record: &NodeRecord, kind: PeerKind) -> PeerInfo {
        // For inbound connections, the `record` was built from the TCP socket address, which
        // carries the peer's OS-assigned ephemeral source port (not dialable). If the peer
        // announced a non-zero listening port in its `Hello` message, prefer that combined with
        // the connection IP so the resulting enode is actually dialable.
        let enode = match (self.direction, self.peer_listen_port) {
            (Direction::Incoming, Some(port)) => NodeRecord::new_with_ports(
                self.remote_addr.ip(),
                port,
                Some(record.udp_port),
                record.id,
            )
            .to_string(),
            _ => record.to_string(),
        };
        PeerInfo {
            remote_id: self.remote_id,
            direction: self.direction,
            enode,
            enr: None,
            remote_addr: self.remote_addr,
            local_addr: self.local_addr,
            capabilities: self.capabilities.clone(),
            client_version: self.client_version.clone(),
            eth_version: self.version,
            best_number: self.status.latest_block,
            best_td: self.current_td,
            status: self.status.clone(),
            session_established: self.established,
            kind,
        }
    }
}

/// Sender half of the session command channel with broadcast-aware backpressure.
///
/// Blocks use a dedicated bounded command lane and independent full-block/hash quotas.
/// Transaction gossip first uses the ordinary bounded lane, then a quota-limited overflow lane.
/// Permits cover command channels and the outgoing queue until transport handoff or cancellation.
#[derive(Debug)]
pub(crate) struct SessionCommandSender<N: NetworkPrimitives> {
    /// Bounded channel for all commands (primary path).
    tx: mpsc::Sender<QueuedSessionCommand<N>>,
    block_tx: mpsc::Sender<QueuedSessionCommand<N>>,
    /// Unbounded channel used for broadcasts that overflow the bounded channel, and for
    /// disconnect commands (which must never be dropped due to backpressure).
    unbounded_tx: mpsc::UnboundedSender<QueuedSessionCommand<N>>,
    /// Shared counter of in-flight broadcast items (channels + outgoing queue).
    broadcast_items: BroadcastItemCounter,
}

impl<N: NetworkPrimitives> SessionCommandSender<N> {
    /// Creates a new sender with the given bounded channel, unbounded channel, and shared counter.
    pub(crate) const fn new(
        tx: mpsc::Sender<QueuedSessionCommand<N>>,
        block_tx: mpsc::Sender<QueuedSessionCommand<N>>,
        unbounded_tx: mpsc::UnboundedSender<QueuedSessionCommand<N>>,
        broadcast_items: BroadcastItemCounter,
    ) -> Self {
        Self { tx, block_tx, unbounded_tx, broadcast_items }
    }

    /// Sends a disconnect command via the unbounded channel so it is never dropped due to
    /// backpressure.
    pub(crate) fn disconnect(&self, reason: Option<DisconnectReason>) {
        let _ = self.unbounded_tx.send(QueuedSessionCommand::disconnect(reason));
    }

    /// Sends a disconnect command via the unbounded channel.
    ///
    /// This is infallible from a capacity standpoint (unbounded), but will fail if the
    /// receiver has been dropped (session closed).
    pub(crate) fn try_disconnect(
        &self,
        reason: Option<DisconnectReason>,
    ) -> Result<(), SendError<SessionCommand<N>>> {
        self.unbounded_tx
            .send(QueuedSessionCommand::disconnect(reason))
            .map_err(|e| SendError(e.0.command))
    }

    /// Sends a message to the session with broadcast-aware backpressure.
    ///
    /// For broadcast messages, the broadcast item counter is incremented **before** the message
    /// enters any channel, ensuring the counter always reflects the true in-flight count.
    /// If the bounded channel is full, broadcasts overflow to the unbounded channel (up to the
    /// item limit). Non-broadcast messages that cannot fit in the bounded channel are dropped.
    ///
    /// Returns `true` if the message was accepted, `false` if it was dropped.
    pub(crate) fn send_message(&self, msg: PeerMessage<N>) -> bool {
        let kind = MessageKind::of_peer(&msg);
        let broadcast = msg.is_broadcast();
        let permit = if broadcast {
            let Some(permit) = self.broadcast_items.try_acquire(kind, msg.message_item_count())
            else {
                kind.dropped("capacity");
                return false;
            };
            Some(permit)
        } else {
            None
        };
        let queued = QueuedSessionCommand {
            command: SessionCommand::Message(msg),
            metadata: MessageMetadata { admitted: Instant::now(), kind, permit },
        };
        if kind.is_block() {
            return match self.block_tx.try_send(queued) {
                Ok(()) => true,
                Err(error) => {
                    kind.dropped(if matches!(error, mpsc::error::TrySendError::Full(_)) {
                        "command_full"
                    } else {
                        "closed"
                    });
                    false
                }
            };
        }
        match self.tx.try_send(queued) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(queued)) if broadcast => {
                if self.unbounded_tx.send(queued).is_ok() {
                    true
                } else {
                    kind.dropped("closed");
                    false
                }
            }
            Err(error) => {
                kind.dropped(if matches!(error, mpsc::error::TrySendError::Full(_)) {
                    "command_full"
                } else {
                    "closed"
                });
                false
            }
        }
    }

    /// Returns the current number of in-flight broadcast items.
    pub(crate) fn queued_broadcast_items(&self) -> usize {
        self.broadcast_items.get()
    }
}

/// Events a pending session can produce.
///
/// This represents the state changes a session can undergo until it is ready to send capability messages <https://github.com/ethereum/devp2p/blob/6b0abc3d956a626c28dce1307ee9f546db17b6bd/rlpx.md>.
///
/// A session starts with a `Handshake`, followed by a `Hello` message which
#[derive(Debug)]
pub enum PendingSessionEvent<N: NetworkPrimitives> {
    /// Represents a successful `Hello` and `Status` exchange: <https://github.com/ethereum/devp2p/blob/6b0abc3d956a626c28dce1307ee9f546db17b6bd/rlpx.md#hello-0x00>
    Established {
        /// An internal identifier for the established session
        session_id: SessionId,
        /// The remote node's socket address
        remote_addr: SocketAddr,
        /// The local address of the connection
        local_addr: Option<SocketAddr>,
        /// The remote node's public key
        peer_id: PeerId,
        /// All capabilities the peer announced
        capabilities: Arc<Capabilities>,
        /// The Status message the peer sent for the `eth` handshake
        status: Arc<UnifiedStatus>,
        /// The actual connection stream which can be used to send and receive `eth` protocol
        /// messages
        conn: EthRlpxConnection<N>,
        /// The direction of the session, either `Inbound` or `Outgoing`
        direction: Direction,
        /// The remote node's user agent, usually containing the client name and version
        client_id: String,
        /// The TCP listening port the peer announced in its `Hello` message, if non-zero.
        ///
        /// See `ActiveSessionHandle::peer_listen_port` for context.
        peer_listen_port: Option<u16>,
    },
    /// Handshake unsuccessful, session was disconnected.
    Disconnected {
        /// The remote node's socket address
        remote_addr: SocketAddr,
        /// The internal identifier for the disconnected session
        session_id: SessionId,
        /// The direction of the session, either `Inbound` or `Outgoing`
        direction: Direction,
        /// The error that caused the disconnect
        error: Option<PendingSessionHandshakeError>,
    },
    /// Thrown when unable to establish a [`TcpStream`](tokio::net::TcpStream).
    OutgoingConnectionError {
        /// The remote node's socket address
        remote_addr: SocketAddr,
        /// The internal identifier for the disconnected session
        session_id: SessionId,
        /// The remote node's public key
        peer_id: PeerId,
        /// The error that caused the outgoing connection failure
        error: io::Error,
    },
    /// Thrown when authentication via ECIES failed.
    EciesAuthError {
        /// The remote node's socket address
        remote_addr: SocketAddr,
        /// The internal identifier for the disconnected session
        session_id: SessionId,
        /// The error that caused the ECIES session to fail
        error: ECIESError,
        /// The direction of the session, either `Inbound` or `Outgoing`
        direction: Direction,
    },
}

/// Internal envelope; the public SessionCommand stays independent of queue accounting.
#[derive(Debug)]
pub(crate) struct QueuedSessionCommand<N: NetworkPrimitives> {
    pub(crate) command: SessionCommand<N>,
    pub(crate) metadata: MessageMetadata,
}

impl<N: NetworkPrimitives> QueuedSessionCommand<N> {
    fn disconnect(reason: Option<DisconnectReason>) -> Self {
        Self {
            command: SessionCommand::Disconnect { reason },
            metadata: MessageMetadata {
                admitted: Instant::now(),
                kind: MessageKind::Other,
                permit: None,
            },
        }
    }
}

/// Commands that can be sent to the spawned session.
#[derive(Debug)]
pub enum SessionCommand<N: NetworkPrimitives> {
    /// Disconnect the connection
    Disconnect {
        /// Why the disconnect was initiated
        reason: Option<DisconnectReason>,
    },
    /// Sends a message to the peer
    Message(PeerMessage<N>),
}

/// Message variants an active session can produce and send back to the
/// [`SessionManager`](crate::session::SessionManager)
#[derive(Debug)]
pub enum ActiveSessionMessage<N: NetworkPrimitives> {
    /// Session was gracefully disconnected.
    Disconnected {
        /// The remote node's public key
        peer_id: PeerId,
        /// The remote node's socket address
        remote_addr: SocketAddr,
    },
    /// Session was closed due an error
    ClosedOnConnectionError {
        /// The remote node's public key
        peer_id: PeerId,
        /// The remote node's socket address
        remote_addr: SocketAddr,
        /// The error that caused the session to close
        error: EthStreamError,
    },
    /// A session received a valid message via `RLPx`.
    ValidMessage {
        /// Identifier of the remote peer.
        peer_id: PeerId,
        /// Message received from the peer.
        message: PeerMessage<N>,
    },
    /// Received a bad message from the peer.
    BadMessage {
        /// Identifier of the remote peer.
        peer_id: PeerId,
    },
    /// Remote peer is considered in protocol violation
    ProtocolBreach {
        /// Identifier of the remote peer.
        peer_id: PeerId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        message::NewBlockMessage,
        session::active::{MAX_QUEUED_BLOCKS, MAX_QUEUED_BLOCK_HASHES},
    };
    use alloy_primitives::B256;
    use reth_eth_wire::{
        BlockHashNumber, EthNetworkPrimitives, NewBlockHashes, NewPooledTransactionHashes66,
    };

    #[test]
    fn independent_broadcast_budgets_cover_channels_and_reject_oversized_batches() {
        let (tx, rx) = mpsc::channel(1);
        let (block_tx, block_rx) = mpsc::channel(32);
        let (overflow_tx, overflow_rx) = mpsc::unbounded_channel();
        let counter = BroadcastItemCounter::new();
        let sender = SessionCommandSender::<EthNetworkPrimitives>::new(
            tx,
            block_tx,
            overflow_tx,
            counter.clone(),
        );
        let tx_hashes = |count| {
            PeerMessage::PooledTransactions(
                NewPooledTransactionHashes66(vec![B256::ZERO; count]).into(),
            )
        };
        assert!(!sender.send_message(tx_hashes(4097)));
        assert!(sender.send_message(tx_hashes(4095)));
        assert!(sender.send_message(tx_hashes(0)), "empty batch reserves one slot in overflow");
        assert!(!sender.send_message(tx_hashes(0)));
        for _ in 0..MAX_QUEUED_BLOCKS {
            assert!(sender.send_message(PeerMessage::NewBlock(NewBlockMessage {
                hash: B256::ZERO,
                block: Arc::default()
            })));
        }
        assert!(!sender.send_message(PeerMessage::NewBlock(NewBlockMessage {
            hash: B256::ZERO,
            block: Arc::default()
        })));
        let blocks = |count| {
            PeerMessage::NewBlockHashes(NewBlockHashes(vec![BlockHashNumber::default(); count]))
        };
        assert!(!sender.send_message(blocks(MAX_QUEUED_BLOCK_HASHES + 1)));
        for _ in 0..MAX_QUEUED_BLOCK_HASHES {
            assert!(sender.send_message(blocks(0)));
        }
        assert!(!sender.send_message(blocks(0)));
        assert_eq!(counter.get(), 4096 + 16 + 16);
        drop((rx, block_rx, overflow_rx));
        assert_eq!(counter.get(), 0, "teardown releases all three channels");
        assert!(!sender.send_message(blocks(1)));
        assert!(!sender.send_message(tx_hashes(1)));
        assert_eq!(counter.get(), 0, "closed channels cannot leak a reservation");
    }

    #[test]
    fn closed_overflow_rejects_and_failed_admission_can_retry() {
        let (tx, mut rx) = mpsc::channel(1);
        let (block_tx, mut block_rx) = mpsc::channel(1);
        let (overflow_tx, overflow_rx) = mpsc::unbounded_channel();
        let counter = BroadcastItemCounter::new();
        let sender = SessionCommandSender::<EthNetworkPrimitives>::new(
            tx,
            block_tx,
            overflow_tx,
            counter.clone(),
        );
        let tx_message = || {
            PeerMessage::PooledTransactions(NewPooledTransactionHashes66(vec![B256::ZERO]).into())
        };
        assert!(sender.send_message(tx_message()));
        drop(overflow_rx);
        assert!(!sender.send_message(tx_message()), "closed overflow is not successful admission");
        assert_eq!(counter.get(), 1);
        drop(rx.try_recv().unwrap());
        assert!(sender.send_message(tx_message()));
        let block_message =
            || PeerMessage::NewBlockHashes(NewBlockHashes(vec![BlockHashNumber::default()]));
        assert!(sender.send_message(block_message()));
        assert!(
            !sender.send_message(block_message()),
            "full block command channel releases its permit"
        );
        assert_eq!(counter.get(), 2);
        drop(block_rx.try_recv().unwrap());
        assert!(sender.send_message(block_message()), "same block is eligible again");
        drop((rx, block_rx));
        assert_eq!(counter.get(), 0);
    }
}
