use std::fmt::Display;

use alloy_primitives::B256;
use bitset::BitSet;
use blst::{
    min_pk::{PublicKey, Signature},
    BLST_ERROR,
};
use reth_bsc_consensus::{DIFF_INTURN, DIFF_NOTURN};
use reth_bsc_forks::BscHardforks;
use reth_errors::{BlockExecutionError, ProviderError};
use reth_ethereum_forks::EthereumHardforks;
use reth_evm::ConfigureEvm;
use reth_primitives::{
    parlia::{Snapshot, VoteAddress, MAX_ATTESTATION_EXTRA_LENGTH},
    GotExpected, Header,
};
use reth_provider::ParliaProvider;
use revm_primitives::db::Database;

use crate::{BscBlockExecutionError, BscBlockExecutor, SnapshotReader};

const BLST_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

impl<EvmConfig, DB, P> BscBlockExecutor<EvmConfig, DB, P>
where
    EvmConfig: ConfigureEvm<Header = Header>,
    DB: Database<Error: Into<ProviderError> + Display>,
    P: ParliaProvider,
{
    /// Apply settings and verify headers before a new block is executed.
    pub(crate) fn on_new_block(
        &mut self,
        header: &Header,
        parent: &Header,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
        snap: &Snapshot,
    ) -> Result<(), BlockExecutionError> {
        // Set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag = self.chain_spec().is_spurious_dragon_active_at_block(header.number);
        self.state.set_state_clear_flag(state_clear_flag);

        self.verify_cascading_fields(header, parent, ancestor, snap)
    }

    fn verify_cascading_fields(
        &self,
        header: &Header,
        parent: &Header,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
        snap: &Snapshot,
    ) -> Result<(), BlockExecutionError> {
        self.verify_block_time_for_ramanujan(snap, header, parent)?;
        self.verify_vote_attestation(snap, header, parent, ancestor)?;
        self.verify_seal(snap, header)?;

        Ok(())
    }

    fn verify_block_time_for_ramanujan(
        &self,
        snapshot: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        if self.chain_spec().is_ramanujan_active_at_block(header.number) &&
            header.timestamp <
                parent.timestamp +
                    self.parlia().period() +
                    self.parlia().back_off_time(snapshot, header)
        {
            return Err(BscBlockExecutionError::FutureBlock {
                block_number: header.number,
                hash: header.hash_slow(),
            }
            .into());
        }

        Ok(())
    }

    fn verify_vote_attestation(
        &self,
        snap: &Snapshot,
        header: &Header,
        parent: &Header,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
    ) -> Result<(), BlockExecutionError> {
        if !self.chain_spec().is_plato_active_at_block(header.number) {
            return Ok(());
        }

        let attestation =
            self.parlia().get_vote_attestation_from_header(header).map_err(|err| {
                BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
            })?;
        if let Some(attestation) = attestation {
            if attestation.extra.len() > MAX_ATTESTATION_EXTRA_LENGTH {
                return Err(BscBlockExecutionError::TooLargeAttestationExtraLen {
                    extra_len: MAX_ATTESTATION_EXTRA_LENGTH,
                }
                .into());
            }

            // the attestation target block should be direct parent.
            let target_block = attestation.data.target_number;
            let target_hash = attestation.data.target_hash;
            if target_block != parent.number || target_hash != header.parent_hash {
                return Err(BscBlockExecutionError::InvalidAttestationTarget {
                    block_number: GotExpected { got: target_block, expected: parent.number },
                    block_hash: GotExpected { got: target_hash, expected: parent.hash_slow() }
                        .into(),
                }
                .into());
            }

            // the attestation source block should be the highest justified block.
            let source_block = attestation.data.source_number;
            let source_hash = attestation.data.source_hash;
            let justified = &(self.get_justified_header(ancestor, snap)?);
            if source_block != justified.number || source_hash != justified.hash_slow() {
                return Err(BscBlockExecutionError::InvalidAttestationSource {
                    block_number: GotExpected { got: source_block, expected: justified.number },
                    block_hash: GotExpected { got: source_hash, expected: justified.hash_slow() }
                        .into(),
                }
                .into());
            }

            // Get the target_number - 1 block's snapshot.
            let pre_target_header = &(self.get_header_by_hash(parent.parent_hash, ancestor)?);
            let snapshot_reader = SnapshotReader::new(self.provider.clone(), self.parlia.clone());
            let snap = &(snapshot_reader.snapshot(pre_target_header, None)?);

            // query bls keys from snapshot.
            let validators_count = snap.validators.len();
            let vote_bit_set = BitSet::from_u64(attestation.vote_address_set);
            let bit_set_count = vote_bit_set.count() as usize;

            if bit_set_count > validators_count {
                return Err(BscBlockExecutionError::InvalidAttestationVoteCount(GotExpected {
                    got: bit_set_count as u64,
                    expected: validators_count as u64,
                })
                .into());
            }
            let mut vote_addrs: Vec<VoteAddress> = Vec::with_capacity(bit_set_count);
            for (i, val) in snap.validators.iter().enumerate() {
                if !vote_bit_set.test(i) {
                    continue;
                }

                let val_info = snap.validators_map.get(val).ok_or_else(|| {
                    BscBlockExecutionError::VoteAddrNotFoundInSnap { address: *val }
                })?;
                vote_addrs.push(val_info.vote_addr);
            }

            // check if voted validator count satisfied 2/3 + 1
            let at_least_votes = (validators_count * 2 + 2) / 3; // ceil division
            if vote_addrs.len() < at_least_votes {
                return Err(BscBlockExecutionError::InvalidAttestationVoteCount(GotExpected {
                    got: vote_addrs.len() as u64,
                    expected: at_least_votes as u64,
                })
                .into());
            }

            // check bls aggregate sig
            let vote_addrs: Vec<PublicKey> = vote_addrs
                .iter()
                .map(|addr| PublicKey::from_bytes(addr.as_slice()).unwrap())
                .collect();
            let vote_addrs_ref: Vec<&PublicKey> = vote_addrs.iter().collect();

            let sig = Signature::from_bytes(&attestation.agg_signature[..])
                .map_err(|_| BscBlockExecutionError::BLSTInnerError)?;
            let err = sig.fast_aggregate_verify(
                true,
                attestation.data.hash().as_slice(),
                BLST_DST,
                &vote_addrs_ref,
            );

            return match err {
                BLST_ERROR::BLST_SUCCESS => Ok(()),
                _ => Err(BscBlockExecutionError::BLSTInnerError.into()),
            };
        }

        Ok(())
    }

    fn verify_seal(&self, snap: &Snapshot, header: &Header) -> Result<(), BlockExecutionError> {
        let block_number = header.number;
        let proposer = self.parlia().recover_proposer(header).map_err(|err| {
            BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
        })?;

        if proposer != header.beneficiary {
            return Err(BscBlockExecutionError::WrongHeaderSigner {
                block_number,
                signer: GotExpected { got: proposer, expected: header.beneficiary }.into(),
            }
            .into());
        }

        if !snap.validators.contains(&proposer) {
            return Err(BscBlockExecutionError::SignerUnauthorized { block_number, proposer }.into());
        }

        if snap.sign_recently(proposer) {
            return Err(BscBlockExecutionError::SignerOverLimit { proposer }.into());
        }

        let is_inturn = snap.is_inturn(proposer);
        if (is_inturn && header.difficulty != DIFF_INTURN) ||
            (!is_inturn && header.difficulty != DIFF_NOTURN)
        {
            return Err(
                BscBlockExecutionError::InvalidDifficulty { difficulty: header.difficulty }.into()
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{b256, hex};
    use blst::min_pk::{PublicKey, Signature};
    use reth_primitives::parlia::{VoteAddress, VoteData, VoteSignature};

    use super::BLST_DST;

    #[test]
    fn verify_vote_attestation() {
        let vote_data = VoteData {
            source_number: 1,
            source_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            target_number: 2,
            target_hash: b256!("0000000000000000000000000000000000000000000000000000000000000002"),
        };

        let vote_addrs = vec![
            VoteAddress::from_slice(hex::decode("0x92134f208bc32515409e3e91e89691e2800724d6b15e667cfe11652c2daf77d3494b5d216e2ce5794cc253a6395f707d").unwrap().as_slice()),
            VoteAddress::from_slice(hex::decode("0xb0c7b88a54614ec9a5d5ab487db071464364a599900928a10fb1237b44478412583ea062e6d03fd0a8334f539ded9302").unwrap().as_slice()),
            VoteAddress::from_slice(hex::decode("0xb3d050e2cd6ce18fb45939d3406ae5904d1bbbdca1e72a73307a8c038af0e0d382c1614724cd1fe0dabcff82f3ff7d91").unwrap().as_slice()),
        ];

        let agg_signature = VoteSignature::from_slice(hex::decode("0x8b4aa0952e95b829596e5fbfe936195ba17cb21c83e1e69ac295ca166ed270e5ceb0cc285d51480288b6f9be2852ca7a1151364cbad69fafdbda8844189927ce0684ae5b4b0b8b42dbf1bca0957645f8dc53823554cc87d4e8adfa28d1dfec53").unwrap().as_slice());

        let vote_addrs: Vec<PublicKey> =
            vote_addrs.iter().map(|addr| PublicKey::from_bytes(addr.as_slice()).unwrap()).collect();
        let vote_addrs: Vec<&PublicKey> = vote_addrs.iter().collect();

        let sig = Signature::from_bytes(&agg_signature[..]).unwrap();
        let res =
            sig.fast_aggregate_verify(true, vote_data.hash().as_slice(), BLST_DST, &vote_addrs);

        assert_eq!(res, blst::BLST_ERROR::BLST_SUCCESS);
    }
}
