//! Bsc block executor.

use crate::{post_execution::PostExecutionInput, BscBlockExecutionError, BscEvmConfig};
use lazy_static::lazy_static;
use lru::LruCache;
use parking_lot::RwLock;
use reth_bsc_consensus::{
    is_breathe_block, is_system_transaction, validate_block_post_execution, Parlia,
    ValidatorElectionInfo, ValidatorsInfo,
};
use reth_chainspec::ChainSpec;
use reth_errors::{BlockExecutionError, BlockValidationError, ProviderError};
use reth_evm::{
    execute::{
        BatchExecutor, BlockExecutionInput, BlockExecutionOutput, BlockExecutorProvider, Executor,
    },
    ConfigureEvm,
};
use reth_primitives::{
    parlia::{ParliaConfig, Snapshot, VoteAddress, CHECKPOINT_INTERVAL},
    system_contracts::get_upgrade_system_contracts,
    Address, BlockNumber, BlockWithSenders, Bytes, Header, Receipt, Transaction, TransactionSigned,
    B256, BSC_MAINNET, U256,
};
use reth_provider::{ExecutionOutcome, ParliaProvider};
use reth_prune_types::PruneModes;
use reth_revm::{
    batch::{BlockBatchRecord, BlockExecutorStats},
    db::states::bundle_state::BundleRetention,
    Evm, State,
};
use revm_primitives::{
    db::{Database, DatabaseCommit},
    BlockEnv, CfgEnvWithHandlerCfg, EnvWithHandlerCfg, ResultAndState, TransactTo,
};
use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, time::Instant};
use tracing::debug;

const SNAP_CACHE_NUM: usize = 2048;

lazy_static! {
    // snapshot cache map by block_hash: snapshot
    static ref RECENT_SNAPS: RwLock<LruCache<B256, Snapshot>> = RwLock::new(LruCache::new(NonZeroUsize::new(SNAP_CACHE_NUM).unwrap()));
}

/// Provides executors to execute regular bsc blocks
#[derive(Debug, Clone)]
pub struct BscExecutorProvider<P, EvmConfig = BscEvmConfig> {
    chain_spec: Arc<ChainSpec>,
    evm_config: EvmConfig,
    parlia_config: ParliaConfig,
    provider: P,
}

impl<P> BscExecutorProvider<P> {
    /// Creates a new default bsc executor provider.
    pub fn bsc(chain_spec: Arc<ChainSpec>, provider: P) -> Self {
        Self::new(chain_spec, Default::default(), Default::default(), provider)
    }

    /// Returns a new provider for the mainnet.
    pub fn mainnet(provider: P) -> Self {
        Self::bsc(BSC_MAINNET.clone(), provider)
    }
}

impl<P, EvmConfig> BscExecutorProvider<P, EvmConfig> {
    /// Creates a new executor provider.
    pub const fn new(
        chain_spec: Arc<ChainSpec>,
        evm_config: EvmConfig,
        parlia_config: ParliaConfig,
        provider: P,
    ) -> Self {
        Self { chain_spec, evm_config, parlia_config, provider }
    }
}

impl<P, EvmConfig> BscExecutorProvider<P, EvmConfig>
where
    P: Clone,
    EvmConfig: ConfigureEvm,
{
    fn bsc_executor<DB>(&self, db: DB) -> BscBlockExecutor<EvmConfig, DB, P>
    where
        DB: Database<Error = ProviderError>,
    {
        BscBlockExecutor::new(
            self.chain_spec.clone(),
            self.evm_config.clone(),
            self.parlia_config.clone(),
            State::builder().with_database(db).with_bundle_update().without_state_clear().build(),
            self.provider.clone(),
        )
    }
}

impl<P, EvmConfig> BlockExecutorProvider for BscExecutorProvider<P, EvmConfig>
where
    P: ParliaProvider + Clone + Unpin + 'static,
    EvmConfig: ConfigureEvm,
{
    type Executor<DB: Database<Error = ProviderError>> = BscBlockExecutor<EvmConfig, DB, P>;

    type BatchExecutor<DB: Database<Error = ProviderError>> = BscBatchExecutor<EvmConfig, DB, P>;

    fn executor<DB>(&self, db: DB) -> Self::Executor<DB>
    where
        DB: Database<Error = ProviderError>,
    {
        self.bsc_executor(db)
    }

    fn batch_executor<DB>(&self, db: DB, prune_modes: PruneModes) -> Self::BatchExecutor<DB>
    where
        DB: Database<Error = ProviderError>,
    {
        let executor = self.bsc_executor(db);
        BscBatchExecutor {
            executor,
            batch_record: BlockBatchRecord::new(prune_modes),
            stats: BlockExecutorStats::default(),
            snapshots: Vec::new(),
        }
    }
}

/// Helper type for the output of executing a block.
#[derive(Debug, Clone)]
pub(crate) struct BscExecuteOutput {
    receipts: Vec<Receipt>,
    gas_used: u64,
    snapshot: Option<Snapshot>,
}

/// Helper container type for EVM with chain spec.
#[derive(Debug, Clone)]
pub(crate) struct BscEvmExecutor<EvmConfig> {
    /// The chain spec
    chain_spec: Arc<ChainSpec>,
    /// How to create an EVM.
    evm_config: EvmConfig,
}

impl<EvmConfig> BscEvmExecutor<EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    /// Executes the transactions in the block and returns the receipts.
    ///
    /// This applies the pre-execution changes, and executes the transactions.
    ///
    /// # Note
    ///
    /// It does __not__ apply post-execution changes.
    fn execute_pre_and_transactions<Ext, DB>(
        &self,
        block: &BlockWithSenders,
        mut evm: Evm<'_, Ext, &mut State<DB>>,
    ) -> Result<(Vec<TransactionSigned>, Vec<Receipt>, u64), BlockExecutionError>
    where
        DB: Database<Error = ProviderError>,
    {
        // execute transactions
        let mut cumulative_gas_used = 0;
        let mut system_txs = Vec::with_capacity(2); // Normally there are 2 system transactions.
        let mut receipts = Vec::with_capacity(block.body.len());
        for (sender, transaction) in block.transactions_with_sender() {
            if is_system_transaction(transaction, *sender, &block.header) {
                system_txs.push(transaction.clone());
                continue;
            }
            // systemTxs should be always at the end of block.
            if self.chain_spec.is_cancun_active_at_timestamp(block.timestamp) &&
                !system_txs.is_empty()
            {
                return Err(BscBlockExecutionError::UnexpectedNormalTx.into());
            }

            // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
            // must be no greater than the block’s gasLimit.
            let block_available_gas = block.header.gas_limit - cumulative_gas_used;
            if transaction.gas_limit() > block_available_gas {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: transaction.gas_limit(),
                    block_available_gas,
                }
                .into());
            }

            self.patch_mainnet_before_tx(transaction, evm.db_mut());
            self.patch_chapel_before_tx(transaction, evm.db_mut());

            EvmConfig::fill_tx_env(evm.tx_mut(), transaction, *sender);

            // Execute transaction.
            let ResultAndState { result, state } = evm.transact().map_err(move |err| {
                // Ensure hash is calculated for error log, if not already done
                BlockValidationError::EVM {
                    hash: transaction.recalculate_hash(),
                    error: err.into(),
                }
            })?;

            evm.db_mut().commit(state);

            self.patch_mainnet_after_tx(transaction, evm.db_mut());
            self.patch_chapel_after_tx(transaction, evm.db_mut());

            // append gas used
            cumulative_gas_used += result.gas_used();

            // Push transaction changeset and calculate header bloom filter for receipt.
            receipts.push(
                #[allow(clippy::needless_update)] // side-effect of optimism fields
                Receipt {
                    tx_type: transaction.tx_type(),
                    // Success flag was added in `EIP-658: Embedding transaction status code in
                    // receipts`.
                    success: result.is_success(),
                    cumulative_gas_used,
                    // convert to reth log
                    logs: result.into_logs(),
                    ..Default::default()
                },
            );
        }
        drop(evm);

        Ok((system_txs, receipts, cumulative_gas_used))
    }
}

/// A basic Bsc block executor.
///
/// Expected usage:
/// - Create a new instance of the executor.
/// - Execute the block.
#[derive(Debug)]
pub struct BscBlockExecutor<EvmConfig, DB, P> {
    /// Chain specific evm config that's used to execute a block.
    executor: BscEvmExecutor<EvmConfig>,
    /// The state to use for execution
    pub(crate) state: State<DB>,
    /// Extra provider for bsc
    provider: P,
    /// Parlia consensus instance
    parlia: Arc<Parlia>,
}

impl<EvmConfig, DB, P> BscBlockExecutor<EvmConfig, DB, P> {
    /// Creates a new Ethereum block executor.
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        evm_config: EvmConfig,
        parlia_config: ParliaConfig,
        state: State<DB>,
        provider: P,
    ) -> Self {
        let parlia = Arc::new(Parlia::new(Arc::clone(&chain_spec), parlia_config));
        Self { executor: BscEvmExecutor { chain_spec, evm_config }, state, provider, parlia }
    }

    #[inline]
    pub(crate) fn chain_spec(&self) -> &ChainSpec {
        &self.executor.chain_spec
    }

    #[allow(unused)]
    #[inline]
    pub(crate) fn parlia(&self) -> &Parlia {
        &self.parlia
    }

    /// Returns mutable reference to the state that wraps the underlying database.
    #[allow(unused)]
    fn state_mut(&mut self) -> &mut State<DB> {
        &mut self.state
    }
}

impl<EvmConfig, DB, P> BscBlockExecutor<EvmConfig, DB, P>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error = ProviderError>,
    P: ParliaProvider,
{
    /// Configures a new evm configuration and block environment for the given block.
    ///
    /// Caution: this does not initialize the tx environment.
    fn evm_env_for_block(&self, header: &Header, total_difficulty: U256) -> EnvWithHandlerCfg {
        let mut cfg = CfgEnvWithHandlerCfg::new(Default::default(), Default::default());
        let mut block_env = BlockEnv::default();

        EvmConfig::fill_cfg_and_block_env(
            &mut cfg,
            &mut block_env,
            self.chain_spec(),
            header,
            total_difficulty,
        );

        EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env, Default::default())
    }

    /// Execute a single block and apply the state changes to the internal state.
    ///
    /// Returns the receipts of the transactions in the block and the total gas used.
    ///
    /// Returns an error if execution fails or parlia verification fails.
    ///
    /// This function does not perform receipt root and gas used check.
    fn execute_and_verify(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<BscExecuteOutput, BlockExecutionError> {
        // 1. get parent header and snapshot
        let parent = &(self.get_header_by_hash(block.parent_hash)?);
        let snap = &(self.snapshot(parent, None)?);

        // 2. prepare state on new block
        self.on_new_block(&block.header, parent, snap)?;

        // 3. get data from contracts before execute transactions
        let post_execution_input =
            self.do_system_call_before_execution(&block.header, total_difficulty, parent)?;

        // 4. execute normal transactions
        let env = self.evm_env_for_block(&block.header, total_difficulty);

        if !self.parlia.chain_spec().is_feynman_active_at_timestamp(block.timestamp) {
            // apply system contract upgrade
            self.upgrade_system_contracts(block.number, block.timestamp, parent.timestamp)?;
        }

        let (mut system_txs, mut receipts, mut gas_used) = {
            let evm = self.executor.evm_config.evm_with_env(&mut self.state, env.clone());
            self.executor.execute_pre_and_transactions(block, evm)
        }?;

        // 5. apply post execution changes
        self.post_execution(
            block,
            parent,
            snap,
            post_execution_input,
            &mut system_txs,
            &mut receipts,
            &mut gas_used,
            env,
        )?;

        if snap.block_number % CHECKPOINT_INTERVAL == 0 {
            Ok(BscExecuteOutput { receipts, gas_used, snapshot: Some(snap.clone()) })
        } else {
            Ok(BscExecuteOutput { receipts, gas_used, snapshot: None })
        }
    }

    pub(crate) fn find_ancient_header(
        &self,
        header: &Header,
        count: u64,
    ) -> Result<Header, BlockExecutionError> {
        let mut result = header.clone();
        for _ in 0..count {
            result = self.get_header_by_hash(result.parent_hash)?;
        }
        Ok(result)
    }

    pub(crate) fn snapshot(
        &self,
        header: &Header,
        parent: Option<&Header>,
    ) -> Result<Snapshot, BlockExecutionError> {
        let mut cache = RECENT_SNAPS.write();

        let mut header = header.clone();
        let mut block_number = header.number;
        let mut block_hash = header.hash_slow();
        let mut skip_headers = Vec::new();

        let snap: Option<Snapshot>;
        loop {
            // Read from cache
            if let Some(cached) = cache.get(&block_hash) {
                snap = Some(cached.clone());
                break;
            }

            // Read from db
            if block_number % CHECKPOINT_INTERVAL == 0 {
                if let Some(cached) =
                    self.provider.get_parlia_snapshot(block_hash).map_err(|err| {
                        BscBlockExecutionError::ProviderInnerError { error: err.into() }
                    })?
                {
                    snap = Some(cached);
                    break;
                }
            }

            // If we're at the genesis, snapshot the initial state.
            if block_number == 0 {
                let ValidatorsInfo { consensus_addrs, vote_addrs } =
                    self.parlia.parse_validators_from_header(&header).map_err(|err| {
                        BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                    })?;
                snap = Some(Snapshot::new(
                    consensus_addrs,
                    block_number,
                    block_hash,
                    self.parlia.epoch(),
                    vote_addrs,
                ));
                break;
            }

            // No snapshot for this header, gather the header and move backward
            skip_headers.push(header.clone());
            if let Some(parent) = parent {
                block_number = parent.number;
                block_hash = header.parent_hash;
                header = parent.clone();
            } else if let Ok(h) = self.get_header_by_hash(header.parent_hash) {
                block_number = h.number;
                block_hash = header.parent_hash;
                header = h;
            }
        }

        let mut snap = snap.ok_or_else(|| BscBlockExecutionError::SnapshotNotFound)?;

        // apply skip headers
        skip_headers.reverse();
        for header in &skip_headers {
            let ValidatorsInfo { consensus_addrs, vote_addrs } = if header.number > 0 &&
                header.number % self.parlia.epoch() == (snap.validators.len() / 2) as u64
            {
                // change validator set
                let checkpoint_header =
                    self.find_ancient_header(header, (snap.validators.len() / 2) as u64)?;

                self.parlia.parse_validators_from_header(&checkpoint_header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?
            } else {
                ValidatorsInfo::default()
            };

            let validator = self.parlia.recover_proposer(header).map_err(|err| {
                BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
            })?;
            let attestation =
                self.parlia.get_vote_attestation_from_header(header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?;

            snap = snap
                .apply(validator, header, consensus_addrs, vote_addrs, attestation)
                .ok_or_else(|| BscBlockExecutionError::ApplySnapshotFailed)?;
        }

        cache.put(snap.block_hash, snap.clone());
        Ok(snap)
    }

    pub(crate) fn get_justified_header(
        &self,
        snap: &Snapshot,
    ) -> Result<Header, BlockExecutionError> {
        if snap.vote_data.source_hash == B256::ZERO && snap.vote_data.target_hash == B256::ZERO {
            return self
                .provider
                .header_by_number(0)
                .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })?
                .ok_or_else(|| {
                    BscBlockExecutionError::UnknownHeader { block_hash: B256::ZERO }.into()
                });
        }

        self.get_header_by_hash(snap.vote_data.target_hash)
    }

    pub(crate) fn get_header_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Header, BlockExecutionError> {
        self.provider
            .header(&block_hash)
            .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })?
            .ok_or_else(|| BscBlockExecutionError::UnknownHeader { block_hash }.into())
    }

    /// Upgrade system contracts based on the hardfork rules.
    pub(crate) fn upgrade_system_contracts(
        &mut self,
        block_number: BlockNumber,
        block_time: u64,
        parent_block_time: u64,
    ) -> Result<bool, BscBlockExecutionError> {
        if let Ok(contracts) = get_upgrade_system_contracts(
            self.parlia().chain_spec(),
            block_number,
            block_time,
            parent_block_time,
        ) {
            for (k, v) in contracts {
                debug!("Upgrade system contract {:?} at height {:?}", k, block_number);

                let account = self.state.load_cache_account(k).map_err(|err| {
                    BscBlockExecutionError::ProviderInnerError { error: err.into() }
                })?;

                let mut new_info = account.account_info().unwrap_or_default();
                new_info.code_hash = v.clone().unwrap().hash_slow();
                new_info.code = v;
                let transition = account.change(new_info, HashMap::new());

                self.state.apply_transition(vec![(k, transition)]);
            }

            Ok(true)
        } else {
            Err(BscBlockExecutionError::SystemContractUpgradeError)
        }
    }

    pub(crate) fn eth_call(
        &mut self,
        to: Address,
        data: Bytes,
        env: EnvWithHandlerCfg,
    ) -> Result<Bytes, BlockExecutionError> {
        let mut evm = self.executor.evm_config.evm_with_env(&mut self.state, env);
        let tx_env = evm.tx_mut();

        tx_env.caller = Address::default();
        tx_env.transact_to = TransactTo::Call(to);
        tx_env.nonce = None;
        tx_env.gas_limit = u64::MAX / 2;
        tx_env.value = U256::ZERO;
        tx_env.data = data;
        tx_env.gas_price = U256::ZERO;
        // The chain ID check is not relevant here and is disabled if set to None
        tx_env.chain_id = None;
        // Setting the gas priority fee to None ensures the effective gas price is derived from
        // the `gas_price` field, which we need to be zero
        tx_env.gas_priority_fee = None;
        tx_env.access_list = Vec::new();
        tx_env.blob_hashes = Vec::new();
        tx_env.max_fee_per_blob_gas = None;

        // disable the base fee check for this call by setting the base fee to zero
        let block_env = evm.block_mut();
        block_env.basefee = U256::ZERO;

        // Execute call.
        let ResultAndState { result, .. } = evm.transact().map_err(move |e| {
            // Ensure hash is calculated for error log, if not already done
            BlockValidationError::EVM { hash: B256::default(), error: e.into() }
        })?;

        if !result.is_success() {
            return Err(BscBlockExecutionError::EthCallFailed.into());
        }

        let output = result.output().ok_or_else(|| BscBlockExecutionError::EthCallFailed)?;
        Ok(output.clone())
    }

    pub(crate) fn transact_system_tx(
        &mut self,
        mut transaction: Transaction,
        sender: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let mut evm = self.executor.evm_config.evm_with_env(&mut self.state, env);

        let nonce = evm.db_mut().basic(sender).unwrap().unwrap_or_default().nonce;
        transaction.set_nonce(nonce);
        let hash = transaction.signature_hash();
        if system_txs.is_empty() || hash != system_txs[0].signature_hash() {
            debug!("unexpected transaction: {:?}", transaction);
            for tx in system_txs.iter() {
                debug!("left system tx: {:?}", tx);
            }
            return Err(BscBlockExecutionError::UnexpectedSystemTx.into());
        }
        system_txs.remove(0);

        let tx_env = evm.tx_mut();
        tx_env.caller = sender;
        tx_env.transact_to = TransactTo::Call(transaction.to().unwrap());
        tx_env.nonce = Some(transaction.nonce());
        tx_env.gas_limit = u64::MAX / 2;
        tx_env.value = transaction.value();
        tx_env.data = transaction.input().clone();
        // System transactions' gas price is always zero
        tx_env.gas_price = U256::ZERO;
        tx_env.chain_id = transaction.chain_id();
        // Setting the gas priority fee to None ensures the effective gas price is derived from
        // the `gas_price` field, which we need to be zero
        tx_env.gas_priority_fee = None;
        tx_env.access_list = Vec::new();
        tx_env.blob_hashes = Vec::new();
        tx_env.max_fee_per_blob_gas = None;
        tx_env.bsc.is_system_transaction = Some(true);

        // disable the base fee check for this call by setting the base fee to zero
        let block_env = evm.block_mut();
        block_env.basefee = U256::ZERO;

        // Execute transaction.
        let ResultAndState { result, state } = evm.transact().map_err(move |e| {
            // Ensure hash is calculated for error log, if not already done
            BlockValidationError::EVM { hash, error: e.into() }
        })?;

        evm.db_mut().commit(state);

        // append gas used
        *cumulative_gas_used += result.gas_used();

        // Push transaction changeset and calculate header bloom filter for receipt.
        receipts.push(Receipt {
            tx_type: transaction.tx_type(),
            // Success flag was added in `EIP-658: Embedding transaction status code in
            // receipts`.
            success: result.is_success(),
            cumulative_gas_used: *cumulative_gas_used,
            // convert to reth log
            logs: result.into_logs().into_iter().map(Into::into).collect(),
        });

        Ok(())
    }

    fn do_system_call_before_execution(
        &mut self,
        header: &Header,
        total_difficulty: U256,
        parent: &Header,
    ) -> Result<PostExecutionInput, BlockExecutionError> {
        // env of parent state
        let env =
            self.evm_env_for_block(parent, total_difficulty.saturating_sub(header.difficulty));
        let mut output = PostExecutionInput {
            current_validators: None,
            max_elected_validators: None,
            validators_election_info: None,
        };

        // 1. get current validators info
        if header.number % self.parlia().epoch() == 0 {
            let (validators, vote_addrs) = self.get_current_validators(parent.number, env.clone());

            let vote_addrs_map = if vote_addrs.is_empty() {
                HashMap::new()
            } else {
                validators.iter().cloned().zip(vote_addrs).collect::<HashMap<_, _>>()
            };

            output.current_validators = Some((validators, vote_addrs_map));
        };

        // 2. get election info
        if self.parlia().chain_spec().is_feynman_active_at_timestamp(header.timestamp) &&
            is_breathe_block(parent.timestamp, header.timestamp) &&
            !self
                .parlia()
                .chain_spec()
                .is_on_feynman_at_timestamp(header.timestamp, parent.timestamp)
        {
            let (to, data) = self.parlia().get_max_elected_validators();
            let bz = self.eth_call(to, data, env.clone())?;
            output.max_elected_validators =
                Some(self.parlia().unpack_data_into_max_elected_validators(bz.as_ref()));

            let (to, data) = self.parlia().get_validator_election_info();
            let bz = self.eth_call(to, data, env)?;

            let (validators, voting_powers, vote_addrs, total_length) =
                self.parlia().unpack_data_into_validator_election_info(bz.as_ref());

            let total_length = total_length.to::<u64>() as usize;
            if validators.len() != total_length ||
                voting_powers.len() != total_length ||
                vote_addrs.len() != total_length
            {
                return Err(BscBlockExecutionError::GetTopValidatorsFailed.into());
            }

            let validator_election_info = validators
                .into_iter()
                .zip(voting_powers)
                .zip(vote_addrs)
                .map(|((validator, voting_power), vote_addr)| ValidatorElectionInfo {
                    address: validator,
                    voting_power,
                    vote_address: vote_addr,
                })
                .collect();

            output.validators_election_info = Some(validator_election_info);
        }

        Ok(output)
    }

    fn get_current_validators(
        &mut self,
        number: BlockNumber,
        env: EnvWithHandlerCfg,
    ) -> (Vec<Address>, Vec<VoteAddress>) {
        if !self.parlia().chain_spec().is_luban_active_at_block(number) {
            let (to, data) = self.parlia().get_current_validators_before_luban(number);
            let output = self.eth_call(to, data, env).unwrap();

            (self.parlia().unpack_data_into_validator_set_before_luban(output.as_ref()), Vec::new())
        } else {
            let (to, data) = self.parlia().get_current_validators();
            let output = self.eth_call(to, data, env).unwrap();

            self.parlia().unpack_data_into_validator_set(output.as_ref())
        }
    }
}

impl<EvmConfig, DB, P> Executor<DB> for BscBlockExecutor<EvmConfig, DB, P>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error = ProviderError>,
    P: ParliaProvider,
{
    type Input<'a> = BlockExecutionInput<'a, BlockWithSenders>;
    type Output = BlockExecutionOutput<Receipt>;
    type Error = BlockExecutionError;

    /// Executes the block and commits the state changes.
    ///
    /// Returns the receipts of the transactions in the block.
    ///
    /// Returns an error if the block could not be executed or failed verification.
    ///
    /// State changes are committed to the database.
    fn execute(mut self, input: Self::Input<'_>) -> Result<Self::Output, Self::Error> {
        let BlockExecutionInput { block, total_difficulty } = input;
        let BscExecuteOutput { receipts, gas_used, snapshot } =
            self.execute_and_verify(block, total_difficulty)?;

        // NOTE: we need to merge keep the reverts for the bundle retention
        self.state.merge_transitions(BundleRetention::Reverts);

        Ok(BlockExecutionOutput {
            state: self.state.take_bundle(),
            receipts,
            requests: Vec::default(),
            gas_used,
            snapshot,
        })
    }
}

/// An executor for a batch of blocks.
///
/// State changes are tracked until the executor is finalized.
#[derive(Debug)]
pub struct BscBatchExecutor<EvmConfig, DB, P> {
    /// The executor used to execute blocks.
    executor: BscBlockExecutor<EvmConfig, DB, P>,
    /// Keeps track of the batch and record receipts based on the configured prune mode
    batch_record: BlockBatchRecord,
    stats: BlockExecutorStats,
    snapshots: Vec<Snapshot>,
}

impl<EvmConfig, DB, P> BscBatchExecutor<EvmConfig, DB, P> {
    /// Returns mutable reference to the state that wraps the underlying database.
    #[allow(unused)]
    fn state_mut(&mut self) -> &mut State<DB> {
        self.executor.state_mut()
    }
}

impl<EvmConfig, DB, P> BatchExecutor<DB> for BscBatchExecutor<EvmConfig, DB, P>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error = ProviderError>,
    P: ParliaProvider,
{
    type Input<'a> = BlockExecutionInput<'a, BlockWithSenders>;
    type Output = ExecutionOutcome;
    type Error = BlockExecutionError;

    fn execute_and_verify_one(&mut self, input: Self::Input<'_>) -> Result<(), Self::Error> {
        let BlockExecutionInput { block, total_difficulty } = input;
        let execute_start = Instant::now();
        let BscExecuteOutput { receipts, gas_used: _, snapshot } =
            self.executor.execute_and_verify(block, total_difficulty)?;
        self.stats.execution_duration += execute_start.elapsed();

        validate_block_post_execution(block, self.executor.chain_spec(), &receipts)?;

        // prepare the state according to the prune mode
        let merge_start = Instant::now();
        let retention = self.batch_record.bundle_retention(block.number);
        self.executor.state.merge_transitions(retention);
        self.stats.merge_transitions_duration += merge_start.elapsed();

        // store receipts in the set
        let receipts_start = Instant::now();
        self.batch_record.save_receipts(receipts)?;
        self.stats.receipt_root_duration += receipts_start.elapsed();

        // store snapshot
        if let Some(snapshot) = snapshot {
            self.snapshots.push(snapshot);
        }

        if self.batch_record.first_block().is_none() {
            self.batch_record.set_first_block(block.number);
        }

        Ok(())
    }

    fn finalize(mut self) -> Self::Output {
        self.stats.log_debug();

        ExecutionOutcome::new_with_snapshots(
            self.executor.state.take_bundle(),
            self.batch_record.take_receipts(),
            self.batch_record.first_block().unwrap_or_default(),
            Vec::default(),
            self.snapshots,
        )
    }

    fn set_tip(&mut self, tip: BlockNumber) {
        self.batch_record.set_tip(tip);
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.executor.state.bundle_state.size_hint())
    }
}
