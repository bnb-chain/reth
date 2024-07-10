use crate::{BscBlockExecutionError, BscBlockExecutor};
use bitset::BitSet;
use reth_bsc_consensus::{
    get_top_validators_by_voting_power, is_breathe_block, ElectedValidators, ValidatorElectionInfo,
    COLLECT_ADDITIONAL_VOTES_REWARD_RATIO, DIFF_INTURN, MAX_SYSTEM_REWARD, SYSTEM_REWARD_PERCENT,
};
use reth_errors::{BlockExecutionError, BlockValidationError, ProviderError};
use reth_evm::ConfigureEvm;
use reth_primitives::{
    hex,
    parlia::{Snapshot, VoteAddress, VoteAttestation},
    system_contracts::SYSTEM_REWARD_CONTRACT,
    Address, BlockWithSenders, GotExpected, Header, Receipt, TransactionSigned, U256,
};
use reth_provider::ParliaProvider;
use reth_revm::bsc::SYSTEM_ADDRESS;
use revm_primitives::{db::Database, EnvWithHandlerCfg};
use std::collections::HashMap;
use tracing::debug;

/// Helper type for the input of post execution.
#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub(crate) struct PostExecutionInput {
    pub(crate) current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>,
    pub(crate) max_elected_validators: Option<U256>,
    pub(crate) validators_election_info: Option<Vec<ValidatorElectionInfo>>,
}

impl<EvmConfig, DB, P> BscBlockExecutor<EvmConfig, DB, P>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error = ProviderError>,
    P: ParliaProvider,
{
    /// Apply post execution state changes, including system txs and other state change.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn post_execution(
        &mut self,
        block: &BlockWithSenders,
        parent: &Header,
        snap: &Snapshot,
        post_execution_input: PostExecutionInput,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let number = block.number;
        let validator = block.beneficiary;
        let header = &block.header;

        self.verify_validators(post_execution_input.current_validators, header)?;

        if number == 1 {
            self.init_genesis_contracts(
                validator,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        if self.parlia().chain_spec().is_feynman_active_at_timestamp(block.timestamp) {
            // apply system contract upgrade
            self.upgrade_system_contracts(block.number, block.timestamp, parent.timestamp)?;
        }

        if self.parlia().chain_spec().is_on_feynman_at_timestamp(block.timestamp, parent.timestamp)
        {
            self.init_feynman_contracts(
                validator,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        // slash validator if it's not inturn
        if block.difficulty != DIFF_INTURN {
            let spoiled_val = snap.inturn_validator();
            let signed_recently = if self.parlia().chain_spec().is_plato_active_at_block(number) {
                snap.sign_recently(spoiled_val)
            } else {
                snap.recent_proposers.iter().any(|(_, v)| *v == spoiled_val)
            };

            if !signed_recently {
                self.slash_spoiled_validator(
                    validator,
                    spoiled_val,
                    system_txs,
                    receipts,
                    cumulative_gas_used,
                    env.clone(),
                )?;
            }
        }

        self.distribute_incoming(header, system_txs, receipts, cumulative_gas_used, env.clone())?;

        if self.parlia().chain_spec().is_plato_active_at_block(number) {
            self.distribute_finality_reward(
                header,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        // update validator set after Feynman upgrade
        if self.parlia().chain_spec().is_feynman_active_at_timestamp(header.timestamp) &&
            is_breathe_block(parent.timestamp, header.timestamp) &&
            !self
                .parlia()
                .chain_spec()
                .is_on_feynman_at_timestamp(header.timestamp, parent.timestamp)
        {
            let max_elected_validators = post_execution_input
                .max_elected_validators
                .ok_or_else(|| BscBlockExecutionError::InvalidValidatorsElectionInfoData)?;
            let validators_election_info = post_execution_input
                .validators_election_info
                .ok_or_else(|| BscBlockExecutionError::InvalidValidatorsElectionInfoData)?;

            self.update_validator_set_v2(
                max_elected_validators,
                validators_election_info,
                validator,
                system_txs,
                receipts,
                cumulative_gas_used,
                env,
            )?;
        }

        if !system_txs.is_empty() {
            return Err(BscBlockExecutionError::UnexpectedSystemTx.into());
        }

        Ok(())
    }

    fn verify_validators(
        &self,
        current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>,
        header: &Header,
    ) -> Result<(), BlockExecutionError> {
        let number = header.number;
        if number % self.parlia().epoch() != 0 {
            return Ok(())
        };

        let (mut validators, mut vote_addrs_map) = current_validators
            .ok_or_else(|| BscBlockExecutionError::InvalidCurrentValidatorsData)?;
        validators.sort();

        let validator_num = validators.len();
        if self.parlia().chain_spec().is_on_luban_at_block(number) {
            vote_addrs_map = validators
                .iter()
                .cloned()
                .zip(vec![VoteAddress::default(); validator_num])
                .collect::<HashMap<_, _>>();
        }

        let validator_bytes: Vec<u8> = validators
            .into_iter()
            .flat_map(|v| {
                let mut bytes = v.to_vec();
                if self.parlia().chain_spec().is_luban_active_at_block(number) {
                    bytes.extend_from_slice(vote_addrs_map[&v].as_ref());
                }
                bytes
            })
            .collect();

        let expected = self.parlia().get_validator_bytes_from_header(header).unwrap();
        if !validator_bytes.as_slice().eq(expected.as_slice()) {
            debug!("validator bytes: {:?}", hex::encode(validator_bytes));
            debug!("expected: {:?}", hex::encode(expected));
            return Err(BscBlockExecutionError::InvalidValidators.into());
        }

        Ok(())
    }

    fn init_genesis_contracts(
        &mut self,
        validator: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let transactions = self.parlia().init_genesis_contracts();
        for tx in transactions {
            self.transact_system_tx(
                tx,
                validator,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        Ok(())
    }

    fn init_feynman_contracts(
        &mut self,
        validator: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let transactions = self.parlia().init_feynman_contracts();
        for tx in transactions {
            self.transact_system_tx(
                tx,
                validator,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        Ok(())
    }

    fn slash_spoiled_validator(
        &mut self,
        validator: Address,
        spoiled_val: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        self.transact_system_tx(
            self.parlia().slash(spoiled_val),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env,
        )?;

        Ok(())
    }

    fn distribute_incoming(
        &mut self,
        header: &Header,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let validator = header.beneficiary;

        let system_account = self
            .state
            .load_cache_account(SYSTEM_ADDRESS)
            .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })?;

        if header.number != 1 &&
            (system_account.account.is_none() ||
                system_account.account.as_ref().unwrap().info.balance == U256::ZERO)
        {
            return Ok(());
        }

        let (mut block_reward, transition) = system_account.drain_balance();
        self.state.apply_transition(vec![(SYSTEM_ADDRESS, transition)]);

        // if block reward is zero, no need to distribute
        if block_reward == 0 {
            return Ok(());
        }

        let balance_increment = HashMap::from([(validator, block_reward)]);
        self.state
            .increment_balances(balance_increment)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        let system_reward_balance = self
            .state
            .basic(SYSTEM_REWARD_CONTRACT.parse().unwrap())
            .unwrap()
            .unwrap_or_default()
            .balance;
        if !self.parlia().chain_spec().is_kepler_active_at_timestamp(header.timestamp) &&
            system_reward_balance < U256::from(MAX_SYSTEM_REWARD)
        {
            let reward_to_system = block_reward >> SYSTEM_REWARD_PERCENT;
            if reward_to_system > 0 {
                self.transact_system_tx(
                    self.parlia().distribute_to_system(reward_to_system),
                    validator,
                    system_txs,
                    receipts,
                    cumulative_gas_used,
                    env.clone(),
                )?;
            }

            block_reward -= reward_to_system;
        }

        self.transact_system_tx(
            self.parlia().distribute_to_validator(validator, block_reward),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env,
        )?;

        Ok(())
    }

    fn distribute_finality_reward(
        &mut self,
        header: &Header,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        if header.number % self.parlia().epoch() != 0 {
            return Ok(());
        }

        let validator = header.beneficiary;
        let mut accumulated_weights: HashMap<Address, U256> = HashMap::new();

        let start = (header.number - self.parlia().epoch()).max(1);
        let end = header.number;
        let mut target_hash = header.parent_hash;
        for _ in (start..end).rev() {
            let header = &(self.get_header_by_hash(target_hash)?);

            if let Some(attestation) =
                self.parlia().get_vote_attestation_from_header(header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?
            {
                self.process_attestation(&attestation, header, &mut accumulated_weights)?;
            }

            target_hash = header.parent_hash;
        }

        let mut validators: Vec<Address> = accumulated_weights.keys().cloned().collect();
        validators.sort();
        let weights: Vec<U256> = validators.iter().map(|val| accumulated_weights[val]).collect();

        self.transact_system_tx(
            self.parlia().distribute_finality_reward(validators, weights),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env,
        )?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn update_validator_set_v2(
        &mut self,
        max_elected_validators: U256,
        validators_election_info: Vec<ValidatorElectionInfo>,
        validator: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let ElectedValidators { validators, voting_powers, vote_addrs } =
            get_top_validators_by_voting_power(validators_election_info, max_elected_validators)
                .ok_or_else(|| BscBlockExecutionError::GetTopValidatorsFailed)?;

        self.transact_system_tx(
            self.parlia().update_validator_set_v2(validators, voting_powers, vote_addrs),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env,
        )?;

        Ok(())
    }

    fn process_attestation(
        &self,
        attestation: &VoteAttestation,
        parent_header: &Header,
        accumulated_weights: &mut HashMap<Address, U256>,
    ) -> Result<(), BlockExecutionError> {
        let justified_header = self.get_header_by_hash(attestation.data.target_hash)?;
        let parent = self.get_header_by_hash(justified_header.parent_hash)?;
        let snapshot = self.snapshot(&parent, None)?;
        let validators = &snapshot.validators;
        let validators_bit_set = BitSet::from_u64(attestation.vote_address_set);

        if validators_bit_set.count() as usize > validators.len() {
            return Err(BscBlockExecutionError::InvalidAttestationVoteCount(GotExpected {
                got: validators_bit_set.count(),
                expected: validators.len() as u64,
            })
            .into());
        }

        let mut valid_vote_count = 0;
        for (index, validator) in validators.iter().enumerate() {
            if validators_bit_set.test(index) {
                *accumulated_weights.entry(*validator).or_insert(U256::ZERO) += U256::from(1);
                valid_vote_count += 1;
            }
        }

        let quorum = (validators.len() * 2 + 2) / 3; // ceil div
        if valid_vote_count > quorum {
            let reward =
                ((valid_vote_count - quorum) * COLLECT_ADDITIONAL_VOTES_REWARD_RATIO) / 100;
            *accumulated_weights.entry(parent_header.beneficiary).or_insert(U256::ZERO) +=
                U256::from(reward);
        }

        Ok(())
    }
}
