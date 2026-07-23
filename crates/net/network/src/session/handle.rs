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
            status: self.status.clone(),
            session_established: self.established,
            kind,
            best_hash,
            best_number,
            best_td: td,
        }
    }
}

/// Sender half of the session command channel with broadcast-aware backpressure.
///
/// Commands are first sent through a bounded channel. If the bounded channel is full and the
/// message is a broadcast with room under the broadcast item limit, it overflows to a dedicated
/// unbounded channel that the session task drains alongside the bounded one.
///
/// The shared `broadcast_items` counter tracks items across **all** buffers (bounded channel,
/// overflow channel, and the session's outgoing queue), so the
/// [`SessionManager`](super::SessionManager) has an accurate view of total in-flight broadcast
/// pressure.
#[derive(Debug)]
pub(crate) struct SessionCommandSender<N: NetworkPrimitives> {
    /// Bounded channel for all commands (primary path).
    tx: mpsc::Sender<SessionCommand<N>>,
    /// Unbounded channel used for broadcasts that overflow the bounded channel, and for
    /// disconnect commands (which must never be dropped due to backpressure).
    unbounded_tx: mpsc::UnboundedSender<SessionCommand<N>>,
    /// Shared counter of in-flight broadcast items (channels + outgoing queue).
    broadcast_items: BroadcastItemCounter,
}

impl<N: NetworkPrimitives> SessionCommandSender<N> {
    /// Creates a new sender with the given bounded channel, unbounded channel, and shared counter.
    pub(crate) const fn new(
        tx: mpsc::Sender<SessionCommand<N>>,
        unbounded_tx: mpsc::UnboundedSender<SessionCommand<N>>,
        broadcast_items: BroadcastItemCounter,
    ) -> Self {
        Self { tx, unbounded_tx, broadcast_items }
    }

    /// Sends a disconnect command via the unbounded channel so it is never dropped due to
    /// backpressure.
    pub(crate) fn disconnect(&self, reason: Option<DisconnectReason>) {
        let _ = self.unbounded_tx.send(SessionCommand::Disconnect { reason });
    }

    /// Sends a disconnect command via the unbounded channel.
    ///
    /// This is infallible from a capacity standpoint (unbounded), but will fail if the
    /// receiver has been dropped (session closed).
    pub(crate) fn try_disconnect(
        &self,
        reason: Option<DisconnectReason>,
    ) -> Result<(), SendError<SessionCommand<N>>> {
        self.unbounded_tx.send(SessionCommand::Disconnect { reason }).map_err(|e| SendError(e.0))
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
        if msg.is_broadcast() {
            let items = msg.message_item_count();

            // Check + increment atomically (optimistic)
            if !self.broadcast_items.try_add(items) {
                return false;
            }

            // Try bounded channel first
            match self.tx.try_send(SessionCommand::Message(msg)) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(cmd)) => {
                    // Overflow to unbounded channel (counter already incremented)
                    let _ = self.unbounded_tx.send(cmd);
                    true
                }
                Err(_) => {
                    // Channel closed, undo increment
                    self.broadcast_items.sub(items);
                    false
                }
            }
        } else {
            // Non-broadcast: bounded channel only
            match self.tx.try_send(SessionCommand::Message(msg)) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(SessionCommand::Message(msg))) => {
                    trace!(
                        target: "net::session",
                        msg_kind = msg.message_kind(),
                        "session command buffer full, dropping non-broadcast message"
                    );
                    false
                }
                Err(_) => false,
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
