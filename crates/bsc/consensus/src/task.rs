use crate::{client::ParliaClient, Parlia, Storage};
use reth_beacon_consensus::{BeaconEngineMessage, ForkchoiceStatus};
use reth_chainspec::ChainSpec;
use reth_engine_primitives::EngineTypes;
use reth_network::message::EngineMessage;
use reth_network_p2p::{headers::client::HeadersClient, priority::Priority};
use reth_primitives::{Block, BlockBody, BlockHashOrNumber, B256};
use reth_rpc_types::engine::ForkchoiceState;
use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

use tokio::{
    signal,
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    time::{interval, timeout, Duration},
};
use tracing::{debug, error, info, trace};

/// All message variants that can be sent to beacon engine.
#[derive(Debug)]
enum ForkChoiceMessage {
    /// Broadcast new hash.
    NewBlock(HashEvent),
}
/// internal message to beacon engine
#[derive(Debug, Clone)]
struct HashEvent {
    /// Hash of the block
    hash: B256,
}

/// A struct that contains a block hash or number and a block
#[derive(Debug, Clone)]
struct BlockInfo {
    block_hash: BlockHashOrNumber,
    block_number: u64,
    block: Option<Block>,
}

/// A Future that listens for new headers and puts into storage
pub(crate) struct ParliaEngineTask<Engine: EngineTypes> {
    /// The configured chain spec
    chain_spec: Arc<ChainSpec>,
    /// The coneensus instance
    consensus: Parlia,
    /// The client used to fetch headers
    block_fetcher: ParliaClient,
    fetch_header_timeout_duration: u64,
    /// Shared storage to insert new headers
    storage: Storage,
    /// The engine to send messages to the beacon engine
    to_engine: UnboundedSender<BeaconEngineMessage<Engine>>,
    /// The watch for the network block event receiver
    network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
    /// The channel to send fork choice messages
    fork_choice_tx: UnboundedSender<ForkChoiceMessage>,
    /// The channel to receive fork choice messages
    fork_choice_rx: Arc<Mutex<UnboundedReceiver<ForkChoiceMessage>>>,
}

// === impl ParliaEngineTask ===
impl<Engine: EngineTypes + 'static> ParliaEngineTask<Engine> {
    /// Creates a new instance of the task
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        chain_spec: Arc<ChainSpec>,
        consensus: Parlia,
        to_engine: UnboundedSender<BeaconEngineMessage<Engine>>,
        network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
        storage: Storage,
        block_fetcher: ParliaClient,
        fetch_header_timeout_duration: u64,
    ) {
        let (fork_choice_tx, fork_choice_rx) = mpsc::unbounded_channel();
        let this = Self {
            chain_spec,
            consensus,
            to_engine,
            network_block_event_rx,
            storage,
            block_fetcher,
            fetch_header_timeout_duration,
            fork_choice_tx,
            fork_choice_rx: Arc::new(Mutex::new(fork_choice_rx)),
        };

        this.start_block_event_listening();
        this.start_fork_choice_update_notifier();
    }

    /// Start listening to the network block event
    fn start_block_event_listening(&self) {
        let engine_rx = self.network_block_event_rx.clone();
        let mut interval = interval(Duration::from_secs(10));
        let storage = self.storage.clone();
        let block_fetcher = self.block_fetcher.clone();
        let consensus = self.consensus.clone();
        let fork_choice_tx = self.fork_choice_tx.clone();
        let fetch_header_timeout_duration = Duration::from_secs(self.fetch_header_timeout_duration);

        tokio::spawn(async move {
            loop {
                let read_storage = storage.read().await;
                let best_header = read_storage.best_header.clone();
                drop(read_storage);
                let mut engine_rx_guard = engine_rx.lock().await;
                let mut info = BlockInfo {
                    block_hash: BlockHashOrNumber::from(0),
                    block_number: 0,
                    block: None,
                };
                tokio::select! {
                    msg = engine_rx_guard.recv() => {
                        if msg.is_none() {
                            continue;
                        }
                        match msg.unwrap() {
                            EngineMessage::NewBlockHashes(event) => match event.hashes.last() {
                                None => continue,
                                Some(block_hash) => {
                                    info.block_hash = BlockHashOrNumber::Hash(block_hash.hash);
                                    info.block_number = block_hash.number;
                                }
                            },
                            EngineMessage::NewBlock(event) => {
                                info.block_hash = BlockHashOrNumber::Hash(event.hash);
                                info.block_number = event.block.block.number;
                                info.block = Some(event.block.block.clone());
                            }
                        }
                    }
                    _ = interval.tick() => {
                        // If head has not been updated for a long time, take the initiative to get it
                        if SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("Time went backwards")
                            .as_secs()
                            - best_header.timestamp
                            < 10 || best_header.number == 0
                        {
                            continue;
                        }
                        info.block_hash = BlockHashOrNumber::Number(best_header.number+1);
                        info.block_number = best_header.number+1;
                    }
                    _ = signal::ctrl_c() => {
                        info!(target: "consensus::parlia", "block event listener shutting down...");
                        return
                    },
                }

                // skip if number is lower than best number
                // TODO: if there is a big number incoming, will cause the sync broken, need a
                // better solution to handle this
                if info.block_number <= best_header.number {
                    continue;
                }

                let mut header_option = match info.block.clone() {
                    Some(block) => Some(block.header),
                    None => None,
                };

                if header_option.is_none() {
                    debug!(target: "consensus::parlia", { block_hash = ?info.block_hash }, "Fetching new header");
                    // fetch header and verify
                    let fetch_header_result = match timeout(
                        fetch_header_timeout_duration,
                        block_fetcher.get_header_with_priority(info.block_hash, Priority::High),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            trace!(target: "consensus::parlia", "Fetch header timeout");
                            continue
                        }
                    };
                    if fetch_header_result.is_err() {
                        trace!(target: "consensus::parlia", "Failed to fetch header");
                        continue
                    }

                    header_option = fetch_header_result.unwrap().into_data();
                    if header_option.is_none() {
                        trace!(target: "consensus::parlia", "Failed to unwrap header");
                        continue
                    }
                }
                let latest_header = header_option.unwrap();

                // skip if parent hash is not equal to best hash
                if latest_header.number == best_header.number + 1 &&
                    latest_header.parent_hash != best_header.hash()
                {
                    continue;
                }

                // verify header
                let sealed_header = latest_header.clone().seal_slow();
                let is_valid_header = match consensus.validate_header(&sealed_header) {
                    Ok(_) => true,
                    Err(err) => {
                        debug!(target: "consensus::parlia", %err, "Parlia verify header failed");
                        false
                    }
                };
                trace!(target: "consensus::parlia", sealed_header = ?sealed_header, is_valid_header = ?is_valid_header, "Fetch a sealed header");
                if !is_valid_header {
                    continue
                };

                // cache header and block
                let mut storage = storage.write().await;
                storage.insert_new_header(sealed_header.clone());
                if info.block.is_some() {
                    storage.insert_new_block(
                        sealed_header.clone(),
                        BlockBody::from(info.block.clone().unwrap()),
                    );
                }
                drop(storage);
                let result = fork_choice_tx
                    .send(ForkChoiceMessage::NewBlock(HashEvent { hash: sealed_header.hash() }));
                if result.is_err() {
                    error!(target: "consensus::parlia", "Failed to send new block event to fork choice");
                }
            }
        });
        info!(target: "consensus::parlia", "started listening to network block event")
    }

    fn start_fork_choice_update_notifier(&self) {
        let fork_choice_rx = self.fork_choice_rx.clone();
        let to_engine = self.to_engine.clone();
        tokio::spawn(async move {
            loop {
                let mut fork_choice_rx_guard = fork_choice_rx.lock().await;
                tokio::select! {
                    msg = fork_choice_rx_guard.recv() => {
                        if msg.is_none() {
                            continue;
                        }
                        match msg.unwrap() {
                            ForkChoiceMessage::NewBlock(event) => {
                                // notify parlia engine
                                let state = ForkchoiceState {
                                    head_block_hash: event.hash,
                                    // safe(justified) and finalized hash will be determined in the parlia consensus engine and stored in the snapshot after the block sync
                                    safe_block_hash: B256::ZERO,
                                    finalized_block_hash: B256::ZERO,
                                };


                                // send the new update to the engine, this will trigger the engine
                                // to download and execute the block we just inserted
                                let (tx, rx) = oneshot::channel();
                                let _ = to_engine.send(BeaconEngineMessage::ForkchoiceUpdated {
                                    state,
                                    payload_attrs: None,
                                    tx,
                                });
                                debug!(target: "consensus::parlia", ?state, "Sent fork choice update");

                                match rx.await.unwrap() {
                                    Ok(fcu_response) => {
                                        match fcu_response.forkchoice_status() {
                                            ForkchoiceStatus::Valid => {
                                                trace!(target: "consensus::parlia", ?fcu_response, "Forkchoice update returned valid response");
                                            }
                                            ForkchoiceStatus::Invalid => {
                                                error!(target: "consensus::parlia", ?fcu_response, "Forkchoice update returned invalid response");
                                            }
                                            ForkchoiceStatus::Syncing => {
                                                debug!(target: "consensus::parlia", ?fcu_response, "Forkchoice update returned SYNCING, waiting for VALID");
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        error!(target: "consensus::parlia", %err, "Parlia fork choice update failed");
                                    }
                                }
                            }
                        }
                    }
                    _ = signal::ctrl_c() => {
                        info!(target: "consensus::parlia", "fork choice notifier shutting down...");
                        return
                    },
                }
            }
        });
        info!(target: "consensus::parlia", "started fork choice notifier")
    }
}

impl<Engine: EngineTypes> fmt::Debug for ParliaEngineTask<Engine> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("chain_spec")
            .field("chain_spec", &self.chain_spec)
            .field("consensus", &self.consensus)
            .field("storage", &self.storage)
            .field("block_fetcher", &self.block_fetcher)
            .finish_non_exhaustive()
    }
}
