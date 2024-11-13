//! Bsc block executor.

use core::fmt::Display;
use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use lazy_static::lazy_static;
use lru::LruCache;
use parking_lot::RwLock;
use reth_bsc_chainspec::BscChainSpec;
use reth_bsc_consensus::{
    is_breathe_block, validate_block_post_execution_of_bsc, Parlia, ValidatorElectionInfo,
    ValidatorsInfo,
};
use reth_bsc_forks::BscHardforks;
use reth_bsc_primitives::system_contracts::{
    get_upgrade_system_contracts, is_system_transaction, SLASH_CONTRACT,
};
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_errors::{BlockExecutionError, BlockValidationError, ProviderError};
use reth_evm::{
    execute::{
        BatchExecutor, BlockExecutionInput, BlockExecutionOutput, BlockExecutorProvider, Executor,
    },
    system_calls::{NoopHook, OnStateHook},
    ConfigureEvm,
};
use reth_primitives::{
    parlia::{ParliaConfig, Snapshot, VoteAddress, CHECKPOINT_INTERVAL, DEFAULT_TURN_LENGTH},
    BlockWithSenders, Header, Receipt, Transaction, TransactionSigned,
};
use reth_provider::{ExecutionOutcome, ParliaProvider};
use reth_prune_types::PruneModes;
use reth_revm::{batch::BlockBatchRecord, db::states::bundle_state::BundleRetention, Evm, State};
use revm_primitives::{
    db::{Database, DatabaseCommit},
    BlockEnv, CfgEnvWithHandlerCfg, EVMError, EnvWithHandlerCfg, EvmState, ResultAndState,
    TransactTo,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::{post_execution::PostExecutionInput, BscBlockExecutionError, BscEvmConfig};

const SNAP_CACHE_NUM: usize = 2048;

lazy_static! {
    // snapshot cache map by block_hash: snapshot
    static ref RECENT_SNAPS: RwLock<LruCache<B256, Snapshot>> = RwLock::new(LruCache::new(NonZeroUsize::new(SNAP_CACHE_NUM).unwrap()));
}

/// Provides executors to execute regular bsc blocks
#[derive(Debug, Clone)]
pub struct BscExecutorProvider<P, EvmConfig = BscEvmConfig> {
    chain_spec: Arc<BscChainSpec>,
    evm_config: EvmConfig,
    parlia_config: ParliaConfig,
    provider: P,
}

impl<P> BscExecutorProvider<P> {
    /// Creates a new default bsc executor provider.
    pub fn bsc(chain_spec: Arc<BscChainSpec>, provider: P) -> Self {
        Self::new(chain_spec.clone(), BscEvmConfig::new(chain_spec), Default::default(), provider)
    }
}

impl<P, EvmConfig> BscExecutorProvider<P, EvmConfig> {
    /// Creates a new executor provider.
    pub const fn new(
        chain_spec: Arc<BscChainSpec>,
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
    EvmConfig: ConfigureEvm<Header = Header>,
{
    fn bsc_executor<DB>(
        &self,
        db: DB,
        prefetch_tx: Option<UnboundedSender<EvmState>>,
    ) -> BscBlockExecutor<EvmConfig, DB, P>
    where
        DB: Database<Error: Into<ProviderError> + Display>,
    {
        if let Some(tx) = prefetch_tx {
            BscBlockExecutor::new_with_prefetch_tx(
                self.chain_spec.clone(),
                self.evm_config.clone(),
                self.parlia_config.clone(),
                State::builder()
                    .with_database(db)
                    .with_bundle_update()
                    .without_state_clear()
                    .build(),
                self.provider.clone(),
                tx,
            )
        } else {
            BscBlockExecutor::new(
                self.chain_spec.clone(),
                self.evm_config.clone(),
                self.parlia_config.clone(),
                State::builder()
                    .with_database(db)
                    .with_bundle_update()
                    .without_state_clear()
                    .build(),
                self.provider.clone(),
            )
        }
    }
}

impl<P, EvmConfig> BlockExecutorProvider for BscExecutorProvider<P, EvmConfig>
where
    P: ParliaProvider + Clone + Unpin + 'static,
    EvmConfig: ConfigureEvm<Header = Header>,
{
    type Executor<DB: Database<Error: Into<ProviderError> + Display>> =
        BscBlockExecutor<EvmConfig, DB, P>;

    type BatchExecutor<DB: Database<Error: Into<ProviderError> + Display>> =
        BscBatchExecutor<EvmConfig, DB, P>;

    fn executor<DB>(
        &self,
        db: DB,
        prefetch_tx: Option<UnboundedSender<EvmState>>,
    ) -> Self::Executor<DB>
    where
        DB: Database<Error: Into<ProviderError> + Display>,
    {
        self.bsc_executor(db, prefetch_tx)
    }

    fn batch_executor<DB>(
        &self,
        db: DB,
        prefetch_tx: Option<UnboundedSender<EvmState>>,
    ) -> Self::BatchExecutor<DB>
    where
        DB: Database<Error: Into<ProviderError> + Display>,
    {
        let executor = self.bsc_executor(db, prefetch_tx);
        BscBatchExecutor {
            executor,
            batch_record: BlockBatchRecord::default(),
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
    chain_spec: Arc<BscChainSpec>,
    /// How to create an EVM.
    evm_config: EvmConfig,
}

impl<EvmConfig> BscEvmExecutor<EvmConfig>
where
    EvmConfig: ConfigureEvm<Header = Header>,
{
    /// Executes the transactions in the block and returns the receipts.
    ///
    /// This applies the pre-execution changes, and executes the transactions.
    ///
    /// The optional `state_hook` is unused for now.
    ///
    /// # Note
    ///
    /// It does __not__ apply post-execution changes.
    fn execute_pre_and_transactions<Ext, DB, F>(
        &self,
        block: &BlockWithSenders,
        mut evm: Evm<'_, Ext, &mut State<DB>>,
        _state_hook: Option<F>,
        tx: Option<UnboundedSender<EvmState>>,
    ) -> Result<(Vec<TransactionSigned>, Vec<Receipt>, u64), BlockExecutionError>
    where
        DB: Database<Error: Into<ProviderError> + Display>,
        F: OnStateHook,
    {
        // execute transactions
        let mut cumulative_gas_used = 0;
        let mut system_txs = Vec::with_capacity(2); // Normally there are 2 system transactions.
        let mut receipts = Vec::with_capacity(block.body.transactions.len());
        for (sender, transaction) in block.transactions_with_sender() {
            if is_system_transaction(transaction, *sender, block.beneficiary) {
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

            self.evm_config.fill_tx_env(evm.tx_mut(), transaction, *sender);

            // Execute transaction.
            let ResultAndState { result, state } = evm.transact().map_err(move |err| {
                let new_err = match err {
                    EVMError::Transaction(e) => EVMError::Transaction(e),
                    EVMError::Header(e) => EVMError::Header(e),
                    EVMError::Database(e) => EVMError::Database(e.into()),
                    EVMError::Custom(e) => EVMError::Custom(e),
                    EVMError::Precompile(e) => EVMError::Precompile(e),
                };
                // Ensure hash is calculated for error log, if not already done
                BlockValidationError::EVM {
                    hash: transaction.recalculate_hash(),
                    error: Box::new(new_err),
                }
            })?;

            if let Some(tx) = tx.as_ref() {
                tx.send(state.clone()).unwrap_or_else(|err| {
                    debug!(target: "evm_executor", ?err, "Failed to send post state to prefetch channel")
                });
            }

            evm.db_mut().commit(state);

            self.patch_mainnet_after_tx(transaction, evm.db_mut());
            self.patch_chapel_after_tx(transaction, evm.db_mut());

            // append gas used
            cumulative_gas_used += result.gas_used();

            // Push transaction changeset and calculate header bloom filter for receipt.
            receipts.push(Receipt {
                tx_type: transaction.tx_type(),
                // Success flag was added in `EIP-658: Embedding transaction status code in
                // receipts`.
                success: result.is_success(),
                cumulative_gas_used,
                // convert to reth log
                logs: result.into_logs(),
            });
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
    pub(crate) provider: Arc<P>,
    /// Parlia consensus instance
    pub(crate) parlia: Arc<Parlia>,
    /// Prefetch channel
    prefetch_tx: Option<UnboundedSender<EvmState>>,
}

impl<EvmConfig, DB, P> BscBlockExecutor<EvmConfig, DB, P> {
    /// Creates a new Parlia block executor.
    pub fn new(
        chain_spec: Arc<BscChainSpec>,
        evm_config: EvmConfig,
        parlia_config: ParliaConfig,
        state: State<DB>,
        provider: P,
    ) -> Self {
        let parlia = Arc::new(Parlia::new(Arc::clone(&chain_spec), parlia_config));
        let shared_provider = Arc::new(provider);
        Self {
            executor: BscEvmExecutor { chain_spec, evm_config },
            state,
            provider: shared_provider,
            parlia,
            prefetch_tx: None,
        }
    }

    /// Creates a new BSC block executor with a prefetch channel.
    pub fn new_with_prefetch_tx(
        chain_spec: Arc<BscChainSpec>,
        evm_config: EvmConfig,
        parlia_config: ParliaConfig,
        state: State<DB>,
        provider: P,
        tx: UnboundedSender<EvmState>,
    ) -> Self {
        let parlia = Arc::new(Parlia::new(Arc::clone(&chain_spec), parlia_config));
        let shared_provider = Arc::new(provider);
        Self {
            executor: BscEvmExecutor { chain_spec, evm_config },
            state,
            provider: shared_provider,
            parlia,
            prefetch_tx: Some(tx),
        }
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
    EvmConfig: ConfigureEvm<Header = Header>,
    DB: Database<Error: Into<ProviderError> + Display>,
    P: ParliaProvider,
{
    /// Configures a new evm configuration and block environment for the given block.
    ///
    /// Caution: this does not initialize the tx environment.
    fn evm_env_for_block(&self, header: &Header, total_difficulty: U256) -> EnvWithHandlerCfg {
        let mut cfg = CfgEnvWithHandlerCfg::new(Default::default(), Default::default());
        let mut block_env = BlockEnv::default();
        self.executor.evm_config.fill_cfg_and_block_env(
            &mut cfg,
            &mut block_env,
            header,
            total_difficulty,
        );

        EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env, Default::default())
    }

    /// Convenience method to invoke `execute_without_verification_with_state_hook` setting the
    /// state hook as `None`.
    fn execute_without_verification(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
    ) -> Result<BscExecuteOutput, BlockExecutionError> {
        self.execute_without_verification_with_state_hook(
            block,
            total_difficulty,
            ancestor,
            None::<NoopHook>,
        )
    }

    /// Execute a single block and apply the state changes to the internal state.
    ///
    /// Returns the receipts of the transactions in the block and the total gas used.
    ///
    /// Returns an error if execution fails.
    fn execute_without_verification_with_state_hook<F>(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
        state_hook: Option<F>,
    ) -> Result<BscExecuteOutput, BlockExecutionError>
    where
        F: OnStateHook,
    {
        // 1. get parent header and snapshot
        let parent = &(self.get_header_by_hash(block.parent_hash, ancestor)?);
        let snapshot_reader = SnapshotReader::new(self.provider.clone(), self.parlia.clone());
        let snap = &(snapshot_reader.snapshot(parent, ancestor)?);

        // 2. prepare state on new block
        self.on_new_block(&block.header, parent, ancestor, snap)?;

        // 3. get data from contracts before execute transactions
        let post_execution_input =
            self.do_system_call_before_execution(&block.header, total_difficulty, parent)?;

        // 4. execute normal transactions
        let env = self.evm_env_for_block(&block.header, total_difficulty);

        if !self.chain_spec().is_feynman_active_at_timestamp(block.timestamp) {
            // apply system contract upgrade
            self.upgrade_system_contracts(block.number, block.timestamp, parent.timestamp)?;
        }

        let (mut system_txs, mut receipts, mut gas_used) = {
            let evm = self.executor.evm_config.evm_with_env(&mut self.state, env.clone());
            self.executor.execute_pre_and_transactions(
                block,
                evm,
                state_hook,
                self.prefetch_tx.clone(),
            )
        }?;

        // 5. apply post execution changes
        self.post_execution(
            block,
            parent,
            ancestor,
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

    pub(crate) fn get_justified_header(
        &self,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
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

        self.get_header_by_hash(snap.vote_data.target_hash, ancestor)
    }

    pub(crate) fn get_header_by_hash(
        &self,
        block_hash: B256,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
    ) -> Result<Header, BlockExecutionError> {
        ancestor
            .and_then(|m| m.get(&block_hash).cloned())
            .or_else(|| {
                self.provider
                    .header(&block_hash)
                    .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })
                    .ok()
                    .flatten()
            })
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
            self.chain_spec(),
            block_number,
            block_time,
            parent_block_time,
        ) {
            for (k, v) in contracts {
                debug!("Upgrade system contract {:?} at height {:?}", k, block_number);

                let account = self.state.load_cache_account(k).map_err(|err| {
                    BscBlockExecutionError::ProviderInnerError { error: Box::new(err.into()) }
                })?;

                let mut new_info = account.account_info().unwrap_or_default();
                new_info.code_hash = v.clone().unwrap().hash_slow();
                new_info.code = v;
                let transition = account.change(new_info, Default::default());

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
        let ResultAndState { result, .. } = evm.transact().map_err(move |err| {
            let new_err = match err {
                EVMError::Transaction(e) => EVMError::Transaction(e),
                EVMError::Header(e) => EVMError::Header(e),
                EVMError::Database(e) => EVMError::Database(e.into()),
                EVMError::Custom(e) => EVMError::Custom(e),
                EVMError::Precompile(e) => EVMError::Precompile(e),
            };
            // Ensure hash is calculated for error log, if not already done
            BlockValidationError::EVM { hash: B256::default(), error: Box::new(new_err) }
        })?;

        if !result.is_success() {
            return Err(BscBlockExecutionError::EthCallFailed.into());
        }

        let output = result.output().ok_or(BscBlockExecutionError::EthCallFailed)?;
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

        let nonce = evm
            .db_mut()
            .basic(sender)
            .map_err(|err| BscBlockExecutionError::ProviderInnerError {
                error: Box::new(err.into()),
            })
            .unwrap()
            .unwrap_or_default()
            .nonce;
        transaction.set_nonce(nonce);
        let hash = transaction.signature_hash();
        if system_txs.is_empty() || hash != system_txs[0].signature_hash() {
            // slash tx could fail and not in the block
            if let Some(to) = transaction.to() {
                if to == SLASH_CONTRACT.parse::<Address>().unwrap() &&
                    (system_txs.is_empty() ||
                        system_txs[0].to().unwrap_or_default() !=
                            SLASH_CONTRACT.parse::<Address>().unwrap())
                {
                    warn!("slash validator failed");
                    return Ok(());
                }
            }

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
        let ResultAndState { result, state } = evm.transact().map_err(move |err| {
            let new_err = match err {
                EVMError::Transaction(e) => EVMError::Transaction(e),
                EVMError::Header(e) => EVMError::Header(e),
                EVMError::Database(e) => EVMError::Database(e.into()),
                EVMError::Custom(e) => EVMError::Custom(e),
                EVMError::Precompile(e) => EVMError::Precompile(e),
            };
            // Ensure hash is calculated for error log, if not already done
            BlockValidationError::EVM { hash, error: Box::new(new_err) }
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
                validators
                    .iter()
                    .copied()
                    .zip(vote_addrs)
                    .collect::<std::collections::HashMap<_, _>>()
            };

            output.current_validators = Some((validators, vote_addrs_map));
        };

        // 2. get election info
        if self.chain_spec().is_feynman_active_at_timestamp(header.timestamp) &&
            is_breathe_block(parent.timestamp, header.timestamp) &&
            !self.chain_spec().is_on_feynman_at_timestamp(header.timestamp, parent.timestamp)
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
        if self.chain_spec().is_luban_active_at_block(number) {
            let (to, data) = self.parlia().get_current_validators();
            let output = self.eth_call(to, data, env).unwrap();

            self.parlia().unpack_data_into_validator_set(output.as_ref())
        } else {
            let (to, data) = self.parlia().get_current_validators_before_luban(number);
            let output = self.eth_call(to, data, env).unwrap();

            (self.parlia().unpack_data_into_validator_set_before_luban(output.as_ref()), Vec::new())
        }
    }
}

impl<EvmConfig, DB, P> Executor<DB> for BscBlockExecutor<EvmConfig, DB, P>
where
    EvmConfig: ConfigureEvm<Header = Header>,
    DB: Database<Error: Into<ProviderError> + Display>,
    P: ParliaProvider,
{
    type Input<'a> = BlockExecutionInput<'a, BlockWithSenders, Header>;
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
        let BlockExecutionInput { block, total_difficulty, ancestor_headers } = input;
        let BscExecuteOutput { receipts, gas_used, snapshot } =
            self.execute_without_verification(block, total_difficulty, ancestor_headers)?;

        // NOTE: we need to merge keep the reverts for the bundle retention
        self.state.merge_transitions(BundleRetention::Reverts);

        Ok(BlockExecutionOutput {
            state: self.state.take_bundle(),
            receipts,
            requests: Default::default(),
            gas_used,
            snapshot,
        })
    }

    fn execute_with_state_closure<F>(
        mut self,
        input: Self::Input<'_>,
        mut witness: F,
    ) -> Result<Self::Output, Self::Error>
    where
        F: FnMut(&State<DB>),
    {
        let BlockExecutionInput { block, total_difficulty, ancestor_headers } = input;
        let BscExecuteOutput { receipts, gas_used, snapshot } =
            self.execute_without_verification(block, total_difficulty, ancestor_headers)?;

        // NOTE: we need to merge keep the reverts for the bundle retention
        self.state.merge_transitions(BundleRetention::Reverts);
        witness(&self.state);

        Ok(BlockExecutionOutput {
            state: self.state.take_bundle(),
            receipts,
            requests: Default::default(),
            gas_used,
            snapshot,
        })
    }

    fn execute_with_state_hook<F>(
        mut self,
        input: Self::Input<'_>,
        state_hook: F,
    ) -> Result<Self::Output, Self::Error>
    where
        F: OnStateHook,
    {
        let BlockExecutionInput { block, total_difficulty, ancestor_headers } = input;
        let BscExecuteOutput { receipts, gas_used, snapshot } = self
            .execute_without_verification_with_state_hook(
                block,
                total_difficulty,
                ancestor_headers,
                Some(state_hook),
            )?;

        // NOTE: we need to merge keep the reverts for the bundle retention
        self.state.merge_transitions(BundleRetention::Reverts);

        Ok(BlockExecutionOutput {
            state: self.state.take_bundle(),
            receipts,
            requests: Default::default(),
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
    EvmConfig: ConfigureEvm<Header = Header>,
    DB: Database<Error: Into<ProviderError> + Display>,
    P: ParliaProvider,
{
    type Input<'a> = BlockExecutionInput<'a, BlockWithSenders, Header>;
    type Output = ExecutionOutcome;
    type Error = BlockExecutionError;

    fn execute_and_verify_one(&mut self, input: Self::Input<'_>) -> Result<(), Self::Error> {
        let BlockExecutionInput { block, total_difficulty, .. } = input;
        let BscExecuteOutput { receipts, gas_used: _, snapshot } =
            self.executor.execute_without_verification(block, total_difficulty, None)?;

        validate_block_post_execution_of_bsc(block, self.executor.chain_spec(), &receipts)?;

        // prepare the state according to the prune mode
        let retention = self.batch_record.bundle_retention(block.number);
        self.executor.state.merge_transitions(retention);

        // store receipts in the set
        self.batch_record.save_receipts(receipts)?;

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

    fn set_prune_modes(&mut self, prune_modes: PruneModes) {
        self.batch_record.set_prune_modes(prune_modes);
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.executor.state.bundle_state.size_hint())
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotReader<P> {
    /// Extra provider for bsc
    provider: Arc<P>,
    /// Parlia consensus instance
    parlia: Arc<Parlia>,
}

impl<P> SnapshotReader<P>
where
    P: ParliaProvider,
{
    pub const fn new(provider: Arc<P>, parlia: Arc<Parlia>) -> Self {
        Self { provider, parlia }
    }

    pub fn snapshot(
        &self,
        header: &Header,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
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
            if let Ok(h) = self.get_header_by_hash(header.parent_hash, ancestor) {
                block_number = h.number;
                block_hash = header.parent_hash;
                header = h;
            } else {
                return Err(
                    BscBlockExecutionError::UnknownHeader { block_hash: header.parent_hash }.into()
                )
            }
        }

        let mut snap = snap.ok_or(BscBlockExecutionError::SnapshotNotFound)?;

        // the old snapshots don't have turn length, make sure we initialize it with default
        // before accessing it
        if snap.turn_length.is_none() || snap.turn_length == Some(0) {
            snap.turn_length = Some(DEFAULT_TURN_LENGTH);
        }

        // apply skip headers
        skip_headers.reverse();
        for header in &skip_headers {
            let (ValidatorsInfo { consensus_addrs, vote_addrs }, turn_length) = if header.number > 0 &&
                header.number % self.parlia.epoch() == snap.miner_history_check_len()
            {
                // change validator set
                let checkpoint_header =
                    self.find_ancient_header(header, ancestor, snap.miner_history_check_len())?;

                let validators_info = self
                    .parlia
                    .parse_validators_from_header(&checkpoint_header)
                    .map_err(|err| BscBlockExecutionError::ParliaConsensusInnerError {
                        error: err.into(),
                    })?;

                let turn_length =
                    self.parlia.get_turn_length_from_header(&checkpoint_header).map_err(|err| {
                        BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                    })?;

                (validators_info, turn_length)
            } else {
                (ValidatorsInfo::default(), None)
            };

            let validator = self.parlia.recover_proposer(header).map_err(|err| {
                BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
            })?;
            let attestation =
                self.parlia.get_vote_attestation_from_header(header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?;

            snap = snap
                .apply(
                    validator,
                    header,
                    consensus_addrs,
                    vote_addrs,
                    attestation,
                    turn_length,
                    self.parlia.chain_spec().is_bohr_active_at_timestamp(header.timestamp),
                )
                .ok_or(BscBlockExecutionError::ApplySnapshotFailed)?;

            cache.put(snap.block_hash, snap.clone());
        }

        Ok(snap)
    }

    fn get_header_by_hash(
        &self,
        block_hash: B256,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
    ) -> Result<Header, BlockExecutionError> {
        ancestor
            .and_then(|m| m.get(&block_hash).cloned())
            .or_else(|| {
                self.provider
                    .header(&block_hash)
                    .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })
                    .ok()
                    .flatten()
            })
            .ok_or_else(|| BscBlockExecutionError::UnknownHeader { block_hash }.into())
    }

    fn find_ancient_header(
        &self,
        header: &Header,
        ancestor: Option<&alloy_primitives::map::HashMap<B256, Header>>,
        count: u64,
    ) -> Result<Header, BlockExecutionError> {
        let mut result = header.clone();
        for _ in 0..count {
            result = self.get_header_by_hash(result.parent_hash, ancestor)?;
        }
        Ok(result)
    }
}
