use std::collections::{BTreeMap, HashMap};

use crate::{
    parlia::{VoteAddress, VoteAttestation, VoteData},
    Header,
};
use alloy_primitives::{Address, BlockNumber, B256};
#[cfg(any(test, feature = "reth-codec"))]
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

/// Number of blocks after which to save the snapshot to the database
pub const CHECKPOINT_INTERVAL: u64 = 1024;
/// Default turn length before Bohr upgrade
pub const DEFAULT_TURN_LENGTH: u8 = 1;

/// record validators information
#[derive(Debug, Default, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(any(test, feature = "reth-codec"), derive(Compact))]
#[cfg_attr(any(test, feature = "reth-codec"), reth_codecs::add_arbitrary_tests(compact))]
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
    /// record length of `turn`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_length: Option<u8>,
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
            turn_length: Some(DEFAULT_TURN_LENGTH),
        }
    }

    /// Apply the next block to the snapshot
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &self,
        validator: Address,
        next_header: &Header,
        mut new_validators: Vec<Address>,
        vote_addrs: Option<Vec<VoteAddress>>,
        attestation: Option<VoteAttestation>,
        turn_length: Option<u8>,
        is_bohr: bool,
    ) -> Option<Self> {
        let block_number = next_header.number;
        if self.block_number + 1 != block_number {
            return None;
        }

        let mut snap = self.clone();
        snap.block_hash = next_header.hash_slow();
        snap.block_number = block_number;
        let limit = self.miner_history_check_len() + 1;
        if block_number >= limit {
            snap.recent_proposers.remove(&(block_number - limit));
        }

        if !snap.validators.contains(&validator) {
            return None;
        }
        if snap.sign_recently(validator) {
            return None;
        }
        snap.recent_proposers.insert(block_number, validator);

        let epoch_key = u64::MAX - next_header.number / snap.epoch_num;
        if !new_validators.is_empty() &&
            (!is_bohr || !snap.recent_proposers.contains_key(&epoch_key))
        {
            new_validators.sort();
            if let Some(turn_length) = turn_length {
                snap.turn_length = Some(turn_length);
            }

            if is_bohr {
                snap.recent_proposers = Default::default();
                snap.recent_proposers.insert(epoch_key, Address::default());
            } else {
                let new_limit = (new_validators.len() / 2 + 1) as u64;
                if new_limit < limit {
                    for i in 0..(limit - new_limit) {
                        snap.recent_proposers.remove(&(block_number - new_limit - i));
                    }
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

    /// Returns the number of blocks after which the miner history should be checked
    pub fn miner_history_check_len(&self) -> u64 {
        let turn_length = u64::from(self.turn_length.unwrap_or(DEFAULT_TURN_LENGTH));
        (self.validators.len() / 2 + 1) as u64 * turn_length - 1
    }

    /// Returns the validator who should propose the block
    pub fn inturn_validator(&self) -> Address {
        let turn_length = u64::from(self.turn_length.unwrap_or(DEFAULT_TURN_LENGTH));
        self.validators
            [((self.block_number + 1) as usize) / turn_length as usize % self.validators.len()]
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

    /// Returns the map of the number of times each validator has signed a block in the recent
    /// blocks
    pub fn count_recent_proposers(&self) -> HashMap<Address, u8> {
        let left_history_bound = if self.block_number > self.miner_history_check_len() {
            self.block_number - self.miner_history_check_len()
        } else {
            0
        };

        let mut counts = HashMap::new();
        for (&seen, &recent) in &self.recent_proposers {
            if seen <= left_history_bound || recent == Address::default() {
                continue;
            }
            *counts.entry(recent).or_insert(0) += 1;
        }

        counts
    }

    /// Returns true if the validator has signed a block in the last limit blocks
    pub fn sign_recently(&self, validator: Address) -> bool {
        self.sign_recently_by_counts(validator, &self.count_recent_proposers())
    }

    /// Returns true if the validator has signed a block in the recents blocks
    pub fn sign_recently_by_counts(
        &self,
        validator: Address,
        counts: &HashMap<Address, u8>,
    ) -> bool {
        if let Some(&seen_times) = counts.get(&validator) {
            let turn_length = self.turn_length.unwrap_or(DEFAULT_TURN_LENGTH);
            if seen_times >= turn_length {
                return true;
            }
        }
        false
    }
}
