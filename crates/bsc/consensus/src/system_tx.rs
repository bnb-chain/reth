use crate::{
    Parlia, BSC_GOVERNOR_CONTRACT, BSC_TIMELOCK_CONTRACT, CROSS_CHAIN_CONTRACT, GOV_TOKEN_CONTRACT,
    LIGHT_CLIENT_CONTRACT, RELAYER_HUB_CONTRACT, RELAYER_INCENTIVIZE_CONTRACT, SLASH_CONTRACT,
    STAKE_HUB_CONTRACT, SYSTEM_REWARD_CONTRACT, TOKEN_HUB_CONTRACT, TOKEN_RECOVER_PORTAL_CONTRACT,
    VALIDATOR_CONTRACT,
};
use alloy_dyn_abi::{DynSolValue, JsonAbiExt};
use reth_primitives::{Address, Bytes, Transaction, TxKind, TxLegacy, U256};

/// Assemble system tx
impl Parlia {
    pub fn init_genesis_contracts(&self) -> Vec<Transaction> {
        let function = self.validator_abi.function("init").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[]).unwrap();

        let contracts = vec![
            *VALIDATOR_CONTRACT,
            *SLASH_CONTRACT,
            *LIGHT_CLIENT_CONTRACT,
            *RELAYER_HUB_CONTRACT,
            *TOKEN_HUB_CONTRACT,
            *RELAYER_INCENTIVIZE_CONTRACT,
            *CROSS_CHAIN_CONTRACT,
        ];

        contracts
            .into_iter()
            .map(|contract| {
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(self.chain_spec.chain.id()),
                    nonce: 0,
                    gas_limit: u64::MAX / 2,
                    gas_price: 0,
                    value: U256::ZERO,
                    input: Bytes::from(input.clone()),
                    to: TxKind::Call(contract),
                })
            })
            .collect()
    }

    pub fn init_feynman_contracts(&self) -> Vec<Transaction> {
        let function = self.stake_hub_abi.function("initialize").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[]).unwrap();

        let contracts = vec![
            *STAKE_HUB_CONTRACT,
            *BSC_GOVERNOR_CONTRACT,
            *GOV_TOKEN_CONTRACT,
            *BSC_TIMELOCK_CONTRACT,
            *TOKEN_RECOVER_PORTAL_CONTRACT,
        ];

        contracts
            .into_iter()
            .map(|contract| {
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(self.chain_spec.chain.id()),
                    nonce: 0,
                    gas_limit: u64::MAX / 2,
                    gas_price: 0,
                    value: U256::ZERO,
                    input: Bytes::from(input.clone()),
                    to: TxKind::Call(contract),
                })
            })
            .collect()
    }

    pub fn slash(&self, address: Address) -> Transaction {
        let function = self.slash_abi.function("slash").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[DynSolValue::from(address)]).unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::ZERO,
            input: Bytes::from(input),
            to: TxKind::Call(*SLASH_CONTRACT),
        })
    }

    pub fn distribute_to_system(&self, system_reward: u128) -> Transaction {
        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::from(system_reward),
            input: Bytes::default(),
            to: TxKind::Call(*SYSTEM_REWARD_CONTRACT),
        })
    }

    pub fn distribute_to_validator(&self, address: Address, block_reward: u128) -> Transaction {
        let function = self.validator_abi.function("deposit").unwrap().first().unwrap();
        let input = function.abi_encode_input(&[DynSolValue::from(address)]).unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::from(block_reward),
            input: Bytes::from(input),
            to: TxKind::Call(*VALIDATOR_CONTRACT),
        })
    }

    pub fn distribute_finality_reward(
        &self,
        validators: Vec<Address>,
        weights: Vec<U256>,
    ) -> Transaction {
        let function =
            self.validator_abi.function("distributeFinalityReward").unwrap().first().unwrap();

        let validators = validators.into_iter().map(DynSolValue::from).collect();
        let weights = weights.into_iter().map(DynSolValue::from).collect();
        let input = function
            .abi_encode_input(&[DynSolValue::Array(validators), DynSolValue::Array(weights)])
            .unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::ZERO,
            input: Bytes::from(input),
            to: TxKind::Call(*VALIDATOR_CONTRACT),
        })
    }

    pub fn update_validator_set_v2(
        &self,
        validators: Vec<Address>,
        voting_powers: Vec<u64>,
        vote_addresses: Vec<Vec<u8>>,
    ) -> Transaction {
        let function =
            self.validator_abi.function("updateValidatorSetV2").unwrap().first().unwrap();

        let validators = validators.into_iter().map(DynSolValue::from).collect();
        let voting_powers = voting_powers.into_iter().map(DynSolValue::from).collect();
        let vote_addresses = vote_addresses.into_iter().map(DynSolValue::from).collect();
        let input = function
            .abi_encode_input(&[
                DynSolValue::Array(validators),
                DynSolValue::Array(voting_powers),
                DynSolValue::Array(vote_addresses),
            ])
            .unwrap();

        Transaction::Legacy(TxLegacy {
            chain_id: Some(self.chain_spec.chain.id()),
            nonce: 0,
            gas_limit: u64::MAX / 2,
            gas_price: 0,
            value: U256::ZERO,
            input: Bytes::from(input),
            to: TxKind::Call(*VALIDATOR_CONTRACT),
        })
    }
}
