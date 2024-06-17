#![allow(missing_docs)]

use alloy_eips::eip4844::{Blob, BlobTransactionSidecar, Bytes48};
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable, RlpDecodableWrapper, RlpEncodableWrapper};
use bytes::BufMut;
use reth_codecs::{derive_arbitrary, Compact};
use revm_primitives::U256;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

#[derive_arbitrary]
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    RlpEncodableWrapper,
    RlpDecodableWrapper,
)]
pub struct BlobSidecars(Vec<BlobSidecar>);

impl BlobSidecars {
    /// Create a new `BlobSidecars` instance.
    pub fn new(sidecars: Vec<BlobSidecar>) -> Self {
        Self(sidecars)
    }

    /// Calculate the total size, including capacity, of the `BlobSidecars`.
    #[inline]
    pub fn total_size(&self) -> usize {
        self.capacity() * std::mem::size_of::<BlobSidecar>()
    }

    /// Calculate a heuristic for the in-memory size of the [`BlobSidecars`].
    #[inline]
    pub fn size(&self) -> usize {
        self.len() * std::mem::size_of::<BlobSidecar>()
    }

    /// Get an iterator over the `BlobSidecars`.
    pub fn iter(&self) -> std::slice::Iter<'_, BlobSidecar> {
        self.0.iter()
    }

    /// Get a mutable iterator over the `BlobSidecars`.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, BlobSidecar> {
        self.0.iter_mut()
    }

    /// Convert [Self] into raw vec of `sidecars`.
    pub fn into_inner(self) -> Vec<BlobSidecar> {
        self.0
    }
}

impl IntoIterator for BlobSidecars {
    type Item = BlobSidecar;
    type IntoIter = std::vec::IntoIter<BlobSidecar>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl AsRef<[BlobSidecar]> for BlobSidecars {
    fn as_ref(&self) -> &[BlobSidecar] {
        &self.0
    }
}

impl Deref for BlobSidecars {
    type Target = Vec<BlobSidecar>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BlobSidecars {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<BlobSidecar>> for BlobSidecars {
    fn from(sidecars: Vec<BlobSidecar>) -> Self {
        Self(sidecars)
    }
}

#[derive_arbitrary]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlobSidecar {
    pub blob_transaction_sidecar: BlobTransactionSidecar,
    pub block_number: U256,
    pub block_hash: B256,
    pub tx_index: u64,
    pub tx_hash: B256,
}

impl BlobSidecars {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `EncodeIndex` encodes the i-th `BlobTransactionSidecar` to out. Note that this does not
    /// check for errors because we assume that `BlobSidecars` will only ever contain valid
    /// sidecars
    pub fn encode_index(&self, out: &mut dyn BufMut, index: usize) {
        let header = alloy_rlp::Header { list: true, payload_length: self.0[index].length() };
        header.encode(out);
        self.0[index].encode(out);
    }
}

impl Encodable for BlobSidecar {
    fn encode(&self, out: &mut dyn BufMut) {
        let list_header_self = alloy_rlp::Header { list: true, payload_length: self.length() };
        list_header_self.encode(out);

        let list_header_tx_sidecar = alloy_rlp::Header {
            list: true,
            payload_length: self.blob_transaction_sidecar.length(),
        };
        list_header_tx_sidecar.encode(out);

        self.blob_transaction_sidecar.encode(out);
        self.block_number.encode(out);
        self.block_hash.encode(out);
        self.tx_index.encode(out);
        self.tx_hash.encode(out);
    }

    fn length(&self) -> usize {
        self.blob_transaction_sidecar.length() +
            self.blob_transaction_sidecar.length().length() +
            self.block_number.length() +
            self.block_hash.length() +
            self.tx_index.length() +
            self.tx_hash.length()
    }
}

impl Decodable for BlobSidecar {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let _rlp_head_self = alloy_rlp::Header::decode(buf)?;
        let _rlp_head_tx_sidecar = alloy_rlp::Header::decode(buf)?;

        let this = Self {
            blob_transaction_sidecar: BlobTransactionSidecar {
                blobs: Decodable::decode(buf)?,
                commitments: Decodable::decode(buf)?,
                proofs: Decodable::decode(buf)?,
            },
            block_number: Decodable::decode(buf)?,
            block_hash: Decodable::decode(buf)?,
            tx_index: Decodable::decode(buf)?,
            tx_hash: Decodable::decode(buf)?,
        };

        Ok(this)
    }
}

impl Compact for BlobSidecar {
    fn to_compact<B>(self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut size = 0;
        size += self.blob_transaction_sidecar.blobs.to_compact(buf);
        size += self.blob_transaction_sidecar.commitments.to_compact(buf);
        size += self.blob_transaction_sidecar.proofs.to_compact(buf);
        size += self.block_number.to_compact(buf);
        size += self.block_hash.to_compact(buf);
        size += self.tx_index.to_compact(buf);
        size += self.tx_hash.to_compact(buf);
        size
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (blobs, buf) = Vec::<Blob>::from_compact(buf, len);
        let (commitments, buf) = Vec::<Bytes48>::from_compact(buf, len);
        let (proofs, buf) = Vec::<Bytes48>::from_compact(buf, len);

        let blob_transaction_sidecar = BlobTransactionSidecar { blobs, commitments, proofs };

        let (block_number, buf) = U256::from_compact(buf, len);
        let (block_hash, buf) = B256::from_compact(buf, len);
        let (tx_index, buf) = u64::from_compact(buf, len);
        let (tx_hash, buf) = B256::from_compact(buf, len);

        let blob_sidecar =
            Self { blob_transaction_sidecar, block_number, block_hash, tx_index, tx_hash };

        (blob_sidecar, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::U256;
    use alloy_rlp::Decodable;

    #[test]
    fn rlp_encode_blob_sidecar() {
        let blob_sidecar = BlobSidecar {
            blob_transaction_sidecar: BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![],
                proofs: vec![],
            },
            block_number: U256::from(rand::random::<u64>()),
            block_hash: B256::random(),
            tx_index: rand::random::<u64>(),
            tx_hash: B256::random(),
        };

        let mut encoded = vec![];
        blob_sidecar.encode(&mut encoded);

        let decoded = BlobSidecar::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(blob_sidecar, decoded);
    }
}
