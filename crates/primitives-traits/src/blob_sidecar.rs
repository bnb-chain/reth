#![allow(missing_docs)]

use alloy_eips::eip4844::{Blob, BlobTransactionSidecar, Bytes48, BYTES_PER_BLOB};
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable, RlpDecodableWrapper, RlpEncodableWrapper};
use bytes::{Buf, BufMut};
use derive_more::{Deref, DerefMut, From, IntoIterator};
use reth_codecs::{derive_arbitrary, main_codec, Compact};
use revm_primitives::U256;
use serde::{Deserialize, Serialize};

#[main_codec(no_arbitrary)]
#[derive_arbitrary]
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Default,
    Deref,
    DerefMut,
    From,
    IntoIterator,
    RlpEncodableWrapper,
    RlpDecodableWrapper,
)]
pub struct BlobSidecars(Vec<BlobSidecar>);

impl BlobSidecars {
    /// Create a new `BlobSidecars` instance.
    pub const fn new(sidecars: Vec<BlobSidecar>) -> Self {
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
        let mut len = 0;

        buf.put_u16(self.blob_transaction_sidecar.blobs.len() as u16);
        len += 2;
        for item in self.blob_transaction_sidecar.blobs {
            len += item.to_compact(buf);
        }

        buf.put_u16(self.blob_transaction_sidecar.commitments.len() as u16);
        len += 2;
        for item in self.blob_transaction_sidecar.commitments {
            len += item.to_compact(buf);
        }

        buf.put_u16(self.blob_transaction_sidecar.proofs.len() as u16);
        len += 2;
        for item in self.blob_transaction_sidecar.proofs {
            len += item.to_compact(buf);
        }

        buf.put_slice(self.block_number.as_le_slice());
        len += 32;

        buf.put_slice(self.block_hash.as_slice());
        len += 32;

        buf.put_u64(self.tx_index);
        len += 8;

        buf.put_slice(self.tx_hash.as_slice());
        len += 32;

        len
    }

    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let blobs_len = buf.get_u16() as usize;
        let mut blobs = Vec::with_capacity(blobs_len);
        for _ in 0..blobs_len {
            let (item, rest) = Blob::from_compact(buf, BYTES_PER_BLOB);
            blobs.push(item);
            buf = rest;
        }

        let commitments_len = buf.get_u16() as usize;
        let mut commitments = Vec::with_capacity(commitments_len);
        for _ in 0..commitments_len {
            let (item, rest) = Bytes48::from_compact(buf, 48);
            commitments.push(item);
            buf = rest;
        }

        let proofs_len = buf.get_u16() as usize;
        let mut proofs = Vec::with_capacity(proofs_len);
        for _ in 0..proofs_len {
            let (item, rest) = Bytes48::from_compact(buf, 48);
            proofs.push(item);
            buf = rest;
        }

        let block_number = U256::from_le_slice(&buf[..32]);
        buf = &buf[32..];

        let block_hash = B256::from_slice(&buf[..32]);
        buf = &buf[32..];

        let tx_index = buf.get_u64();

        let tx_hash = B256::from_slice(&buf[..32]);
        buf = &buf[32..];

        let blob_sidecar = Self {
            blob_transaction_sidecar: BlobTransactionSidecar { blobs, commitments, proofs },
            block_number,
            block_hash,
            tx_index,
            tx_hash,
        };

        (blob_sidecar, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::Decodable;

    #[test]
    fn test_blob_sidecar_rlp() {
        let blob_sidecar = BlobSidecar {
            blob_transaction_sidecar: BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![Default::default()],
                proofs: vec![Default::default()],
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

    #[test]
    fn test_blob_sidecar_compact() {
        let blob_sidecar = BlobSidecar {
            blob_transaction_sidecar: BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![Default::default()],
                proofs: vec![Default::default()],
            },
            block_number: U256::from(rand::random::<u64>()),
            block_hash: B256::random(),
            tx_index: rand::random::<u64>(),
            tx_hash: B256::random(),
        };

        let mut buf = vec![];
        let len = blob_sidecar.clone().to_compact(&mut buf);
        let (decoded, _) = BlobSidecar::from_compact(&buf, len);
        assert_eq!(blob_sidecar, decoded);
    }
}
