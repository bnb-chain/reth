use crate::{
    models::parlia::{VoteAddress, VoteAttestation, VoteData},
    table::{Compress, Decompress},
    DatabaseError,
};
use bytes::BufMut;
use reth_codecs::{main_codec, Compact};
use reth_primitives::{Address, BlockNumber, Header, B256};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
};

/// Number of blocks after which to save the snapshot to the database
pub const CHECKPOINT_INTERVAL: u64 = 1024;

/// record validators information
#[main_codec]
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct ValidatorInfo {
    /// The index of the validator
    /// The index should offset by 1
    pub index: u64,
    /// The vote address of the validator
    pub vote_addr: VoteAddress,
}

/// Snapshot, record validators and proposal from epoch chg.
#[derive(Debug, Default, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// record current epoch number
    pub epoch_num: u64,
    /// record block number when epoch chg
    pub block_number: BlockNumber,
    /// record block hash when epoch chg
    pub block_hash: B256,
    /// record epoch validators when epoch chg, sorted by ascending order.
    pub validators: Vec<Address>,
    /// record every validator's information
    pub validators_map: HashMap<Address, ValidatorInfo>,
    /// record recent block proposers
    pub recent_proposers: BTreeMap<BlockNumber, Address>,
    /// record the block attestation's vote data
    pub vote_data: VoteData,
}

impl Snapshot {
    /// Create a new snapshot
    pub fn new(
        mut validators: Vec<Address>,
        block_number: BlockNumber,
        block_hash: B256,
        epoch_num: u64,
        val_info_map: Option<HashMap<Address, ValidatorInfo>>,
    ) -> Self {
        // notice: the validators should be sorted by ascending order.
        validators.sort();
        Self {
            block_number,
            block_hash,
            epoch_num,
            validators,
            validators_map: val_info_map.unwrap_or_default(),
            recent_proposers: Default::default(),
            vote_data: Default::default(),
        }
    }

    /// Apply the next block to the snapshot
    pub fn apply(
        &mut self,
        validator: Address,
        next_header: &Header,
        mut next_validators: Vec<Address>,
        val_info_map: Option<HashMap<Address, ValidatorInfo>>,
        attestation: Option<VoteAttestation>,
    ) -> Option<Snapshot> {
        let block_number = next_header.number;
        if self.block_number + 1 != block_number {
            return None;
        }

        let mut snap = self.clone();
        snap.block_hash = next_header.hash_slow();
        snap.block_number = block_number;
        let limit = (snap.validators.len() / 2 + 1) as u64;
        if block_number >= limit {
            snap.recent_proposers.remove(&(block_number - limit));
        }

        if !snap.validators.contains(&validator) {
            return None;
        }
        if snap.recent_proposers.iter().any(|(_, &addr)| addr == validator) {
            return None;
        }
        snap.recent_proposers.insert(block_number, validator);

        if !next_validators.is_empty() {
            next_validators.sort();
            snap.validators = next_validators;
            snap.validators_map = val_info_map.unwrap_or_default();
        }

        if let Some(attestation) = attestation {
            snap.vote_data = attestation.data;
        }
        Some(snap)
    }

    /// Returns true if the block difficulty should be inturn
    pub fn is_inturn(&self, proposer: Address) -> bool {
        self.inturn_validator() == proposer
    }

    /// Returns the validator who should propose the block
    pub fn inturn_validator(&self) -> Address {
        self.validators[((self.block_number + 1) as usize) % self.validators.len()]
    }

    /// Return index of the validator's index in validators list
    pub fn index_of(&self, validator: Address) -> Option<usize> {
        for (i, &addr) in self.validators.iter().enumerate() {
            if validator == addr {
                return Some(i);
            }
        }
        None
    }

    /// Returns true if the validator has signed a block in the last limit blocks
    pub fn sign_recently(&self, validator: Address) -> bool {
        for (num, addr) in self.recent_proposers.iter() {
            if *addr == validator {
                let limit = (self.validators.len() / 2 + 1) as u64;
                if self.block_number + 1 < limit || *num > self.block_number + 1 - limit {
                    return true;
                }
            }
        }
        false
    }
}

impl Compress for Snapshot {
    type Compressed = Vec<u8>;

    fn compress(self) -> Self::Compressed {
        serde_cbor::to_vec(&self).expect("Failed to serialize Snapshot")
    }

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(self, buf: &mut B) {
        let compressed = self.compress();
        buf.put_slice(&compressed);
    }
}

impl Decompress for Snapshot {
    fn decompress<B: AsRef<[u8]>>(value: B) -> Result<Self, DatabaseError> {
        serde_cbor::from_slice(value.as_ref()).map_err(|_| DatabaseError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    #[test]
    fn compress_snapshot() {
        let mut rng = rand::thread_rng();

        let mut snap = Snapshot {
            epoch_num: rng.gen::<u64>(),
            block_number: rng.gen::<u64>(),
            block_hash: B256::random(),
            validators: vec![Address::random()],
            validators_map: HashMap::new(),
            recent_proposers: BTreeMap::new(),
            vote_data: VoteData::default(),
        };
        snap.validators_map.insert(
            snap.validators[0],
            ValidatorInfo { index: 1, vote_addr: VoteAddress::random() },
        );
        snap.recent_proposers.insert(1, snap.validators[0]);
        snap.vote_data = VoteData {
            source_number: rng.gen::<u64>(),
            source_hash: B256::random(),
            target_number: rng.gen::<u64>(),
            target_hash: B256::random(),
        };
        println!("original snapshot: {:?}", snap);

        let compressed = snap.clone().compress();
        println!("compressed snapshot: {:?}", compressed);

        let decompressed = Snapshot::decompress(&compressed).unwrap();
        println!("decompressed snapshot: {:?}", decompressed);
        assert_eq!(snap, decompressed);
    }
}
