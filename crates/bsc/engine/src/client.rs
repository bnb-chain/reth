//! This includes download client implementations for parlia consensus.
use std::fmt::Debug;

use alloy_primitives::B256;
use reth_network_p2p::{
    bodies::client::{BodiesClient, BodiesFut},
    download::DownloadClient,
    headers::client::{HeadersClient, HeadersDirection, HeadersFut, HeadersRequest},
    priority::Priority,
    BlockClient,
};
use reth_network_peers::{PeerId, WithPeerId};
use reth_primitives::{BlockBody, Header, SealedHeader};
use tracing::trace;

use crate::Storage;

#[derive(Debug, Clone)]
enum InnerFetchError {
    HeaderNotFound,
    BodyNotFound,
}

type InnerFetchHeaderResult = Result<Vec<Header>, InnerFetchError>;
type InnerFetchBodyResult = Result<Vec<BlockBody>, InnerFetchError>;

/// A client for fetching headers and bodies from the network.
/// This client will first try to fetch from the local storage, and if the data is not found, it
/// will fetch from the network.
#[derive(Debug, Clone)]
pub struct ParliaClient<Client> {
    /// cached header and body
    storage: Storage,
    fetch_client: Client,
    peer_id: PeerId,
}

impl<Client> ParliaClient<Client>
where
    Client: BlockClient + 'static,
{
    pub(crate) fn new(storage: Storage, fetch_client: Client) -> Self {
        let peer_id = PeerId::random();
        Self { storage, fetch_client, peer_id }
    }

    async fn fetch_headers(&self, request: HeadersRequest) -> InnerFetchHeaderResult {
        trace!(target: "consensus::parlia", ?request, "received headers request");

        let storage = self.storage.read().await;
        let HeadersRequest { start, limit, direction } = request;
        let mut headers = Vec::<SealedHeader>::new();
        let mut block = start;

        for _ in 0..limit {
            // fetch from storage
            if let Some(header) = storage.header_by_hash_or_number(block) {
                match direction {
                    HeadersDirection::Falling => block = header.parent_hash.into(),
                    HeadersDirection::Rising => {
                        if !headers.is_empty() &&
                            headers.last().cloned().unwrap().hash() != header.parent_hash
                        {
                            return Err(InnerFetchError::HeaderNotFound);
                        }
                        let next = header.number + 1;
                        block = next.into()
                    }
                }

                headers.push(header.clone());
            } else {
                return Err(InnerFetchError::HeaderNotFound);
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

    async fn clean_cache(&self) {
        let mut storage = self.storage.write().await;
        storage.clean_caches()
    }
}

impl<Client> HeadersClient for ParliaClient<Client>
where
    Client: BlockClient + 'static,
{
    type Output = HeadersFut;

    fn get_headers_with_priority(
        &self,
        request: HeadersRequest,
        priority: Priority,
    ) -> Self::Output {
        let this = self.clone();
        let peer_id = self.peer_id;
        Box::pin(async move {
            let result = this.fetch_headers(request.clone()).await;
            if let Ok(headers) = result {
                return Ok(WithPeerId::new(peer_id, headers));
            }
            this.fetch_client.get_headers_with_priority(request.clone(), priority).await
        })
    }
}

impl<Client> BodiesClient for ParliaClient<Client>
where
    Client: BlockClient + 'static,
{
    type Output = BodiesFut;

    fn get_block_bodies_with_priority(
        &self,
        hashes: Vec<B256>,
        priority: Priority,
    ) -> Self::Output {
        let this = self.clone();
        let peer_id = self.peer_id;
        Box::pin(async move {
            let result = this.fetch_bodies(hashes.clone()).await;
            if let Ok(blocks) = result {
                return Ok(WithPeerId::new(peer_id, blocks));
            }
            this.fetch_client.get_block_bodies_with_priority(hashes.clone(), priority).await
        })
    }
}

impl<Client> DownloadClient for ParliaClient<Client>
where
    Client: BlockClient + 'static,
{
    fn report_bad_message(&self, peer_id: PeerId) {
        let this = self.clone();
        if peer_id == self.peer_id {
            tokio::spawn(async move {
                this.clean_cache().await;
            });
        } else {
            this.fetch_client.report_bad_message(peer_id)
        }
    }

    fn num_connected_peers(&self) -> usize {
        let this = self.clone();
        this.fetch_client.num_connected_peers()
    }
}
