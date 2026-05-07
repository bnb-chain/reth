use crate::{
    errors::{EthHandshakeError, EthStreamError, P2PStreamError},
    ethstream::MAX_MESSAGE_SIZE,
    CanDisconnect,
};
use bytes::{Bytes, BytesMut};
use futures::{Sink, SinkExt, Stream};
use reth_eth_wire_types::{
    DisconnectReason, EthMessage, EthNetworkPrimitives, ProtocolMessage, StatusMessage,
    UnifiedStatus,
};
use reth_ethereum_forks::ForkFilter;
use reth_primitives_traits::GotExpected;
use std::{fmt::Debug, future::Future, pin::Pin, time::Duration};
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tracing::{trace, warn};

/// A trait that knows how to perform the P2P handshake.
pub trait EthRlpxHandshake: Debug + Send + Sync + 'static {
    /// Perform the P2P handshake for the `eth` protocol.
    fn handshake<'a>(
        &'a self,
        unauth: &'a mut dyn UnauthEth,
        status: UnifiedStatus,
        fork_filter: ForkFilter,
        timeout_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<UnifiedStatus, EthStreamError>> + 'a + Send>>;
}

/// An unauthenticated stream that can send and receive messages.
pub trait UnauthEth:
    Stream<Item = Result<BytesMut, P2PStreamError>>
    + Sink<Bytes, Error = P2PStreamError>
    + CanDisconnect<Bytes>
    + Unpin
    + Send
{
}

impl<T> UnauthEth for T where
    T: Stream<Item = Result<BytesMut, P2PStreamError>>
        + Sink<Bytes, Error = P2PStreamError>
        + CanDisconnect<Bytes>
        + Unpin
        + Send
{
}

/// The Ethereum P2P handshake.
///
/// This performs the regular ethereum `eth` rlpx handshake.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct EthHandshake;

impl EthRlpxHandshake for EthHandshake {
    fn handshake<'a>(
        &'a self,
        unauth: &'a mut dyn UnauthEth,
        status: UnifiedStatus,
        fork_filter: ForkFilter,
        timeout_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<UnifiedStatus, EthStreamError>> + 'a + Send>> {
        Box::pin(async move {
            timeout(timeout_limit, EthereumEthHandshake(unauth).eth_handshake(status, fork_filter))
                .await
                .map_err(|_| EthStreamError::StreamTimeout)?
        })
    }
}

/// A type that performs the ethereum specific `eth` protocol handshake.
#[derive(Debug)]
pub struct EthereumEthHandshake<'a, S: ?Sized>(pub &'a mut S);

impl<S: ?Sized, E> EthereumEthHandshake<'_, S>
where
    S: Stream<Item = Result<BytesMut, E>> + CanDisconnect<Bytes> + Send + Unpin,
    EthStreamError: From<E> + From<<S as Sink<Bytes>>::Error>,
{
    /// Performs the `eth` rlpx protocol handshake using the given input stream.
    pub async fn eth_handshake(
        self,
        unified_status: UnifiedStatus,
        fork_filter: ForkFilter,
    ) -> Result<UnifiedStatus, EthStreamError> {
        let unauth = self.0;

        let status = unified_status.into_message();

        // Send our status message
        let status_msg = alloy_rlp::encode(ProtocolMessage::<EthNetworkPrimitives>::from(
            EthMessage::Status(status),
        ))
        .into();
        unauth.send(status_msg).await.map_err(EthStreamError::from)?;

        // Receive peer's response
        let their_msg_res = unauth.next().await;
        let their_msg = match their_msg_res {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => return Err(EthStreamError::from(e)),
            None => {
                unauth
                    .disconnect(DisconnectReason::DisconnectRequested)
                    .await
                    .map_err(EthStreamError::from)?;
                return Err(EthStreamError::EthHandshakeError(EthHandshakeError::NoResponse));
            }
        };

        if their_msg.len() > MAX_MESSAGE_SIZE {
            // ProtocolBreach origin: peer's first message larger than MAX_MESSAGE_SIZE.
            warn!(
                target: "net::eth-wire::handshake",
                msg_len = their_msg.len(),
                max = MAX_MESSAGE_SIZE,
                "eth handshake: peer's first message exceeds MAX_MESSAGE_SIZE -> sending DisconnectReason::ProtocolBreach"
            );
            unauth
                .disconnect(DisconnectReason::ProtocolBreach)
                .await
                .map_err(EthStreamError::from)?;
            return Err(EthStreamError::MessageTooBig(their_msg.len()));
        }

        let version = status.version();
        let msg = match ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            version,
            &mut their_msg.as_ref(),
        ) {
            Ok(m) => m,
            Err(err) => {
                // NOTE: this path emits DisconnectRequested (NOT ProtocolBreach), but
                // is the most common "peer sent garbage" cause for peer drops, so log
                // visibly when it happens.
                warn!(
                    target: "net::eth-wire::handshake",
                    eth_version = ?version,
                    msg_len = their_msg.len(),
                    decode_error = %err,
                    "eth handshake: failed to decode peer's first message; sending DisconnectRequested (NOT ProtocolBreach). msg={their_msg:x}"
                );
                unauth
                    .disconnect(DisconnectReason::DisconnectRequested)
                    .await
                    .map_err(EthStreamError::from)?;
                return Err(EthStreamError::InvalidMessage(err));
            }
        };

        // Validate peer response
        match msg.message {
            EthMessage::Status(their_status_message) => {
                trace!("Validating incoming ETH status from peer");

                if status.genesis() != their_status_message.genesis() {
                    // ProtocolBreach origin: peer is on a different chain genesis.
                    warn!(
                        target: "net::eth-wire::handshake",
                        ours = ?status.genesis(),
                        theirs = ?their_status_message.genesis(),
                        "eth handshake: mismatched genesis -> sending DisconnectReason::ProtocolBreach"
                    );
                    unauth
                        .disconnect(DisconnectReason::ProtocolBreach)
                        .await
                        .map_err(EthStreamError::from)?;
                    return Err(EthHandshakeError::MismatchedGenesis(
                        GotExpected {
                            expected: status.genesis(),
                            got: their_status_message.genesis(),
                        }
                        .into(),
                    )
                    .into());
                }

                if status.version() != their_status_message.version() {
                    // ProtocolBreach origin: negotiated eth version mismatch.
                    warn!(
                        target: "net::eth-wire::handshake",
                        ours = ?status.version(),
                        theirs = ?their_status_message.version(),
                        "eth handshake: mismatched eth protocol version -> sending DisconnectReason::ProtocolBreach"
                    );
                    unauth
                        .disconnect(DisconnectReason::ProtocolBreach)
                        .await
                        .map_err(EthStreamError::from)?;
                    return Err(EthHandshakeError::MismatchedProtocolVersion(GotExpected {
                        got: their_status_message.version(),
                        expected: status.version(),
                    })
                    .into());
                }

                if *status.chain() != *their_status_message.chain() {
                    // ProtocolBreach origin: peer reports a different chain id.
                    warn!(
                        target: "net::eth-wire::handshake",
                        ours = ?status.chain(),
                        theirs = ?their_status_message.chain(),
                        "eth handshake: mismatched chain id -> sending DisconnectReason::ProtocolBreach"
                    );
                    unauth
                        .disconnect(DisconnectReason::ProtocolBreach)
                        .await
                        .map_err(EthStreamError::from)?;
                    return Err(EthHandshakeError::MismatchedChain(GotExpected {
                        got: *their_status_message.chain(),
                        expected: *status.chain(),
                    })
                    .into());
                }

                // Ensure peer's total difficulty is reasonable
                if let StatusMessage::Legacy(s) = their_status_message &&
                    s.total_difficulty.bit_len() > 160
                {
                    // ProtocolBreach origin: total_difficulty bit length too large.
                    warn!(
                        target: "net::eth-wire::handshake",
                        td_bit_len = s.total_difficulty.bit_len(),
                        maximum = 160,
                        "eth handshake: peer total_difficulty bit_len > 160 -> sending DisconnectReason::ProtocolBreach"
                    );
                    unauth
                        .disconnect(DisconnectReason::ProtocolBreach)
                        .await
                        .map_err(EthStreamError::from)?;
                    return Err(EthHandshakeError::TotalDifficultyBitLenTooLarge {
                        got: s.total_difficulty.bit_len(),
                        maximum: 160,
                    }
                    .into());
                }

                // Fork validation
                if let Err(err) = fork_filter
                    .validate(their_status_message.forkid())
                    .map_err(EthHandshakeError::InvalidFork)
                {
                    // ProtocolBreach origin: fork filter rejected the peer's forkid.
                    warn!(
                        target: "net::eth-wire::handshake",
                        peer_forkid = ?their_status_message.forkid(),
                        error = %err,
                        "eth handshake: fork filter rejected peer's forkid -> sending DisconnectReason::ProtocolBreach"
                    );
                    unauth
                        .disconnect(DisconnectReason::ProtocolBreach)
                        .await
                        .map_err(EthStreamError::from)?;
                    return Err(err.into());
                }

                if let StatusMessage::Eth69(s) = their_status_message {
                    if s.earliest > s.latest {
                        return Err(EthHandshakeError::EarliestBlockGreaterThanLatestBlock {
                            got: s.earliest,
                            latest: s.latest,
                        }
                        .into());
                    }

                    if s.blockhash.is_zero() {
                        return Err(EthHandshakeError::BlockhashZero.into());
                    }
                }

                Ok(UnifiedStatus::from_message(their_status_message))
            }
            _ => {
                // ProtocolBreach origin: peer's first eth-protocol message was not a Status.
                warn!(
                    target: "net::eth-wire::handshake",
                    eth_version = ?version,
                    "eth handshake: peer's first eth message was not Status -> sending DisconnectReason::ProtocolBreach"
                );
                unauth
                    .disconnect(DisconnectReason::ProtocolBreach)
                    .await
                    .map_err(EthStreamError::from)?;
                Err(EthStreamError::EthHandshakeError(
                    EthHandshakeError::NonStatusMessageInHandshake,
                ))
            }
        }
    }
}
