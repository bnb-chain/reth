use crate::{
    parlia::{VoteAddress, VoteAttestation, VoteData},
    Address, BlockNumber, Header, B256,
};
use reth_codecs::{main_codec, Compact};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
        vote_addrs: Option<Vec<VoteAddress>>,
    ) -> Self {
        // notice: the validators should be sorted by ascending order.
        validators.sort();

        let mut validators_map = HashMap::new();
        if let Some(vote_addrs) = vote_addrs {
            assert_eq!(
                validators.len(),
                vote_addrs.len(),
                "validators and vote_addrs length not equal"
            );

            for (i, v) in validators.iter().enumerate() {
                let val_info = ValidatorInfo { index: i as u64 + 1, vote_addr: vote_addrs[i] };
                validators_map.insert(*v, val_info);
            }
        } else {
            for v in &validators {
                validators_map.insert(*v, Default::default());
            }
        }

        Self {
            block_number,
            block_hash,
            epoch_num,
            validators,
            validators_map,
            recent_proposers: Default::default(),
            vote_data: Default::default(),
        }
    }

    /// Apply the next block to the snapshot
    pub fn apply(
        &self,
        validator: Address,
        next_header: &Header,
        mut new_validators: Vec<Address>,
        vote_addrs: Option<Vec<VoteAddress>>,
        attestation: Option<VoteAttestation>,
    ) -> Option<Self> {
        let block_number = next_header.number;
        if self.block_number + 1 != block_number {
            return None;
        }

        let mut snap = self.clone();
        snap.block_hash = next_header.hash_slow();
        snap.block_number = block_number;
        let limit = (snap.validators.len() / 2 + 1) as u64;
        if block_number >= limit || block_number >= snap.validators.len() as u64 {
            snap.recent_proposers.remove(&(block_number - limit));
        }

        if !snap.validators.contains(&validator) {
            return None;
        }
        if snap.recent_proposers.iter().any(|(_, &addr)| addr == validator) {
            return None;
        }
        snap.recent_proposers.insert(block_number, validator);

        if !new_validators.is_empty() {
            new_validators.sort();

            let new_limit = (new_validators.len() / 2 + 1) as u64;
            if new_limit < limit {
                for i in 0..(limit - new_limit) {
                    snap.recent_proposers.remove(&(block_number - new_limit - i));
                }
            }

            let mut validators_map = HashMap::new();
            if let Some(vote_addrs) = vote_addrs {
                assert_eq!(
                    new_validators.len(),
                    vote_addrs.len(),
                    "validators and vote_addrs length not equal"
                );

                for (i, v) in new_validators.iter().enumerate() {
                    let val_info = ValidatorInfo { index: i as u64 + 1, vote_addr: vote_addrs[i] };
                    validators_map.insert(*v, val_info);
                }
            } else {
                for v in &new_validators {
                    validators_map.insert(*v, Default::default());
                }
            }

            snap.validators = new_validators;
            snap.validators_map = validators_map;
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
        for (&num, &addr) in &self.recent_proposers {
            if addr == validator {
                let limit = (self.validators.len() / 2 + 1) as u64;
                if num > self.block_number.saturating_sub(limit - 1) {
                    return true;
                }
            }
        }
        false
    }
}
