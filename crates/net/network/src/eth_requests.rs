//! Blocks/Headers management for the p2p network.

use crate::{
    budget::DEFAULT_BUDGET_TRY_DRAIN_DOWNLOADERS, metered_poll_nested_stream_with_budget,
    metrics::EthRequestHandlerMetrics,
};
use alloy_consensus::{constants::KECCAK_EMPTY, BlockHeader, ReceiptWithBloom};
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{Bytes, Sealable, B256};
use alloy_rlp::{length_of_length, Encodable};
use futures::StreamExt;
use reth_eth_wire::{
    message::MAX_MESSAGE_SIZE,
    snap::{
        AccountData, AccountRangeMessage, BlockAccessListsMessage, ByteCodesMessage,
        GetAccountRangeMessage, GetStorageRangesMessage, SnapProtocolMessage, StorageData,
        StorageRangesMessage,
    },
    BlockAccessLists, BlockBodies, BlockHeaders, Cells, EthNetworkPrimitives, GetBlockAccessLists,
    GetBlockBodies, GetBlockHeaders, GetCells, GetNodeData, GetReceipts, GetReceipts70,
    HeadersDirection, NetworkPrimitives, NodeData, Receipts, Receipts69, Receipts70,
};
use reth_network_api::test_utils::PeersHandle;
use reth_network_p2p::{
    error::{RequestError, RequestResult},
    snap::client::SnapResponse,
};
use reth_network_peers::PeerId;
use reth_primitives_traits::Block;
use reth_storage_api::{
    errors::provider::ProviderResult, BalProvider, BlockHashReader, BlockReader, BytecodeReader,
    GetBlockAccessListLimit, HeaderProvider, RangeEnd, RangeResponse, StateProviderFactory,
    StateRangeProviderFactory,
};
use reth_transaction_pool::{blobstore::NoopBlobStore, BlobStore};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{mpsc::Receiver, oneshot};
use tokio_stream::wrappers::ReceiverStream;

// Limits: <https://github.com/ethereum/go-ethereum/blob/b0d44338bbcefee044f1f635a84487cbbd8f0538/eth/protocols/eth/handler.go#L34-L56>

/// Maximum number of receipts to serve.
///
/// Used to limit lookups.
pub const MAX_RECEIPTS_SERVE: usize = 1024;

/// Maximum number of block headers to serve.
///
/// Used to limit lookups.
pub const MAX_HEADERS_SERVE: usize = 1024;

/// Maximum number of block headers to serve.
///
/// Used to limit lookups. With 24KB block sizes nowadays, the practical limit will always be
/// `SOFT_RESPONSE_LIMIT`.
pub const MAX_BODIES_SERVE: usize = 1024;

/// Maximum number of block access lists to serve.
///
/// Used to limit lookups.
pub const MAX_BLOCK_ACCESS_LISTS_SERVE: usize = 1024;

/// Maximum number of cell lookups to serve.
///
/// Used to limit lookups.
pub const MAX_CELLS_SERVE: usize = 1024;

/// Maximum number of bytecode lookups to serve.
///
/// Used to limit lookups.
pub const MAX_BYTE_CODES_SERVE: usize = 1024;

/// Maximum number of storage range account lookups to serve.
pub const MAX_STORAGE_RANGE_ACCOUNTS_SERVE: usize = 1024;

/// Maximum size of replies to data retrievals: 2MB
pub const SOFT_RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

/// Upper bound on the headers reserved once the first requested header resolves.
///
/// Header responses are bound by [`MAX_HEADERS_SERVE`] rather than by size, so a full batch reaches
/// its final capacity within a few doublings while a batch that runs past the local tip wastes at
/// most this many slots.
const MAX_HEADERS_RESERVE: usize = 64;

/// Upper bound on the block bodies or receipt lists reserved once the first requested block
/// resolves.
///
/// These responses are bound by [`SOFT_RESPONSE_LIMIT`], so the first block's size predicts how
/// many fit and this only caps the estimate for tiny blocks.
const MAX_BLOCKS_RESERVE: usize = 16;

/// Geth's shared budget for walking noncanonical ancestors in one header request.
const MAX_NON_CANONICAL_ANCESTORS: usize = 100;

/// Manages eth related requests on top of the p2p network.
///
/// This can be spawned to another task and is supposed to be run as background service.
#[derive(Debug)]
#[must_use = "Manager does nothing unless polled."]
pub struct EthRequestHandler<C, N: NetworkPrimitives = EthNetworkPrimitives> {
    /// The client type that can interact with the chain.
    client: C,
    /// Blob store used for serving blob cell requests.
    blob_store: Box<dyn BlobStore>,
    /// Used for reporting peers.
    // TODO use to report spammers
    #[expect(dead_code)]
    peers: PeersHandle,
    /// Incoming request from the [`NetworkManager`](crate::NetworkManager).
    incoming_requests: ReceiverStream<IncomingEthRequest<N>>,
    /// Metrics for the eth request handler.
    metrics: EthRequestHandlerMetrics,
}

// === impl EthRequestHandler ===
impl<C, N: NetworkPrimitives> EthRequestHandler<C, N> {
    /// Create a new instance
    pub fn new(client: C, peers: PeersHandle, incoming: Receiver<IncomingEthRequest<N>>) -> Self {
        Self {
            client,
            blob_store: Box::<NoopBlobStore>::default(),
            peers,
            incoming_requests: ReceiverStream::new(incoming),
            metrics: Default::default(),
        }
    }

    /// Set blob store for the request handler
    pub fn with_blob_store(mut self, blob_store: Box<dyn BlobStore>) -> Self {
        self.blob_store = blob_store;
        self
    }
}

impl<C, N> EthRequestHandler<C, N>
where
    N: NetworkPrimitives,
    C: HeaderProvider + BlockHashReader,
{
    /// Hash queries and contiguous number queries stop when ancestry checks detect a reorg.
    /// Number queries with skips select the requested canonical heights, as in Geth.
    fn get_headers_response(&self, request: GetBlockHeaders) -> Vec<C::Header> {
        let GetBlockHeaders { start_block, limit, skip, direction } = request;
        let mut headers = Vec::new();
        let limit = limit.min(MAX_HEADERS_SERVE as u64) as usize;
        if limit == 0 {
            return headers
        }
        let (mut hash, mut expected_number) = match start_block {
            BlockHashOrNumber::Hash(hash) => (hash, None),
            BlockHashOrNumber::Number(number) => {
                let Some(hash) = self.client.block_hash(number).unwrap_or_default() else {
                    return headers
                };
                (hash, Some(number))
            }
        };
        let hash_mode = matches!(start_block, BlockHashOrNumber::Hash(_));
        let distance = u64::from(skip) + 1;
        let mut noncanonical_budget = MAX_NON_CANONICAL_ANCESTORS;
        let mut total_bytes = 0;
        for _ in 0..limit {
            let Some(header) = self.exact_header(hash, expected_number) else { break };
            let number = header.number();
            let len = header.length();
            let parent_hash = header.parent_hash();
            if headers.is_empty() {
                headers.reserve(response_reserve_hint(limit, len, MAX_HEADERS_RESERVE));
            }
            total_bytes += len;
            headers.push(header);
            if headers.len() == limit || total_bytes > SOFT_RESPONSE_LIMIT {
                break
            }

            let next = match direction {
                HeadersDirection::Rising => {
                    let Some(next) = number.checked_add(distance) else { break };
                    let Some(next_hash) = self.client.block_hash(next).unwrap_or_default() else {
                        break
                    };
                    if (hash_mode || skip == 0) &&
                        self.header_ancestor(next_hash, next, distance, &mut noncanonical_budget) !=
                            Some(hash)
                    {
                        break
                    }
                    (next_hash, next)
                }
                HeadersDirection::Falling => {
                    let Some(next) = number.checked_sub(distance) else { break };
                    let next_hash = if !hash_mode && skip > 0 {
                        self.client.block_hash(next).unwrap_or_default()
                    } else if distance == 1 {
                        Some(parent_hash)
                    } else {
                        self.header_ancestor(hash, number, distance, &mut noncanonical_budget)
                    };
                    let Some(next_hash) = next_hash else { break };
                    // Number requests select only canonical headers, even when the previous
                    // header moved to a side branch during this response.
                    if !hash_mode &&
                        skip == 0 &&
                        self.client.block_hash(next).unwrap_or_default() != Some(next_hash)
                    {
                        break
                    }
                    (next_hash, next)
                }
            };
            hash = next.0;
            expected_number = Some(next.1);
        }
        headers
    }

    /// Height-indexed providers must not substitute another branch for an exact hash request.
    fn exact_header(&self, hash: B256, expected_number: Option<u64>) -> Option<C::Header> {
        self.client.header(hash).ok()?.filter(|header| {
            header.hash_slow() == hash &&
                expected_number.is_none_or(|number| header.number() == number)
        })
    }

    /// Follows exact parents, using Geth's checked canonical shortcut for long skips.
    fn header_ancestor(
        &self,
        mut hash: B256,
        mut number: u64,
        mut distance: u64,
        noncanonical_budget: &mut usize,
    ) -> Option<B256> {
        number.checked_sub(distance)?;
        if distance == 1 {
            return self.exact_header(hash, Some(number)).map(|header| header.parent_hash())
        }
        while distance > 0 {
            if self.client.block_hash(number).ok()? == Some(hash) {
                let ancestor = self.client.block_hash(number - distance).ok()?;
                // A reorg between the two canonical reads invalidates the shortcut.
                if self.client.block_hash(number).ok()? == Some(hash) {
                    return ancestor
                }
            }
            *noncanonical_budget = noncanonical_budget.checked_sub(1)?;
            let header = self.exact_header(hash, Some(number))?;
            hash = header.parent_hash();
            number -= 1;
            distance -= 1;
        }
        Some(hash)
    }
}

impl<C, N> EthRequestHandler<C, N>
where
    N: NetworkPrimitives,
    C: BlockReader,
{
    fn on_headers_request(
        &self,
        _peer_id: PeerId,
        request: GetBlockHeaders,
        response: oneshot::Sender<RequestResult<BlockHeaders<C::Header>>>,
    ) {
        self.metrics.eth_headers_requests_received_total.increment(1);
        let headers = self.get_headers_response(request);
        let _ = response.send(Ok(BlockHeaders(headers)));
    }

    fn on_bodies_request(
        &self,
        _peer_id: PeerId,
        request: GetBlockBodies,
        response: oneshot::Sender<RequestResult<BlockBodies<<C::Block as Block>::Body>>>,
    ) {
        self.metrics.eth_bodies_requests_received_total.increment(1);
        let count = request.0.len();
        let mut bodies = Vec::new();

        let mut total_bytes = 0;

        for hash in request {
            if let Some(block) = self.client.block_by_hash(hash).unwrap_or_default() {
                let body = block.into_body();
                let len = body.length();
                if bodies.is_empty() {
                    bodies.reserve(response_reserve_hint(count, len, MAX_BLOCKS_RESERVE));
                }
                total_bytes += len;
                bodies.push(body);

                if bodies.len() >= MAX_BODIES_SERVE || total_bytes > SOFT_RESPONSE_LIMIT {
                    break
                }
            } else {
                break
            }
        }

        let _ = response.send(Ok(BlockBodies(bodies)));
    }

    /// Replies to `GetNodeData`.
    ///
    /// State serving via eth `GetNodeData` was removed; answer with an empty payload so eth/66
    /// peers receive a response instead of hanging until the request timeout when the oneshot is
    /// dropped unanswered.
    fn on_node_data_request(
        &self,
        _peer_id: PeerId,
        _request: GetNodeData,
        response: oneshot::Sender<RequestResult<NodeData>>,
    ) {
        self.metrics.eth_node_data_requests_received_total.increment(1);
        let _ = response.send(Ok(NodeData(vec![])));
    }

    fn on_receipts_request(
        &self,
        _peer_id: PeerId,
        request: GetReceipts,
        response: oneshot::Sender<RequestResult<Receipts<C::Receipt>>>,
    ) {
        self.metrics.eth_receipts_requests_received_total.increment(1);

        let receipts = self.get_receipts_response(request, |receipts_by_block| {
            receipts_by_block.into_iter().map(ReceiptWithBloom::from).collect::<Vec<_>>()
        });

        let _ = response.send(Ok(Receipts(receipts)));
    }

    fn on_receipts69_request(
        &self,
        _peer_id: PeerId,
        request: GetReceipts,
        response: oneshot::Sender<RequestResult<Receipts69<C::Receipt>>>,
    ) {
        self.metrics.eth_receipts_requests_received_total.increment(1);

        let receipts = self.get_receipts_response(request, |receipts_by_block| {
            // skip bloom filter for eth69
            receipts_by_block
        });

        let _ = response.send(Ok(Receipts69(receipts)));
    }

    /// Handles partial responses for [`GetReceipts70`] queries.
    ///
    /// This will adhere to the soft limit but allow filling the last vec partially. A response
    /// never ends with an empty incomplete block: if not even the next receipt fits, the response
    /// ends after the last complete block, except when it would otherwise be empty, in which case
    /// the first receipt is served even if it exceeds the soft limit. If it cannot fit within
    /// [`MAX_MESSAGE_SIZE`], including the response framing, the response is empty and complete.
    fn on_receipts70_request(
        &self,
        _peer_id: PeerId,
        request: GetReceipts70,
        response: oneshot::Sender<RequestResult<Receipts70<C::Receipt>>>,
    ) {
        self.metrics.eth_receipts_requests_received_total.increment(1);

        let GetReceipts70 { first_block_receipt_index, block_hashes } = request;

        let count = block_hashes.len();
        let mut receipts = Vec::new();
        let mut total_bytes = 0usize;
        let mut last_block_incomplete = false;

        // The request ID is unavailable here. Reserve the message code, the largest request ID,
        // the incomplete flag, and the three RLP list headers to bound the full encoded response.
        let max_first_receipt_size = MAX_MESSAGE_SIZE -
            (1 + u64::MAX.length() + false.length() + 3 * length_of_length(MAX_MESSAGE_SIZE));

        for (idx, hash) in block_hashes.into_iter().enumerate() {
            if idx >= MAX_RECEIPTS_SERVE {
                break
            }

            let Some(mut block_receipts) =
                self.client.receipts_by_block(BlockHashOrNumber::Hash(hash)).unwrap_or_default()
            else {
                break
            };

            if idx == 0 && first_block_receipt_index > 0 {
                let skip = first_block_receipt_index as usize;
                if skip >= block_receipts.len() {
                    block_receipts.clear();
                } else {
                    block_receipts.drain(0..skip);
                }
            }

            let block_size = block_receipts.length();
            if receipts.is_empty() {
                receipts.reserve(response_reserve_hint(count, block_size, MAX_BLOCKS_RESERVE));
            }

            if total_bytes + block_size <= SOFT_RESPONSE_LIMIT {
                total_bytes += block_size;
                receipts.push(block_receipts);
                continue;
            }

            // If nothing has been added to the response yet, serve the first receipt even if it
            // exceeds the soft limit, provided the framed response fits within the hard limit.
            let always_serve_first = receipts.is_empty();
            let remaining = block_receipts.len();
            let mut partial_block = Vec::new();
            for receipt in block_receipts {
                let receipt_size = receipt.length();
                if total_bytes + receipt_size > SOFT_RESPONSE_LIMIT &&
                    !(always_serve_first &&
                        partial_block.is_empty() &&
                        receipt_size <= max_first_receipt_size)
                {
                    break;
                }
                total_bytes += receipt_size;
                partial_block.push(receipt);
            }

            // An empty partial block delivers nothing, so it is never sent. The requester can
            // simply continue with this block in its next request.
            if !partial_block.is_empty() {
                last_block_incomplete = partial_block.len() < remaining;
                receipts.push(partial_block);
            }
            break;
        }

        let _ = response.send(Ok(Receipts70 { last_block_incomplete, receipts }));
    }

    #[inline]
    fn get_receipts_response<T, F>(&self, request: GetReceipts, transform_fn: F) -> Vec<Vec<T>>
    where
        F: Fn(Vec<C::Receipt>) -> Vec<T>,
        T: Encodable,
    {
        let count = request.0.len();
        let mut receipts = Vec::new();
        let mut total_bytes = 0;

        for hash in request {
            if let Some(receipts_by_block) =
                self.client.receipts_by_block(BlockHashOrNumber::Hash(hash)).unwrap_or_default()
            {
                let transformed_receipts = transform_fn(receipts_by_block);
                let len = transformed_receipts.length();
                if receipts.is_empty() {
                    receipts.reserve(response_reserve_hint(count, len, MAX_BLOCKS_RESERVE));
                }
                total_bytes += len;
                receipts.push(transformed_receipts);

                if receipts.len() >= MAX_RECEIPTS_SERVE || total_bytes > SOFT_RESPONSE_LIMIT {
                    break
                }
            } else {
                break
            }
        }

        receipts
    }

    fn on_cells_request(
        &self,
        _peer_id: PeerId,
        request: GetCells,
        response: oneshot::Sender<RequestResult<Cells>>,
    ) {
        let mut cells_response = Cells { cell_mask: request.cell_mask, ..Default::default() };
        let mut total_bytes = 0;
        let cell_mask = request.cell_mask();

        for hash in request.hashes.into_iter().take(MAX_CELLS_SERVE) {
            let Some(cells) = self.blob_store.get_cells(hash, cell_mask).unwrap_or_default() else {
                continue;
            };

            total_bytes += hash.length() + cells.length();
            cells_response.hashes.push(hash);
            cells_response.cells.push(cells);

            if total_bytes > SOFT_RESPONSE_LIMIT {
                break
            }
        }

        let _ = response.send(Ok(cells_response));
    }
}

/// Number of response items to reserve once the first requested item has resolved.
///
/// Assumes the remaining items are about as large as the first one and reserves as many as fit into
/// [`SOFT_RESPONSE_LIMIT`], bounded by the requested `count` and `max`.
fn response_reserve_hint(count: usize, first_len: usize, max: usize) -> usize {
    SOFT_RESPONSE_LIMIT.div_ceil(first_len.max(1)).min(count).min(max)
}

impl<C, N> EthRequestHandler<C, N>
where
    N: NetworkPrimitives,
    C: BalProvider,
{
    /// Handles [`GetBlockAccessLists`] queries.
    ///
    /// EIP-8159 defines the final `BlockAccessLists` response semantics:
    /// <https://eips.ethereum.org/EIPS/eip-8159>
    fn on_block_access_lists_request(
        &self,
        _peer_id: PeerId,
        mut request: GetBlockAccessLists,
        response: oneshot::Sender<RequestResult<BlockAccessLists>>,
    ) {
        self.metrics.eth_block_access_lists_requests_received_total.increment(1);
        request.0.truncate(MAX_BLOCK_ACCESS_LISTS_SERVE);

        let limit = GetBlockAccessListLimit::ResponseSizeSoftLimit(SOFT_RESPONSE_LIMIT);
        let access_lists =
            self.client.get_bals_by_hashes_with_limit(&request.0, limit).unwrap_or_default();
        let _ = response.send(Ok(BlockAccessLists(access_lists)));
    }
}

impl<C, N> EthRequestHandler<C, N>
where
    N: NetworkPrimitives,
    C: BalProvider + StateProviderFactory + StateRangeProviderFactory,
{
    /// Handles `snap/2` (EIP-8189) requests.
    ///
    /// `GetAccountRange`/`GetStorageRanges` are hash-native throughout and served from retained
    /// canonical roots via [`StateRangeProviderFactory`]. `GetByteCodes` is content-addressed and
    /// independent of any particular state root, so it's served directly.
    /// `GetBlockAccessLists` is answered from the same [`BalProvider`] store eth71's
    /// `GetBlockAccessLists` uses, since both serve the same underlying data.
    fn on_snap_request(
        &self,
        _peer_id: PeerId,
        request: SnapProtocolMessage,
        response: oneshot::Sender<RequestResult<SnapResponse>>,
    ) {
        self.metrics.snap_requests_received_total.increment(1);

        let result = match request {
            SnapProtocolMessage::GetAccountRange(req) => {
                let request_id = req.request_id;
                let response = self.get_account_range_response(req).unwrap_or_else(|error| {
                    tracing::debug!(target: "net::snap", %error, "failed to serve account range");
                    AccountRangeMessage { request_id, accounts: Vec::new(), proof: Vec::new() }
                });
                Ok(SnapResponse::AccountRange(response))
            }
            SnapProtocolMessage::GetStorageRanges(req) => {
                let request_id = req.request_id;
                let response = self.get_storage_ranges_response(req).unwrap_or_else(|error| {
                    tracing::debug!(target: "net::snap", %error, "failed to serve storage ranges");
                    StorageRangesMessage { request_id, slots: Vec::new(), proof: Vec::new() }
                });
                Ok(SnapResponse::StorageRanges(response))
            }
            SnapProtocolMessage::GetByteCodes(req) => {
                let codes = self
                    .get_byte_codes_response(&req.hashes, req.response_bytes as usize)
                    .unwrap_or_else(|error| {
                        tracing::debug!(target: "net::snap", %error, "failed to serve bytecodes");
                        Vec::new()
                    });
                Ok(SnapResponse::ByteCodes(ByteCodesMessage { request_id: req.request_id, codes }))
            }
            SnapProtocolMessage::GetBlockAccessLists(mut req) => {
                req.block_hashes.truncate(MAX_BLOCK_ACCESS_LISTS_SERVE);
                let limit = GetBlockAccessListLimit::ResponseSizeSoftLimit(
                    (req.response_bytes as usize).min(SOFT_RESPONSE_LIMIT),
                );
                let block_access_lists = self
                    .client
                    .get_bals_by_hashes_with_limit(&req.block_hashes, limit)
                    .unwrap_or_default();
                Ok(SnapResponse::BlockAccessLists(BlockAccessListsMessage {
                    request_id: req.request_id,
                    block_access_lists: BlockAccessLists(block_access_lists),
                }))
            }
            // The peer sent us a response-shaped message instead of a request; not something we
            // asked for.
            _ => Err(RequestError::BadResponse),
        };

        let _ = response.send(result);
    }

    /// Returns the bytecode for each of `hashes`, skipping hashes with no known code, stopping
    /// once `response_bytes` (capped at [`SOFT_RESPONSE_LIMIT`]) is exceeded.
    fn get_byte_codes_response(
        &self,
        hashes: &[B256],
        response_bytes: usize,
    ) -> ProviderResult<Vec<Bytes>> {
        let state = self.client.latest()?;
        let response_bytes = response_bytes.min(SOFT_RESPONSE_LIMIT);

        let mut codes = Vec::new();
        let mut total_bytes = 0;
        for hash in hashes.iter().take(MAX_BYTE_CODES_SERVE) {
            let bytes = if *hash == KECCAK_EMPTY {
                Bytes::new()
            } else {
                match state.bytecode_by_hash(hash)? {
                    Some(bytecode) => bytecode.original_bytes(),
                    None => continue,
                }
            };
            total_bytes += bytes.len();
            codes.push(bytes);

            if total_bytes > response_bytes {
                break
            }
        }
        Ok(codes)
    }

    /// Serves a `GetAccountRange` request via [`StateRangeProviderFactory`].
    ///
    /// Fails the request if a storage root or proof becomes unavailable after the range lookup.
    /// Proves the boundary between `starting_hash` and the last returned account, per snap/2's
    /// boundary-proof requirement, unless the range is a zero-origin range that exhausted the
    /// trie, in which case no proof is needed.
    fn get_account_range_response(
        &self,
        req: GetAccountRangeMessage,
    ) -> ProviderResult<AccountRangeMessage> {
        let empty = AccountRangeMessage {
            request_id: req.request_id,
            accounts: Vec::new(),
            proof: Vec::new(),
        };

        let response_bytes = (req.response_bytes as usize).min(SOFT_RESPONSE_LIMIT);
        let Some(state) = self.client.state_range_provider(req.root_hash)? else {
            return Ok(empty)
        };
        let RangeResponse { items: accounts, end } =
            state.account_range(req.starting_hash, req.limit_hash, response_bytes)?;

        let proof = if req.starting_hash.is_zero() && end == RangeEnd::Exhausted {
            Vec::new()
        } else {
            let boundary_keys = boundary_proof_keys(req.starting_hash, accounts.last());
            state.account_range_proof(&boundary_keys)?
        };

        let mut account_data = Vec::with_capacity(accounts.len());
        for (hash, account) in accounts {
            let storage_root = state.storage_root_by_hash(hash)?;
            account_data.push(AccountData::from_trie_account(
                hash,
                &account.into_trie_account(storage_root),
            ));
        }

        Ok(AccountRangeMessage { request_id: req.request_id, accounts: account_data, proof })
    }

    /// Serves a `GetStorageRanges` request via [`StateRangeProviderFactory`].
    ///
    /// `starting_hash`/`limit_hash` apply only to the first account. An account unavailable at
    /// this root makes the whole response empty, rather than skipping it and shifting later
    /// accounts' positions. A proof stops the response at the first account that isn't a
    /// complete, zero-origin range.
    fn get_storage_ranges_response(
        &self,
        req: GetStorageRangesMessage,
    ) -> ProviderResult<StorageRangesMessage> {
        let empty = StorageRangesMessage {
            request_id: req.request_id,
            slots: Vec::new(),
            proof: Vec::new(),
        };
        let Some(state) = self.client.state_range_provider(req.root_hash)? else {
            return Ok(empty)
        };
        let mut slots: Vec<Vec<StorageData>> = Vec::new();
        let mut proof = Vec::new();
        let mut remaining_bytes = (req.response_bytes as usize).min(SOFT_RESPONSE_LIMIT);

        for (i, &hashed_address) in
            req.account_hashes.iter().take(MAX_STORAGE_RANGE_ACCOUNTS_SERVE).enumerate()
        {
            // Keeps traversing storage-empty accounts under a zero budget so a real slot still
            // gets served; worst case is a lookup per account, bounded by the take() above.
            if remaining_bytes == 0 && slots.iter().any(|range| !range.is_empty()) {
                break
            }
            let (origin, limit) = if i == 0 {
                (
                    req.starting_hash.unwrap_or(B256::ZERO),
                    req.limit_hash.unwrap_or(B256::repeat_byte(0xff)),
                )
            } else {
                (B256::ZERO, B256::repeat_byte(0xff))
            };
            let Some(RangeResponse { items: account_slots, end }) =
                state.storage_range(hashed_address, origin, limit, remaining_bytes)?
            else {
                return Ok(empty)
            };

            remaining_bytes = remaining_bytes.saturating_sub(account_slots.len() * 64);
            let last = account_slots.last().map(|(hash, _)| *hash);
            let needs_proof = !origin.is_zero() || end != RangeEnd::Exhausted;
            slots.push(
                account_slots
                    .into_iter()
                    // snap clients verify proofs against RLP-encoded storage trie leaves.
                    .map(|(hash, value)| StorageData::from_value(hash, value))
                    .collect(),
            );

            if needs_proof {
                let boundary_keys = match last {
                    Some(last) => vec![origin, last],
                    None => vec![origin],
                };
                proof = state.storage_range_proof(hashed_address, &boundary_keys)?;
                break
            }
        }

        Ok(StorageRangesMessage { request_id: req.request_id, slots, proof })
    }
}

/// Boundary-proof keys for a range reply: `origin`, plus the last returned item's key if any.
fn boundary_proof_keys<T>(origin: B256, last: Option<&(B256, T)>) -> Vec<B256> {
    match last {
        Some((last, _)) => vec![origin, *last],
        None => vec![origin],
    }
}

/// An endless future.
///
/// This should be spawned or used as part of `tokio::select!`.
impl<C, N> Future for EthRequestHandler<C, N>
where
    N: NetworkPrimitives,
    C: BalProvider
        + StateProviderFactory
        + StateRangeProviderFactory
        + BlockReader<Block = N::Block, Receipt = N::Receipt>
        + HeaderProvider<Header = N::BlockHeader>
        + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let mut acc = Duration::ZERO;
        let maybe_more_incoming_requests = metered_poll_nested_stream_with_budget!(
            acc,
            "net::eth",
            "Incoming eth requests stream",
            DEFAULT_BUDGET_TRY_DRAIN_DOWNLOADERS,
            this.incoming_requests.poll_next_unpin(cx),
            |incoming| {
                match incoming {
                    IncomingEthRequest::GetBlockHeaders { peer_id, request, response } => {
                        this.on_headers_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetBlockBodies { peer_id, request, response } => {
                        this.on_bodies_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetNodeData { peer_id, request, response } => {
                        this.on_node_data_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetReceipts { peer_id, request, response } => {
                        this.on_receipts_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetReceipts69 { peer_id, request, response } => {
                        this.on_receipts69_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetReceipts70 { peer_id, request, response } => {
                        this.on_receipts70_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetBlockAccessLists { peer_id, request, response } => {
                        this.on_block_access_lists_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetCells { peer_id, request, response } => {
                        this.on_cells_request(peer_id, request, response)
                    }
                    IncomingEthRequest::GetSnap { peer_id, request, response } => {
                        this.on_snap_request(peer_id, request, response)
                    }
                }
            },
        );

        this.metrics.acc_duration_poll_eth_req_handler.set(acc.as_secs_f64());

        // stream is fully drained and import futures pending
        if maybe_more_incoming_requests {
            // make sure we're woken up again
            cx.waker().wake_by_ref();
        }

        Poll::Pending
    }
}

/// All `eth` request related to blocks delegated by the network.
#[derive(Debug)]
pub enum IncomingEthRequest<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Request Block headers from the peer.
    ///
    /// The response should be sent through the channel.
    GetBlockHeaders {
        /// The ID of the peer to request block headers from.
        peer_id: PeerId,
        /// The specific block headers requested.
        request: GetBlockHeaders,
        /// The channel sender for the response containing block headers.
        response: oneshot::Sender<RequestResult<BlockHeaders<N::BlockHeader>>>,
    },
    /// Request Block bodies from the peer.
    ///
    /// The response should be sent through the channel.
    GetBlockBodies {
        /// The ID of the peer to request block bodies from.
        peer_id: PeerId,
        /// The specific block bodies requested.
        request: GetBlockBodies,
        /// The channel sender for the response containing block bodies.
        response: oneshot::Sender<RequestResult<BlockBodies<N::BlockBody>>>,
    },
    /// Request Node Data from the peer.
    ///
    /// The response should be sent through the channel.
    GetNodeData {
        /// The ID of the peer to request node data from.
        peer_id: PeerId,
        /// The specific node data requested.
        request: GetNodeData,
        /// The channel sender for the response containing node data.
        response: oneshot::Sender<RequestResult<NodeData>>,
    },
    /// Request Receipts from the peer.
    ///
    /// The response should be sent through the channel.
    GetReceipts {
        /// The ID of the peer to request receipts from.
        peer_id: PeerId,
        /// The specific receipts requested.
        request: GetReceipts,
        /// The channel sender for the response containing receipts.
        response: oneshot::Sender<RequestResult<Receipts<N::Receipt>>>,
    },
    /// Request Receipts from the peer without bloom filter.
    ///
    /// The response should be sent through the channel.
    GetReceipts69 {
        /// The ID of the peer to request receipts from.
        peer_id: PeerId,
        /// The specific receipts requested.
        request: GetReceipts,
        /// The channel sender for the response containing Receipts69.
        response: oneshot::Sender<RequestResult<Receipts69<N::Receipt>>>,
    },
    /// Request Receipts from the peer using eth/70.
    ///
    /// The response should be sent through the channel.
    GetReceipts70 {
        /// The ID of the peer to request receipts from.
        peer_id: PeerId,
        /// The specific receipts requested including the `firstBlockReceiptIndex`.
        request: GetReceipts70,
        /// The channel sender for the response containing Receipts70.
        response: oneshot::Sender<RequestResult<Receipts70<N::Receipt>>>,
    },
    /// Request Block Access Lists from the peer.
    ///
    /// The response should be sent through the channel.
    GetBlockAccessLists {
        /// The ID of the peer to request block access lists from.
        peer_id: PeerId,
        /// The requested block hashes.
        request: GetBlockAccessLists,
        /// The channel sender for the response containing block access lists.
        response: oneshot::Sender<RequestResult<BlockAccessLists>>,
    },
    /// Request Cells from the peer.
    ///
    /// The response should be sent through the channel.
    GetCells {
        /// The ID of the peer to request cells from.
        peer_id: PeerId,
        /// The requested block hashes.
        request: GetCells,
        /// The channel sender for the response containing cells.
        response: oneshot::Sender<RequestResult<Cells>>,
    },
    /// Request a `snap/2` message from the peer.
    ///
    /// The response should be sent through the channel.
    GetSnap {
        /// The ID of the peer to request from.
        peer_id: PeerId,
        /// The `snap/2` request.
        request: SnapProtocolMessage,
        /// The channel sender for the response.
        response: oneshot::Sender<RequestResult<SnapResponse>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{constants::EMPTY_ROOT_HASH, Header, TxType};
    use alloy_eips::{
        eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
        eip7594::{BlobCellMask, BlobTransactionSidecarVariant, Cell},
    };
    use alloy_primitives::{keccak256, map::B256Map, Address, Log, LogData, TxHash, B128, U256};
    use reth_eth_wire::{message::RequestPair, EthMessage, EthStreamInner, EthVersion};
    use reth_ethereum_primitives::Receipt;
    use reth_network_api::test_utils::PeersHandle;
    use reth_primitives_traits::{Account, SealedHeader};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_storage_api::noop::NoopProvider;
    use reth_transaction_pool::blobstore::{
        BlobStoreCleanupStat, BlobStoreError, PooledBlobSidecar,
    };
    use std::{
        cell::{self, RefCell},
        collections::BTreeMap,
        ops::RangeBounds,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    use test_case::test_case;
    use tokio::sync::mpsc;

    #[derive(Debug, Default)]
    struct CountingBlobStore {
        get_cells_calls: Arc<AtomicUsize>,
        expected_cell_mask: Option<BlobCellMask>,
    }

    impl BlobStore for CountingBlobStore {
        fn insert(&self, _tx: B256, _data: PooledBlobSidecar) -> Result<(), BlobStoreError> {
            Ok(())
        }

        fn insert_all(&self, _txs: Vec<(B256, PooledBlobSidecar)>) -> Result<(), BlobStoreError> {
            Ok(())
        }

        fn delete(&self, _tx: B256) -> Result<(), BlobStoreError> {
            Ok(())
        }

        fn delete_all(&self, _txs: Vec<B256>) -> Result<(), BlobStoreError> {
            Ok(())
        }

        fn cleanup(&self) -> BlobStoreCleanupStat {
            BlobStoreCleanupStat::default()
        }

        fn get(
            &self,
            _tx: B256,
        ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
            Ok(None)
        }

        fn contains(&self, _tx: B256) -> Result<bool, BlobStoreError> {
            Ok(false)
        }

        fn get_all(
            &self,
            _txs: Vec<B256>,
        ) -> Result<Vec<(B256, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
            Ok(vec![])
        }

        fn get_exact(
            &self,
            txs: Vec<B256>,
        ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
            if txs.is_empty() {
                return Ok(vec![])
            }

            Err(BlobStoreError::MissingSidecar(txs[0]))
        }

        fn get_by_versioned_hashes_v1(
            &self,
            versioned_hashes: &[B256],
        ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
            Ok(vec![None; versioned_hashes.len()])
        }

        fn get_by_versioned_hashes_v2(
            &self,
            _versioned_hashes: &[B256],
        ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
            Ok(None)
        }

        fn get_by_versioned_hashes_v3(
            &self,
            versioned_hashes: &[B256],
        ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
            Ok(vec![None; versioned_hashes.len()])
        }

        fn get_by_versioned_hashes_v4(
            &self,
            versioned_hashes: &[B256],
            _cell_mask: BlobCellMask,
        ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
            Ok(vec![None; versioned_hashes.len()])
        }

        fn has_versioned_hashes(
            &self,
            versioned_hashes: &[B256],
        ) -> Result<Vec<bool>, BlobStoreError> {
            Ok(vec![false; versioned_hashes.len()])
        }

        fn get_cells(
            &self,
            _tx_hash: TxHash,
            cell_mask: BlobCellMask,
        ) -> Result<Option<Vec<Cell>>, BlobStoreError> {
            self.get_cells_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(expected) = self.expected_cell_mask {
                assert_eq!(cell_mask, expected);
                return Ok(Some(vec![Cell::default()]))
            }
            Ok(None)
        }

        fn data_size_hint(&self) -> Option<usize> {
            Some(0)
        }

        fn blobs_len(&self) -> usize {
            0
        }
    }

    #[tokio::test]
    async fn get_cells_request_limits_blob_store_lookups() {
        let (peers_tx, _) = mpsc::unbounded_channel();
        let (_incoming_tx, incoming_rx) = mpsc::channel(1);
        let get_cells_calls = Arc::new(AtomicUsize::new(0));
        let blob_store = CountingBlobStore {
            get_cells_calls: Arc::clone(&get_cells_calls),
            ..Default::default()
        };
        let handler = EthRequestHandler::<NoopProvider>::new(
            NoopProvider::default(),
            PeersHandle::new(peers_tx),
            incoming_rx,
        )
        .with_blob_store(Box::new(blob_store));
        let (response, rx) = oneshot::channel();
        let request =
            GetCells { hashes: vec![B256::ZERO; MAX_CELLS_SERVE + 1], cell_mask: B128::default() };

        handler.on_cells_request(PeerId::default(), request, response);

        let cells = rx.await.unwrap().unwrap();
        assert!(cells.hashes.is_empty());
        assert_eq!(get_cells_calls.load(Ordering::Relaxed), MAX_CELLS_SERVE);
    }

    #[test_case(0)]
    #[test_case(7)]
    #[test_case(8)]
    #[test_case(63)]
    #[test_case(64)]
    #[test_case(127)]
    #[tokio::test]
    async fn get_cells_request_uses_little_endian_wire_mask(index: u32) {
        use alloy_rlp::{Decodable, Encodable};

        let (peers_tx, _) = mpsc::unbounded_channel();
        let (_incoming_tx, incoming_rx) = mpsc::channel(1);
        let numeric_mask = 1u128 << index;
        let blob_store = CountingBlobStore {
            expected_cell_mask: Some(BlobCellMask::from_bits(numeric_mask)),
            ..Default::default()
        };
        let handler = EthRequestHandler::<NoopProvider>::new(
            NoopProvider::default(),
            PeersHandle::new(peers_tx),
            incoming_rx,
        )
        .with_blob_store(Box::new(blob_store));
        let wire_mask = B128::from(numeric_mask.to_le_bytes());
        let request = GetCells { hashes: vec![B256::ZERO], cell_mask: wire_mask };
        let mut encoded = Vec::new();
        request.encode(&mut encoded);
        let request = GetCells::decode(&mut encoded.as_slice()).unwrap();
        let (response, rx) = oneshot::channel();

        handler.on_cells_request(PeerId::default(), request, response);

        let cells = rx.await.unwrap().unwrap();
        assert_eq!(cells.hashes, vec![B256::ZERO]);
        assert_eq!(cells.cells, vec![vec![Cell::default()]]);
        assert_eq!(cells.cell_mask, wire_mask);
        encoded.clear();
        cells.encode(&mut encoded);
        assert_eq!(Cells::decode(&mut encoded.as_slice()).unwrap(), cells);
    }

    #[tokio::test]
    async fn get_node_data_responds_with_empty_payload() {
        let (peers_tx, _) = mpsc::unbounded_channel();
        let (_incoming_tx, incoming_rx) = mpsc::channel(1);
        let handler = EthRequestHandler::<NoopProvider>::new(
            NoopProvider::default(),
            PeersHandle::new(peers_tx),
            incoming_rx,
        );
        let (response, rx) = oneshot::channel();

        handler.on_node_data_request(PeerId::default(), GetNodeData(vec![B256::ZERO]), response);

        let node_data = rx.await.expect("response channel must not be dropped").unwrap();
        assert!(node_data.0.is_empty(), "GetNodeData should reply with empty NodeData");
    }

    /// Creates a request handler backed by the mock provider for snap response tests.
    fn snap_handler(
        provider: MockEthProvider,
    ) -> EthRequestHandler<MockEthProvider, EthNetworkPrimitives> {
        let (peers_tx, _) = mpsc::unbounded_channel();
        let (_incoming_tx, incoming_rx) = mpsc::channel(1);
        EthRequestHandler::new(provider, PeersHandle::new(peers_tx), incoming_rx)
    }

    #[test_case(
        SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
            request_id: 1,
            root_hash: B256::ZERO,
            starting_hash: B256::ZERO,
            limit_hash: B256::repeat_byte(0xff),
            response_bytes: SOFT_RESPONSE_LIMIT as u64,
        }),
        SnapResponse::AccountRange(AccountRangeMessage {
            request_id: 1,
            accounts: Vec::new(),
            proof: Vec::new(),
        }); "account range"
    )]
    #[test_case(
        SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
            request_id: 2,
            root_hash: B256::ZERO,
            account_hashes: vec![B256::ZERO],
            starting_hash: B256::ZERO.into(),
            limit_hash: B256::repeat_byte(0xff).into(),
            response_bytes: SOFT_RESPONSE_LIMIT as u64,
        }),
        SnapResponse::StorageRanges(StorageRangesMessage {
            request_id: 2,
            slots: Vec::new(),
            proof: Vec::new(),
        }); "storage ranges"
    )]
    #[test_case(
        SnapProtocolMessage::GetByteCodes(reth_eth_wire::snap::GetByteCodesMessage {
            request_id: 3,
            hashes: vec![B256::repeat_byte(0x11)],
            response_bytes: SOFT_RESPONSE_LIMIT as u64,
        }),
        SnapResponse::ByteCodes(ByteCodesMessage { request_id: 3, codes: Vec::new() }); "bytecodes"
    )]
    #[tokio::test]
    async fn snap_requests_return_empty_responses_on_provider_errors(
        request: SnapProtocolMessage,
        expected: SnapResponse,
    ) {
        let provider = MockEthProvider::default();
        provider.set_snap_state_reads_fail(true);
        let handler = snap_handler(provider);
        let (response, rx) = oneshot::channel();

        handler.on_snap_request(PeerId::default(), request, response);

        assert_eq!(rx.await.unwrap(), Ok(expected));
    }

    #[tokio::test]
    async fn unavailable_snap_state_returns_empty_response() {
        let provider = MockEthProvider::default();
        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
                request_id: 1,
                root_hash: B256::ZERO,
                starting_hash: B256::ZERO,
                limit_hash: B256::repeat_byte(0xff),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::AccountRange(AccountRangeMessage {
                request_id: 1,
                accounts: Vec::new(),
                proof: Vec::new(),
            }))
        );
        assert_eq!(provider.snap_state_range_resolutions(), 1);
    }

    #[tokio::test]
    async fn snap_requests_return_empty_responses_on_inconsistent_provider_results() {
        let missing_storage_root = MockEthProvider::default();
        missing_storage_root
            .set_snap_account_range(vec![(B256::ZERO, Account::default())], RangeEnd::Exhausted);

        let missing_account_proof = MockEthProvider::default();
        missing_account_proof.set_snap_account_range(Vec::new(), RangeEnd::Exhausted);

        let missing_storage_proof = MockEthProvider::default();
        missing_storage_proof
            .push_snap_storage_range(vec![(B256::ZERO, U256::ONE)], RangeEnd::ByteLimit);

        let storage_disappears = MockEthProvider::default();
        storage_disappears.push_snap_storage_range(Vec::new(), RangeEnd::Exhausted);
        storage_disappears.push_unavailable_snap_storage_range();

        let account_request = || {
            SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
                request_id: 1,
                root_hash: B256::ZERO,
                starting_hash: B256::ZERO,
                limit_hash: B256::repeat_byte(0xff),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            })
        };
        let storage_request = |account_hashes| {
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 2,
                root_hash: B256::ZERO,
                account_hashes,
                starting_hash: B256::ZERO.into(),
                limit_hash: B256::repeat_byte(0xff).into(),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            })
        };
        let empty_accounts = SnapResponse::AccountRange(AccountRangeMessage {
            request_id: 1,
            accounts: Vec::new(),
            proof: Vec::new(),
        });
        let empty_storage = SnapResponse::StorageRanges(StorageRangesMessage {
            request_id: 2,
            slots: Vec::new(),
            proof: Vec::new(),
        });
        let cases = [
            (missing_storage_root, account_request(), empty_accounts.clone()),
            (missing_account_proof, account_request(), empty_accounts),
            (missing_storage_proof, storage_request(vec![B256::ZERO]), empty_storage.clone()),
            (storage_disappears, storage_request(vec![B256::ZERO, B256::ZERO]), empty_storage),
        ];

        for (provider, request, expected) in cases {
            let handler = snap_handler(provider.clone());
            let (response, rx) = oneshot::channel();
            handler.on_snap_request(PeerId::default(), request, response);
            assert_eq!(rx.await.unwrap(), Ok(expected));
            assert_eq!(provider.snap_state_range_resolutions(), 1);
        }
    }

    #[tokio::test]
    #[allow(clippy::needless_update)]
    async fn snap_account_range_response_encodes_accounts_and_proof() {
        let provider = MockEthProvider::default();
        let first_hash = B256::repeat_byte(0x01);
        let second_hash = B256::repeat_byte(0x02);
        let storage_root = B256::repeat_byte(0x11);
        let code_hash = B256::repeat_byte(0x22);
        let proof = vec![Bytes::from_static(&[0xaa])];
        provider.set_snap_account_range(
            vec![
                (
                    first_hash,
                    Account {
                        nonce: 1,
                        balance: U256::from(2),
                        bytecode_hash: Some(code_hash),
                        ..Default::default()
                    },
                ),
                (second_hash, Account { nonce: 3, balance: U256::from(4), ..Default::default() }),
            ],
            // A hash-limit stop (not an exhausted trie) so a boundary proof is still expected,
            // matching the mocked `proof` below.
            RangeEnd::HashLimit,
        );
        provider.set_snap_storage_root(first_hash, storage_root);
        provider.set_snap_storage_root(second_hash, EMPTY_ROOT_HASH);
        provider.set_snap_account_proof(Some(proof.clone()));

        let mut full_body = vec![0xf8, 0x44, 0x01, 0x02, 0xa0];
        full_body.extend_from_slice(storage_root.as_slice());
        full_body.push(0xa0);
        full_body.extend_from_slice(code_hash.as_slice());
        let empty_body = Bytes::from_static(&[0xc4, 0x03, 0x04, 0x80, 0x80]);

        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
                request_id: 1,
                root_hash: B256::ZERO,
                starting_hash: B256::ZERO,
                limit_hash: B256::repeat_byte(0xff),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::AccountRange(AccountRangeMessage {
                request_id: 1,
                accounts: vec![
                    AccountData {
                        hash: first_hash,
                        body: alloy_rlp::decode_exact(&full_body).unwrap()
                    },
                    AccountData {
                        hash: second_hash,
                        body: alloy_rlp::decode_exact(&empty_body).unwrap()
                    },
                ],
                proof,
            }))
        );
        assert_eq!(provider.snap_state_range_resolutions(), 1);
    }

    #[tokio::test]
    async fn snap_account_range_response_skips_proof_for_exhausted_zero_origin_range() {
        let provider = MockEthProvider::default();
        let hash = B256::repeat_byte(0x01);
        provider.set_snap_account_range(
            vec![(hash, Account { nonce: 1, balance: U256::from(2), ..Default::default() })],
            RangeEnd::Exhausted,
        );
        provider.set_snap_storage_root(hash, EMPTY_ROOT_HASH);
        // A non-empty mocked proof: if the skip-proof optimization regressed to "never skip",
        // this wrongly-included proof would surface in the response and fail the assertion below.
        provider.set_snap_account_proof(Some(vec![Bytes::from_static(&[0xaa])]));

        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
                request_id: 1,
                root_hash: B256::ZERO,
                starting_hash: B256::ZERO,
                limit_hash: B256::repeat_byte(0xff),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        let Ok(SnapResponse::AccountRange(AccountRangeMessage { accounts, proof, .. })) =
            rx.await.unwrap()
        else {
            panic!("expected an account range response");
        };
        assert_eq!(accounts.len(), 1);
        assert!(proof.is_empty(), "a zero-origin, exhausted range must not carry a boundary proof");
    }

    #[tokio::test]
    async fn snap_storage_range_response_encodes_values_and_proof() {
        let provider = MockEthProvider::default();
        let first_hash = B256::repeat_byte(0x01);
        let second_hash = B256::repeat_byte(0x02);
        let origin = B256::repeat_byte(0x10);
        let proof = vec![Bytes::from_static(&[0xbb])];
        provider.push_snap_storage_range(
            vec![(first_hash, U256::from(0x0102)), (second_hash, U256::from(0xff))],
            RangeEnd::Exhausted,
        );
        provider.set_snap_storage_proof(Some(proof.clone()));

        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 2,
                root_hash: B256::ZERO,
                account_hashes: vec![B256::repeat_byte(0x03)],
                starting_hash: origin.into(),
                limit_hash: B256::repeat_byte(0xff).into(),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::StorageRanges(StorageRangesMessage {
                request_id: 2,
                slots: vec![vec![
                    StorageData { hash: first_hash, data: Bytes::from_static(&[0x82, 0x01, 0x02]) },
                    StorageData { hash: second_hash, data: Bytes::from_static(&[0x81, 0xff]) },
                ]],
                proof,
            }))
        );
        assert_eq!(provider.snap_storage_range_requests()[0].1, origin);
        assert_eq!(provider.snap_state_range_resolutions(), 1);
    }

    #[tokio::test]
    async fn snap_storage_ranges_only_bound_the_first_account() {
        let provider = MockEthProvider::default();
        provider.push_snap_storage_range(Vec::new(), RangeEnd::Exhausted);
        provider.push_snap_storage_range(Vec::new(), RangeEnd::Exhausted);
        let first_account = B256::repeat_byte(0x01);
        let second_account = B256::repeat_byte(0x02);
        let origin = B256::ZERO;
        let limit = B256::repeat_byte(0x22);

        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 3,
                root_hash: B256::ZERO,
                account_hashes: vec![first_account, second_account],
                starting_hash: origin.into(),
                limit_hash: limit.into(),
                response_bytes: 1_000,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::StorageRanges(StorageRangesMessage {
                request_id: 3,
                slots: vec![Vec::new(), Vec::new()],
                proof: Vec::new(),
            }))
        );
        assert_eq!(
            provider.snap_storage_range_requests(),
            vec![
                (first_account, origin, limit, 1_000),
                (second_account, B256::ZERO, B256::repeat_byte(0xff), 1_000),
            ]
        );
        assert_eq!(provider.snap_state_range_resolutions(), 1);
    }

    #[tokio::test]
    async fn snap_byte_codes_response_preserves_found_code_order() {
        let provider = MockEthProvider::default();
        let code = Bytes::from_static(&[0x60, 0x00]);
        let code_hash = keccak256(&code);
        let later_code = Bytes::from_static(&[0x60, 0x01]);
        let later_code_hash = keccak256(&later_code);
        provider.add_account(
            Address::repeat_byte(0x01),
            ExtendedAccount::new(1, U256::ZERO).with_bytecode(code.clone()),
        );
        provider.add_account(
            Address::repeat_byte(0x02),
            ExtendedAccount::new(1, U256::ZERO).with_bytecode(later_code),
        );

        let handler = snap_handler(provider);
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetByteCodes(reth_eth_wire::snap::GetByteCodesMessage {
                request_id: 3,
                hashes: vec![KECCAK_EMPTY, B256::repeat_byte(0xff), code_hash, later_code_hash],
                response_bytes: 1,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::ByteCodes(ByteCodesMessage {
                request_id: 3,
                codes: vec![Bytes::new(), code],
            }))
        );
    }

    #[tokio::test]
    async fn snap_storage_ranges_limit_account_lookups() {
        let provider = MockEthProvider::default();
        for _ in 0..=MAX_STORAGE_RANGE_ACCOUNTS_SERVE {
            provider.push_snap_storage_range(Vec::new(), RangeEnd::Exhausted);
        }
        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 4,
                root_hash: B256::ZERO,
                account_hashes: vec![B256::ZERO; MAX_STORAGE_RANGE_ACCOUNTS_SERVE + 1],
                starting_hash: B256::ZERO.into(),
                limit_hash: B256::repeat_byte(0xff).into(),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::StorageRanges(StorageRangesMessage {
                request_id: 4,
                slots: vec![Vec::new(); MAX_STORAGE_RANGE_ACCOUNTS_SERVE],
                proof: Vec::new(),
            }))
        );
        assert_eq!(provider.snap_storage_ranges_remaining(), 1);
        assert_eq!(provider.snap_state_range_resolutions(), 1);
    }

    #[tokio::test]
    async fn snap_storage_range_proves_finite_limit_from_zero_origin() {
        let provider = MockEthProvider::default();
        let hash = B256::repeat_byte(0x01);
        let proof = vec![Bytes::from_static(&[0xcc])];
        // More entries exist beyond `limit_hash`, so the cursor stopped at the hash limit
        // rather than exhausting the trie -- a proof is required even though origin is zero.
        provider.push_snap_storage_range(vec![(hash, U256::ONE)], RangeEnd::HashLimit);
        provider.set_snap_storage_proof(Some(proof.clone()));

        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 5,
                root_hash: B256::ZERO,
                account_hashes: vec![B256::repeat_byte(0x03)],
                starting_hash: B256::ZERO.into(),
                limit_hash: B256::repeat_byte(0x20).into(),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::StorageRanges(StorageRangesMessage {
                request_id: 5,
                slots: vec![vec![StorageData { hash, data: Bytes::from_static(&[0x01]) }]],
                proof,
            }))
        );
    }

    #[tokio::test]
    async fn snap_storage_ranges_are_entirely_empty_when_an_account_is_missing() {
        let provider = MockEthProvider::default();
        provider.push_missing_snap_storage_account();
        provider.push_snap_storage_range(
            vec![(B256::repeat_byte(0x01), U256::ONE)],
            RangeEnd::Exhausted,
        );
        let missing_account = B256::repeat_byte(0x01);
        let valid_account = B256::repeat_byte(0x02);

        let handler = snap_handler(provider.clone());
        let (response, rx) = oneshot::channel();
        handler.on_snap_request(
            PeerId::default(),
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 6,
                root_hash: B256::ZERO,
                account_hashes: vec![missing_account, valid_account],
                starting_hash: B256::ZERO.into(),
                limit_hash: B256::repeat_byte(0xff).into(),
                response_bytes: SOFT_RESPONSE_LIMIT as u64,
            }),
            response,
        );

        assert_eq!(
            rx.await.unwrap(),
            Ok(SnapResponse::StorageRanges(StorageRangesMessage {
                request_id: 6,
                slots: Vec::new(),
                proof: Vec::new(),
            }))
        );
        // The valid account's queued range is never consumed: the response bails out at the
        // first missing account instead of skipping it and shifting later positions.
        assert_eq!(provider.snap_storage_ranges_remaining(), 1);
    }

    /// Creates a receipt whose single log carries `data_len` bytes of data.
    fn receipt_with_data(data_len: usize) -> Receipt {
        Receipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 21_000,
            logs: vec![Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(Vec::new(), vec![0u8; data_len].into()),
            }],
        }
    }

    /// Stores the receipts as block `number` and returns the block hash.
    fn insert_receipts_block(
        provider: &MockEthProvider,
        number: u64,
        receipts: Vec<Receipt>,
    ) -> B256 {
        let hash = B256::with_last_byte(number as u8);
        provider.add_header(hash, Header { number, ..Default::default() });
        provider.add_receipts(number, receipts);
        hash
    }

    /// Sends a [`GetReceipts70`] request to the handler and returns the response.
    async fn get_receipts70(
        provider: MockEthProvider,
        first_block_receipt_index: u64,
        block_hashes: Vec<B256>,
    ) -> Receipts70 {
        let handler = snap_handler(provider);
        let (response, rx) = oneshot::channel();
        handler.on_receipts70_request(
            PeerId::default(),
            GetReceipts70 { first_block_receipt_index, block_hashes },
            response,
        );
        rx.await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn receipts70_serves_only_oversized_first_receipt_of_partial_block() {
        let provider = MockEthProvider::default();
        let oversized = receipt_with_data(SOFT_RESPONSE_LIMIT + 1024);
        let hash =
            insert_receipts_block(&provider, 1, vec![oversized.clone(), receipt_with_data(16)]);

        let resp = get_receipts70(provider, 0, vec![hash]).await;

        // The oversized receipt exhausts the budget, so the rest of the block is left out.
        assert_eq!(
            resp,
            Receipts70 { last_block_incomplete: true, receipts: vec![vec![oversized]] }
        );
    }

    #[tokio::test]
    async fn receipts70_omits_empty_incomplete_block() {
        let provider = MockEthProvider::default();
        let first = vec![receipt_with_data(SOFT_RESPONSE_LIMIT - 1024)];
        let first_hash = insert_receipts_block(&provider, 1, first.clone());
        // The first receipt of the second block does not fit into the remaining budget.
        let second_hash = insert_receipts_block(&provider, 2, vec![receipt_with_data(4096)]);

        let resp = get_receipts70(provider, 0, vec![first_hash, second_hash]).await;

        assert_eq!(resp, Receipts70 { last_block_incomplete: false, receipts: vec![first] });
    }

    #[tokio::test]
    async fn receipts70_serves_partial_block() {
        let provider = MockEthProvider::default();
        let receipts = vec![receipt_with_data(16 * 1024); 200];
        let hash = insert_receipts_block(&provider, 1, receipts.clone());

        let resp = get_receipts70(provider, 0, vec![hash]).await;

        assert!(resp.last_block_incomplete);
        assert_eq!(resp.receipts.len(), 1);
        let partial = &resp.receipts[0];
        assert!(!partial.is_empty() && partial.len() < receipts.len());
        assert_eq!(partial.as_slice(), &receipts[..partial.len()]);
        assert!(partial.iter().map(Encodable::length).sum::<usize>() <= SOFT_RESPONSE_LIMIT);
    }

    #[test_case(0, 0; "first receipt at hard limit")]
    #[test_case(1, 0; "continuation receipt at hard limit")]
    #[test_case(0, 1; "first receipt framing exceeds hard limit")]
    #[test_case(1, 1; "continuation receipt framing exceeds hard limit")]
    #[tokio::test]
    async fn receipts70_enforces_framed_response_limit(first_receipt_index: u64, excess: usize) {
        // Measure the actual wire encoding with the largest request ID. At these sizes, changing
        // the log data length does not change the width of any RLP length prefix.
        let data_len = MAX_MESSAGE_SIZE - 1024;
        let encoded = encode_receipts70_response(Receipts70 {
            last_block_incomplete: false,
            receipts: vec![vec![receipt_with_data(data_len)]],
        });
        let receipt = receipt_with_data(data_len + MAX_MESSAGE_SIZE - encoded.len() + excess);
        assert!(receipt.length() < MAX_MESSAGE_SIZE);
        let expected =
            Receipts70 { last_block_incomplete: false, receipts: vec![vec![receipt.clone()]] };
        assert_eq!(encode_receipts70_response(expected.clone()).len(), MAX_MESSAGE_SIZE + excess);

        let provider = MockEthProvider::default();
        let mut receipts = vec![receipt_with_data(16); first_receipt_index as usize];
        receipts.push(receipt);
        let hash = insert_receipts_block(&provider, 1, receipts);

        let resp = get_receipts70(provider, first_receipt_index, vec![hash]).await;

        if excess == 0 {
            assert_eq!(resp, expected);
        } else {
            assert_eq!(resp, Receipts70 { last_block_incomplete: false, receipts: Vec::new() });
        }
        let encoded = encode_receipts70_response(resp);
        assert!(encoded.len() <= MAX_MESSAGE_SIZE);
        EthStreamInner::<EthNetworkPrimitives>::new(EthVersion::Eth70)
            .decode_message(encoded.as_ref().into())
            .unwrap();
    }

    /// Encodes a receipts response with the largest possible request ID.
    fn encode_receipts70_response(response: Receipts70) -> Bytes {
        EthStreamInner::<EthNetworkPrimitives>::new(EthVersion::Eth70)
            .encode_message(EthMessage::Receipts70(RequestPair {
                request_id: u64::MAX,
                message: response,
            }))
            .unwrap()
            .into()
    }

    #[derive(Debug, Default)]
    struct HeaderChain {
        headers: B256Map<Header>,
        canonical: RefCell<BTreeMap<u64, B256>>,
        replacement: BTreeMap<u64, B256>,
        switch_after_hash_read: Option<usize>,
        hash_reads: cell::Cell<usize>,
        header_reads: cell::Cell<usize>,
    }

    impl HeaderChain {
        fn insert(&mut self, header: Header, canonical: bool) -> B256 {
            let hash = header.hash_slow();
            if canonical {
                self.canonical.get_mut().insert(header.number, hash);
            }
            self.headers.insert(hash, header);
            hash
        }

        fn branch(&mut self, parent: B256, start: u64, count: u64, canonical: bool) -> Vec<B256> {
            let mut parent_hash = parent;
            (start..start + count)
                .map(|number| {
                    parent_hash = self.insert(
                        Header {
                            number,
                            parent_hash,
                            extra_data: vec![u8::from(canonical)].into(),
                            ..Default::default()
                        },
                        canonical,
                    );
                    parent_hash
                })
                .collect()
        }

        fn handler(self) -> EthRequestHandler<Self, EthNetworkPrimitives> {
            let (peers, _) = mpsc::unbounded_channel();
            let (_, incoming) = mpsc::channel(1);
            EthRequestHandler::new(self, PeersHandle::new(peers), incoming)
        }
    }

    impl BlockHashReader for HeaderChain {
        fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
            let hash = self.canonical.borrow().get(&number).copied();
            self.hash_reads.set(self.hash_reads.get() + 1);
            if self.switch_after_hash_read == Some(self.hash_reads.get()) {
                *self.canonical.borrow_mut() = self.replacement.clone();
            }
            Ok(hash)
        }

        fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
            Ok(self.canonical.borrow().range(start..end).map(|(_, hash)| *hash).collect())
        }
    }

    impl HeaderProvider for HeaderChain {
        type Header = Header;

        fn header(&self, hash: B256) -> ProviderResult<Option<Header>> {
            self.header_reads.set(self.header_reads.get() + 1);
            Ok(self.headers.get(&hash).cloned())
        }

        fn header_by_number(&self, number: u64) -> ProviderResult<Option<Header>> {
            Ok(self.block_hash(number)?.and_then(|hash| self.headers.get(&hash).cloned()))
        }

        fn headers_range(&self, range: impl RangeBounds<u64>) -> ProviderResult<Vec<Header>> {
            Ok(self
                .canonical
                .borrow()
                .iter()
                .filter(|(number, _)| range.contains(number))
                .filter_map(|(_, hash)| self.headers.get(hash).cloned())
                .collect())
        }

        fn sealed_header(&self, number: u64) -> ProviderResult<Option<SealedHeader<Header>>> {
            Ok(self.header_by_number(number)?.map(SealedHeader::seal_slow))
        }

        fn sealed_headers_while(
            &self,
            range: impl RangeBounds<u64>,
            mut predicate: impl FnMut(&SealedHeader<Header>) -> bool,
        ) -> ProviderResult<Vec<SealedHeader<Header>>> {
            Ok(self
                .headers_range(range)?
                .into_iter()
                .map(SealedHeader::seal_slow)
                .take_while(|header| predicate(header))
                .collect())
        }
    }

    #[test]
    fn headers_follow_side_branch_parents_and_reject_unrelated_descendants() {
        let mut chain = HeaderChain::default();
        let genesis = chain.insert(Header::default(), true);
        let canonical = chain.branch(genesis, 1, 5, true);
        let side = chain.branch(genesis, 1, 5, false);
        let handler = chain.handler();
        for (start, skip, direction, expected) in [
            (side[4], 1, HeadersDirection::Falling, vec![side[4], side[2], side[0]]),
            (side[1], 1, HeadersDirection::Falling, vec![side[1], genesis]),
            (side[1], 0, HeadersDirection::Rising, vec![side[1]]),
            (side[1], 1, HeadersDirection::Rising, vec![side[1]]),
            (
                canonical[0],
                1,
                HeadersDirection::Rising,
                vec![canonical[0], canonical[2], canonical[4]],
            ),
        ] {
            let response = handler.get_headers_response(GetBlockHeaders {
                start_block: start.into(),
                limit: 3,
                skip,
                direction,
            });
            assert_eq!(response.iter().map(Header::hash_slow).collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn headers_number_query_stops_at_reorg_and_shortcut_rechecks_canonical_hash() {
        let mut chain = HeaderChain::default();
        let genesis = chain.insert(Header::default(), true);
        let old = chain.branch(genesis, 1, 4, true);
        let new = chain.branch(genesis, 1, 4, false);
        chain.replacement = new.iter().enumerate().map(|(i, hash)| (i as u64 + 1, *hash)).collect();
        chain.switch_after_hash_read = Some(1);
        let handler = chain.handler();
        for (start, skip, direction, expected) in [
            (1, 0, HeadersDirection::Rising, vec![old[0]]),
            (4, 0, HeadersDirection::Falling, vec![old[3]]),
            (1, 1, HeadersDirection::Rising, vec![old[0], new[2]]),
            (4, 1, HeadersDirection::Falling, vec![old[3], new[1]]),
        ] {
            *handler.client.canonical.borrow_mut() =
                old.iter().enumerate().map(|(i, hash)| (i as u64 + 1, *hash)).collect();
            handler.client.hash_reads.set(0);
            let response = handler.get_headers_response(GetBlockHeaders {
                start_block: start.into(),
                limit: 4,
                skip,
                direction,
            });
            assert_eq!(response.iter().map(Header::hash_slow).collect::<Vec<_>>(), expected);
        }

        *handler.client.canonical.borrow_mut() =
            old.iter().enumerate().map(|(i, hash)| (i as u64 + 1, *hash)).collect();
        handler.client.hash_reads.set(0);
        let mut budget = MAX_NON_CANONICAL_ANCESTORS;
        assert_eq!(handler.header_ancestor(old[3], 4, 3, &mut budget), Some(old[0]));
    }

    #[test]
    fn headers_stop_on_missing_or_wrong_height_and_bound_noncanonical_walks() {
        let mut chain = HeaderChain::default();
        let genesis = chain.insert(Header::default(), true);
        let side = chain.branch(genesis, 1, 102, false);
        let mut handler = chain.handler();
        let request = GetBlockHeaders {
            start_block: side[101].into(),
            limit: u64::MAX,
            skip: 100,
            direction: HeadersDirection::Falling,
        };
        let response = handler.get_headers_response(request);
        assert_eq!(response.len(), 1);
        assert_eq!(handler.client.header_reads.get(), MAX_NON_CANONICAL_ANCESTORS + 1);

        handler.client.headers.remove(&side[100]);
        let request = GetBlockHeaders {
            start_block: side[101].into(),
            limit: 3,
            skip: 0,
            direction: HeadersDirection::Falling,
        };
        assert_eq!(handler.get_headers_response(request.clone()).len(), 1);
        handler.client.headers.insert(side[100], Header { number: 99, ..Default::default() });
        assert_eq!(handler.get_headers_response(request).len(), 1);
        assert!(handler.exact_header(side[101], Some(101)).is_none());
    }

    #[test]
    fn headers_handle_limits_overflow_and_large_canonical_skips() {
        let mut chain = HeaderChain::default();
        let genesis = chain.insert(Header::default(), true);
        let max = chain.insert(Header { number: u64::MAX, ..Default::default() }, true);
        let handler = chain.handler();
        for (hash, limit, skip, direction, expected) in [
            (max, 0, 0, HeadersDirection::Rising, vec![]),
            (max, 2, 1, HeadersDirection::Rising, vec![max]),
            (genesis, 2, u32::MAX, HeadersDirection::Falling, vec![genesis]),
        ] {
            let response = handler.get_headers_response(GetBlockHeaders {
                start_block: hash.into(),
                limit,
                skip,
                direction,
            });
            assert_eq!(response.iter().map(Header::hash_slow).collect::<Vec<_>>(), expected);
        }
        let mut budget = MAX_NON_CANONICAL_ANCESTORS;
        assert_eq!(handler.header_ancestor(max, u64::MAX, u64::MAX, &mut budget), Some(genesis));
        assert_eq!(budget, MAX_NON_CANONICAL_ANCESTORS);
    }

    #[test]
    fn headers_reject_another_branch_returned_for_an_exact_hash() {
        let mut chain = HeaderChain::default();
        let genesis = chain.insert(Header::default(), true);
        let canonical = chain.branch(genesis, 1, 4, true);
        let side = chain.branch(genesis, 1, 4, false);
        // Model a hash-to-height provider resolving the other branch's header at that height.
        chain.headers.insert(canonical[1], chain.headers[&side[1]].clone());
        chain.headers.insert(side[3], chain.headers[&canonical[3]].clone());
        let handler = chain.handler();
        let response = handler.get_headers_response(GetBlockHeaders {
            start_block: canonical[1].into(),
            limit: 1,
            skip: 0,
            direction: HeadersDirection::Rising,
        });
        assert!(response.is_empty());
        let response = handler.get_headers_response(GetBlockHeaders {
            start_block: canonical[0].into(),
            limit: 2,
            skip: 0,
            direction: HeadersDirection::Rising,
        });
        assert_eq!(response.iter().map(Header::hash_slow).collect::<Vec<_>>(), vec![canonical[0]]);
        let mut budget = MAX_NON_CANONICAL_ANCESTORS;
        assert_eq!(handler.header_ancestor(canonical[1], 2, 1, &mut budget), None);
        assert_eq!(handler.header_ancestor(side[3], 4, 2, &mut budget), None);
    }
}
