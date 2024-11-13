use std::{
    clone::Clone,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{Sealable, B256};
use alloy_rpc_types::{engine::ForkchoiceState, BlockId, RpcBlockHash};
use reth_beacon_consensus::{
    BeaconEngineMessage, EngineNodeTypes, ForkchoiceStatus, MIN_BLOCKS_FOR_PIPELINE_RUN,
};
use reth_bsc_consensus::Parlia;
use reth_bsc_evm::SnapshotReader;
use reth_chainspec::EthChainSpec;
use reth_engine_primitives::EngineApiMessageVersion;
use reth_network_api::events::EngineMessage;
use reth_network_p2p::{
    headers::client::{HeadersClient, HeadersDirection, HeadersRequest},
    priority::Priority,
    BlockClient,
};
use reth_primitives::{Block, BlockBody, SealedHeader};
use reth_provider::{BlockReaderIdExt, CanonChainTracker, ParliaProvider};
use tokio::{
    signal,
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot, Mutex,
    },
    time::{interval, timeout, Duration},
};
use tracing::{debug, error, info, trace};

use crate::{client::ParliaClient, Storage};

// Minimum number of blocks for rebuilding the merkle tree
// When the number of blocks between the trusted header and the new header is less than this value,
// executing stage sync in batch can save time by avoiding merkle tree rebuilding.
const MIN_BLOCKS_FOR_MERKLE_REBUILD: u64 = 100_000;

/// All message variants that can be sent to beacon engine.
#[derive(Debug)]
enum ForkChoiceMessage {
    /// Broadcast new hash.
    NewHeader(NewHeaderEvent),
}
/// internal message to notify the engine of a new block
#[derive(Debug, Clone)]
struct NewHeaderEvent {
    header: SealedHeader,
    local_header: SealedHeader,
    pipeline_sync: bool,
}

/// A struct that contains a block hash or number and a block
#[derive(Debug, Clone)]
struct BlockInfo {
    block_hash: BlockHashOrNumber,
    block_number: u64,
    block: Option<Block>,
}

/// A Future that listens for new headers and puts into storage
pub(crate) struct ParliaEngineTask<
    N: EngineNodeTypes,
    Provider: BlockReaderIdExt + CanonChainTracker,
    SnapshotProvider: ParliaProvider,
    Client: BlockClient,
> {
    /// The configured chain spec
    chain_spec: Arc<N::ChainSpec>,
    /// The consensus instance
    consensus: Parlia,
    /// The provider used to read the block and header from the inserted chain
    provider: Provider,
    /// The snapshot reader used to read the snapshot
    snapshot_reader: Arc<SnapshotReader<SnapshotProvider>>,
    /// The client used to fetch headers
    block_fetcher: ParliaClient<Client>,
    /// The interval of the block producing
    block_interval: u64,
    /// Shared storage to insert new headers
    storage: Storage,
    /// The engine to send messages to the beacon engine
    to_engine: UnboundedSender<BeaconEngineMessage<N::Engine>>,
    /// The watch for the network block event receiver
    network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
    /// The channel to send fork choice messages
    fork_choice_tx: UnboundedSender<ForkChoiceMessage>,
    /// The channel to receive fork choice messages
    fork_choice_rx: Arc<Mutex<UnboundedReceiver<ForkChoiceMessage>>>,
    /// The channel to send chain tracker messages
    chain_tracker_tx: UnboundedSender<ForkChoiceMessage>,
    /// The channel to receive chain tracker messages
    chain_tracker_rx: Arc<Mutex<UnboundedReceiver<ForkChoiceMessage>>>,
    /// The threshold (in number of blocks) for switching from incremental trie building of changes
    /// to whole rebuild.
    merkle_clean_threshold: u64,
}

// === impl ParliaEngineTask ===
impl<
        N: EngineNodeTypes + 'static,
        Provider: BlockReaderIdExt + CanonChainTracker + Clone + 'static,
        SnapshotProvider: ParliaProvider + 'static,
        Client: BlockClient + 'static,
    > ParliaEngineTask<N, Provider, SnapshotProvider, Client>
{
    /// Creates a new instance of the task
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        chain_spec: Arc<N::ChainSpec>,
        consensus: Parlia,
        provider: Provider,
        snapshot_reader: SnapshotReader<SnapshotProvider>,
        to_engine: UnboundedSender<BeaconEngineMessage<N::Engine>>,
        network_block_event_rx: Arc<Mutex<UnboundedReceiver<EngineMessage>>>,
        storage: Storage,
        block_fetcher: ParliaClient<Client>,
        block_interval: u64,
        merkle_clean_threshold: u64,
    ) {
        let (fork_choice_tx, fork_choice_rx) = mpsc::unbounded_channel();
        let (chain_tracker_tx, chain_tracker_rx) = mpsc::unbounded_channel();
        let this = Self {
            chain_spec,
            consensus,
            provider,
            snapshot_reader: Arc::new(snapshot_reader),
            to_engine,
            network_block_event_rx,
            storage,
            block_fetcher,
            block_interval,
            fork_choice_tx,
            fork_choice_rx: Arc::new(Mutex::new(fork_choice_rx)),
            chain_tracker_tx,
            chain_tracker_rx: Arc::new(Mutex::new(chain_tracker_rx)),
            merkle_clean_threshold,
        };

        this.start_block_event_listening();
        this.start_fork_choice_update_notifier();
        this.start_chain_tracker_notifier();
    }

    /// Start listening to the network block event
    fn start_block_event_listening(&self) {
        let engine_rx = self.network_block_event_rx.clone();
        let block_interval = self.block_interval;
        let mut interval = interval(Duration::from_secs(block_interval));
        let chain_spec = self.chain_spec.clone();
        let storage = self.storage.clone();
        let client = self.provider.clone();
        let block_fetcher = self.block_fetcher.clone();
        let consensus = self.consensus.clone();
        let fork_choice_tx = self.fork_choice_tx.clone();
        let chain_tracker_tx = self.chain_tracker_tx.clone();
        let fetch_header_timeout_duration = Duration::from_secs(block_interval);
        let merkle_clean_threshold = self.merkle_clean_threshold;

        tokio::spawn(async move {
            loop {
                let read_storage = storage.read().await;
                let best_header = read_storage.best_header.clone();
                let finalized_hash = read_storage.best_finalized_hash;
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
                        block_fetcher.get_header_with_priority(info.block_hash, Priority::Normal),
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
                let finalized_header = client
                    .sealed_header_by_id(BlockId::Hash(RpcBlockHash::from(finalized_hash)))
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| {
                        SealedHeader::new(
                            chain_spec.genesis_header().clone(),
                            chain_spec.genesis_hash(),
                        )
                    });
                debug!(target: "consensus::parlia", { finalized_header_number = ?finalized_header.number, finalized_header_hash = ?finalized_header.hash() }, "Latest finalized header");
                let latest_unsafe_header =
                    client.latest_header().ok().flatten().unwrap_or_else(|| {
                        SealedHeader::new(
                            chain_spec.genesis_header().clone(),
                            chain_spec.genesis_hash(),
                        )
                    });
                debug!(target: "consensus::parlia", { latest_unsafe_header_number = ?latest_unsafe_header.number, latest_unsafe_header_hash = ?latest_unsafe_header.hash() }, "Latest unsafe header");

                let mut trusted_header = latest_unsafe_header.clone();
                // if parent hash is not equal to latest unsafe hash
                // may be a fork chain detected, we need to trust the finalized header
                if latest_header.number - 1 == latest_unsafe_header.number &&
                    latest_header.parent_hash != latest_unsafe_header.hash()
                {
                    trusted_header = finalized_header.clone();
                }

                // verify header and timestamp
                // predict timestamp is the trusted header timestamp plus the block interval times
                // the difference between the latest header number and the trusted
                // header number the timestamp of latest header should be bigger
                // than the predicted timestamp and less than the current timestamp.
                let predicted_timestamp = trusted_header.timestamp +
                    block_interval * (latest_header.number - 1 - trusted_header.number);
                let sealed = latest_header.clone().seal_slow();
                let (header, seal) = sealed.into_parts();
                let mut sealed_header = SealedHeader::new(header, seal);
                let is_valid_header = match consensus
                    .validate_header_with_predicted_timestamp(&sealed_header, predicted_timestamp)
                {
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
                // check if the header is the same as the block hash
                // that probably means the block is not sealed yet
                let block_hash = match info.block_hash {
                    BlockHashOrNumber::Hash(hash) => hash,
                    BlockHashOrNumber::Number(number) => {
                        // trigger by the interval tick, can only trust the number
                        if number != sealed_header.number {
                            continue;
                        }
                        sealed_header.hash_slow()
                    }
                };
                if sealed_header.hash_slow() != block_hash {
                    continue;
                }

                let mut disconnected_headers = Vec::new();
                let pipeline_sync =
                    (trusted_header.number + MIN_BLOCKS_FOR_PIPELINE_RUN) < sealed_header.number;
                if !pipeline_sync && (sealed_header.number - 1) > trusted_header.number {
                    let fetch_headers_result = match timeout(
                        fetch_header_timeout_duration,
                        block_fetcher.get_headers(HeadersRequest {
                            start: BlockHashOrNumber::Hash(sealed_header.parent_hash),
                            limit: (sealed_header.number - 1) - trusted_header.number,
                            direction: HeadersDirection::Falling,
                        }),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            trace!(target: "consensus::parlia", "Fetch header timeout");
                            continue
                        }
                    };
                    if fetch_headers_result.is_err() {
                        trace!(target: "consensus::parlia", "Failed to fetch header");
                        continue
                    }

                    let headers = fetch_headers_result.unwrap().into_data();
                    if headers.is_empty() {
                        continue
                    }
                    let mut parent_hash = sealed_header.parent_hash;
                    for (i, _) in headers.iter().enumerate() {
                        let sealed = headers[i].clone().seal_slow();
                        let (header, seal) = sealed.into_parts();
                        let sealed_header = SealedHeader::new(header, seal);
                        if sealed_header.hash_slow() != parent_hash {
                            break;
                        }
                        parent_hash = sealed_header.parent_hash;
                        disconnected_headers.push(sealed_header.clone());
                    }

                    // check if the length of the disconnected headers is the same as the headers
                    // if not, the headers are not valid
                    if disconnected_headers.len() != headers.len() {
                        continue;
                    }

                    // check last header.parent_hash is match the trusted header
                    if !disconnected_headers.is_empty() &&
                        disconnected_headers.last().unwrap().parent_hash != trusted_header.hash()
                    {
                        continue;
                    }
                };

                // if the target header is not far enough from the trusted header, make sure not to
                // rebuild the merkle tree
                if pipeline_sync &&
                    (sealed_header.number - trusted_header.number > merkle_clean_threshold &&
                        sealed_header.number - trusted_header.number <
                            MIN_BLOCKS_FOR_MERKLE_REBUILD)
                {
                    let fetch_headers_result = match timeout(
                        fetch_header_timeout_duration,
                        block_fetcher.get_headers(HeadersRequest {
                            start: (trusted_header.number + merkle_clean_threshold - 1).into(),
                            limit: 1,
                            direction: HeadersDirection::Falling,
                        }),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            trace!(target: "consensus::parlia", "Fetch header timeout");
                            continue
                        }
                    };
                    if fetch_headers_result.is_err() {
                        trace!(target: "consensus::parlia", "Failed to fetch header");
                        continue
                    }

                    let headers = fetch_headers_result.unwrap().into_data();
                    if headers.is_empty() {
                        continue
                    }

                    let sealed = headers[0].clone().seal_slow();
                    let (header, seal) = sealed.into_parts();
                    sealed_header = SealedHeader::new(header, seal);
                };

                disconnected_headers.insert(0, sealed_header.clone());
                disconnected_headers.reverse();
                // cache header and block
                let mut storage = storage.write().await;
                if info.block.is_some() {
                    storage.insert_new_block(
                        sealed_header.clone(),
                        BlockBody::from(info.block.clone().unwrap()),
                    );
                }
                for header in disconnected_headers {
                    storage.insert_new_header(header.clone());
                    let result =
                        fork_choice_tx.send(ForkChoiceMessage::NewHeader(NewHeaderEvent {
                            header: header.clone(),
                            // if the pipeline sync is true, the fork choice will not use the safe
                            // and finalized hash.
                            // this can make Block Sync Engine to use pipeline sync mode.
                            pipeline_sync,
                            local_header: latest_unsafe_header.clone(),
                        }));
                    if result.is_err() {
                        error!(target: "consensus::parlia", "Failed to send new block event to
                    fork choice");
                    }
                }
                drop(storage);

                let result = chain_tracker_tx.send(ForkChoiceMessage::NewHeader(NewHeaderEvent {
                    header: sealed_header.clone(),
                    pipeline_sync,
                    local_header: latest_unsafe_header.clone(),
                }));
                if result.is_err() {
                    error!(target: "consensus::parlia", "Failed to send new block event to chain tracker");
                }
            }
        });
        info!(target: "consensus::parlia", "started listening to network block event")
    }

    fn start_fork_choice_update_notifier(&self) {
        let fork_choice_rx = self.fork_choice_rx.clone();
        let to_engine = self.to_engine.clone();
        let storage = self.storage.clone();
        tokio::spawn(async move {
            loop {
                let mut fork_choice_rx_guard = fork_choice_rx.lock().await;
                tokio::select! {
                    msg = fork_choice_rx_guard.recv() => {
                        if msg.is_none() {
                            continue;
                        }
                        match msg.unwrap() {
                            ForkChoiceMessage::NewHeader(event) => {
                                // notify parlia engine
                                let new_header = event.header;
                                let storage = storage.read().await;
                                let safe_hash = storage.best_safe_hash;
                                let finalized_hash = storage.best_finalized_hash;
                                drop(storage);

                                // safe(justified) and finalized hash will be determined in the parlia consensus engine and stored in the snapshot after the block sync
                                let mut state = ForkchoiceState {
                                    head_block_hash: new_header.hash(),
                                    safe_block_hash: B256::ZERO,
                                    finalized_block_hash: B256::ZERO,
                                };
                                if !event.pipeline_sync {
                                    state.safe_block_hash = safe_hash;
                                    state.finalized_block_hash = finalized_hash;
                                }

                                // send the new update to the engine, this will trigger the engine
                                // to download and execute the block we just inserted
                                let (tx, rx) = oneshot::channel();
                                let _ = to_engine.send(BeaconEngineMessage::ForkchoiceUpdated {
                                    state,
                                    payload_attrs: None,
                                    tx,
                                    version: EngineApiMessageVersion::default(),
                                });
                                debug!(target: "consensus::parlia", ?state, "Sent fork choice update");

                                let rx_result = match rx.await {
                                    Ok(result) => result,
                                    Err(err)=> {
                                        error!(target: "consensus::parlia", ?err, "Fork choice update response failed");
                                        break
                                    }
                                };

                                match rx_result {
                                    Ok(fcu_response) => {
                                        match fcu_response.forkchoice_status() {
                                            ForkchoiceStatus::Valid => {
                                                trace!(target: "consensus::parlia", ?fcu_response, "Forkchoice update returned valid response");
                                            }
                                            ForkchoiceStatus::Invalid => {
                                                error!(target: "consensus::parlia", ?fcu_response, "Forkchoice update returned invalid response");
                                                continue
                                            }
                                            ForkchoiceStatus::Syncing => {
                                                trace!(target: "consensus::parlia", ?fcu_response, "Forkchoice update returned SYNCING, waiting for VALID");
                                                continue
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        error!(target: "consensus::parlia", %err, "Parlia fork choice update failed");
                                        continue
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

    fn start_chain_tracker_notifier(&self) {
        let chain_tracker_rx = self.chain_tracker_rx.clone();
        let snapshot_reader = self.snapshot_reader.clone();
        let provider = self.provider.clone();
        let storage = self.storage.clone();

        tokio::spawn(async move {
            loop {
                let mut chain_tracker_rx_guard = chain_tracker_rx.lock().await;
                tokio::select! {
                    msg = chain_tracker_rx_guard.recv() => {
                        if msg.is_none() {
                            continue;
                        }
                        match msg.unwrap() {
                            ForkChoiceMessage::NewHeader(event) => {
                                let new_header = event.local_header;

                                let snap = match snapshot_reader.snapshot(&new_header, None) {
                                    Ok(snap) => snap,
                                    Err(err) => {
                                        error!(target: "consensus::parlia", %err, "Snapshot not found");
                                        continue
                                    }
                                };
                                // safe finalized and safe hash for next round fcu
                                let finalized_hash = snap.vote_data.source_hash;
                                let safe_hash = snap.vote_data.target_hash;
                                let mut storage = storage.write().await;
                                storage.insert_finalized_and_safe_hash(finalized_hash, safe_hash);
                                drop(storage);

                                // notify chain tracker to help rpc module can know the finalized and safe hash
                                match provider.sealed_header(snap.vote_data.source_number) {
                                    Ok(header) => {
                                        if let Some(sealed_header) = header {
                                            provider.set_finalized(sealed_header.clone());
                                        }
                                    }
                                    Err(err) => {
                                        error!(target: "consensus::parlia", %err, "Failed to get source header");
                                    }
                                }

                                match provider.sealed_header(snap.vote_data.target_number) {
                                    Ok(header) => {
                                        if let Some(sealed_header) = header {
                                            provider.set_safe(sealed_header.clone());
                                        }
                                    }
                                    Err(err) => {
                                        error!(target: "consensus::parlia", %err, "Failed to get target header");
                                    }
                                }
                            }

                        }
                    }
                    _ = signal::ctrl_c() => {
                        info!(target: "consensus::parlia", "chain tracker notifier shutting down...");
                        return
                    },
                }
            }
        });

        info!(target: "consensus::parlia", "started chain tracker notifier")
    }
}

impl<
        N: EngineNodeTypes,
        Provider: BlockReaderIdExt + CanonChainTracker,
        SnapshotProvider: ParliaProvider,
        Client: BlockClient,
    > fmt::Debug for ParliaEngineTask<N, Provider, SnapshotProvider, Client>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("chain_spec")
            .field("chain_spec", &self.chain_spec)
            .field("consensus", &self.consensus)
            .field("storage", &self.storage)
            .field("block_fetcher", &self.block_fetcher)
            .field("block_interval", &self.block_interval)
            .finish_non_exhaustive()
    }
}
