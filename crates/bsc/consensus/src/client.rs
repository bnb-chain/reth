//! This includes download client implementations for auto sealing miners.
use crate::Storage;
use reth_network::FetchClient;
use reth_network_p2p::{
    bodies::client::{BodiesClient, BodiesFut},
    download::DownloadClient,
    headers::client::{HeadersClient, HeadersFut, HeadersRequest},
    priority::Priority,
};
use reth_network_peers::{PeerId, WithPeerId};
use reth_primitives::{BlockBody, BlockHashOrNumber, Header, HeadersDirection, B256, SealedHeader};
use std::fmt::Debug;
use tracing::trace;

#[derive(Debug, Clone)]
enum InnerFetchError {
    HeaderNotFound,
    BodyNotFound,
}

type InnerFetchHeaderResult = Result<Vec<Header>, InnerFetchError>;
type InnerFetchBodyResult = Result<Vec<BlockBody>, InnerFetchError>;

/// A client for fetching headers and bodies from the network.
/// This client will first try to fetch from the local storage, and if the data is not found, it will
/// fetch from the network.
#[derive(Debug, Clone)]
pub struct ParliaClient {
    /// cached header and body
    storage: Storage,
    fetch_client: FetchClient,
}

impl ParliaClient {
    pub(crate) fn new(storage: Storage, fetch_client: FetchClient) -> Self {
        Self { storage, fetch_client }
    }

    async fn fetch_headers(&self, request: HeadersRequest) -> InnerFetchHeaderResult {
        trace!(target: "consensus::parlia", ?request, "received headers request");

        let storage = self.storage.read().await;
        let HeadersRequest { start, limit, direction } = request;
        let mut headers = Vec::<SealedHeader>::new();

        let mut block: BlockHashOrNumber = match start {
            BlockHashOrNumber::Hash(start) => start.into(),
            BlockHashOrNumber::Number(num) => {
                if let Some(hash) = storage.block_hash(num) {
                    hash.into()
                } else {
                    return Err(InnerFetchError::HeaderNotFound);
                }
            }
        };

        for _ in 0..limit {
            // fetch from storage
            if let Some(header) = storage.header_by_hash_or_number(block) {
                match direction {
                    HeadersDirection::Falling => block = header.parent_hash.into(),
                    HeadersDirection::Rising => {
                        if !headers.is_empty() && headers.last().cloned().unwrap().hash() != header.parent_hash {
                            break;
                        }
                        let next = header.number + 1;
                        block = next.into()
                    }
                }
                
                headers.push(header.clone());
            } else {
                break;
            }
        }

        trace!(target: "consensus::parlia", ?headers, "returning headers");

        Ok(headers.into_iter().map(|sealed_header| sealed_header.header().clone()).collect())
    }

    async fn fetch_bodies(&self, hashes: Vec<B256>) -> InnerFetchBodyResult {
        trace!(target: "consensus::parlia", ?hashes, "received bodies request");
        let storage = self.storage.read().await;
        let mut bodies = Vec::new();
        for hash in hashes {
            if let Some(body) = storage.bodies.get(&hash).cloned() {
                bodies.push(body);
            } else {
                return Err(InnerFetchError::BodyNotFound);
            }
        }

        trace!(target: "consensus::parlia", ?bodies, "returning bodies");

        Ok(bodies)
    }
}

impl HeadersClient for ParliaClient {
    type Output = HeadersFut;

    fn get_headers_with_priority(
        &self,
        request: HeadersRequest,
        priority: Priority,
    ) -> Self::Output {
        let this = self.clone();
        Box::pin(async move {
            let result = this.fetch_headers(request.clone()).await;
            if !result.is_err() {
                let headers = result.clone().unwrap();
                if headers.len() as u64 == request.limit {
                    return Ok(WithPeerId::new(PeerId::random(), headers.clone()));
                }
            }
            this.fetch_client.get_headers_with_priority(request.clone(), priority).await
        })
    }
}

impl BodiesClient for ParliaClient {
    type Output = BodiesFut;

    fn get_block_bodies_with_priority(
        &self,
        hashes: Vec<B256>,
        priority: Priority,
    ) -> Self::Output {
        let this = self.clone();
        Box::pin(async move {
            let result = this.fetch_bodies(hashes.clone()).await;
            if !result.is_err() {
                return Ok(WithPeerId::new(PeerId::random(), result.unwrap().clone()));
            }
            this.fetch_client.get_block_bodies_with_priority(hashes.clone(), priority).await
        })
    }
}

impl DownloadClient for ParliaClient {
    fn report_bad_message(&self, peer_id: PeerId) {
        let this = self.clone();
        this.fetch_client.report_bad_message(peer_id)
    }

    fn num_connected_peers(&self) -> usize {
        let this = self.clone();
        this.fetch_client.num_connected_peers()
    }
}
