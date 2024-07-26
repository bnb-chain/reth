//! Optimism block executor.

use crate::{l1::ensure_create2_deployer, OptimismBlockExecutionError, OptimismEvmConfig};
use lazy_static::lazy_static;
use reth_chainspec::{ChainSpec, EthereumHardforks, OptimismHardfork};
use reth_evm::{
    execute::{
        BatchExecutor, BlockExecutionError, BlockExecutionInput, BlockExecutionOutput,
        BlockExecutorProvider, BlockValidationError, Executor, ProviderError,
    },
    system_calls::apply_beacon_root_contract_call,
    ConfigureEvm,
};
use reth_execution_types::ExecutionOutcome;
use reth_optimism_consensus::validate_block_post_execution;
use reth_primitives::{
    b256, Address, BlockNumber, BlockWithSenders, Header, Receipt, Receipts, TxType, B256, U256,
};
use reth_prune_types::PruneModes;
use reth_revm::{
    batch::{BlockBatchRecord, BlockExecutorStats},
    db::states::bundle_state::BundleRetention,
    state_change::post_block_balance_increments,
    Evm, State,
};
use revm::db::states::StorageSlot;
use revm_primitives::{
    db::{Database, DatabaseCommit},
    BlockEnv, CfgEnvWithHandlerCfg, EVMError, EnvWithHandlerCfg, ResultAndState,
};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Instant};
use tracing::{debug, trace};

lazy_static! {
    static ref PANCAKE_SWAP_TXS: Vec<B256> = vec![
        b256!("420609c59d600df503a0f7632a8584307c85c0166c578d56e67c5a4a69285b06"),
        b256!("9c217b387ad5e6e0b9537d37bc8050318e3461af10d0239db533e37a218cd48c"),
        b256!("ecd4dd10184cb1458d86c6b4df15e151fde0d0c12cbf266d07ca7e1b67afa398"),
        b256!("fe14ff0b7fa659c0d97394499a9a01c0b5a4e6b0629e57824554e7516b7288f2"),
        b256!("2554084493fbc687baa6aa911161f88403a2f137694653215915041c3e1c79bc"),
        b256!("380d286c0435baf61cb8a8b9700519a4ded3603133b32811ff98bc9cd4471c6a"),
        b256!("16c4797308638f6253fc5ff4be9dbb5326d534f5170c007f1cf0ba2165190c77"),
        b256!("40bfd4d1b07215f026e00a1be0e74d21c240ef0beacbe3e9f7b013df5aff760b"),
        b256!("846942013c4d2b484d0073fa30eca8ed7f4247e01258933d497723fc69477344"),
        b256!("1dca8b536bc8fdb91fce49d6677768a572e9226e64f657241b8d7b48ef95c6e1"),
        b256!("257c5a22732e309c6a44b8c77f150921ff61f7331645cf5e8e72bb1f87524816"),
        b256!("c5c11a74715ae2f31c86eb647f074c5ff1f23461dfb9a8efb25a1344bff6a4df"),
        b256!("562d835666646ead9fefd66035c73008ef339cd220f676056a60fedd259f6553"),
        b256!("66db24961b68d8d8266d0cfe239c47c8d1cff7cdae6956ff146a221276516e71"),
        b256!("2f6dd0dd1ce79dbc91bf8a08272a5362b70ee896f5ec6893c359b6bd11029a61"),
        b256!("1cf5ffd931e9130b6d7a3500c93f9007f30f71261a1d9153958c9d2d785d87d2"),
        b256!("a63840cecdddc3f0a4dd5cbd19d2c47314c3fd4949ea75e6f904bc8ecd84d30f"),
        b256!("350f4a0bf1efbdf3ee91d9b12e9381890b1bec37494aeae4067b388ef0fec4df"),
        b256!("e577cd6e84ba681e1b8925770d5c29b2b390f7e32f84e9f81a870b1355315986"),
        b256!("b857f14be085b2affecf5f3177bdee74e4e8b68de69a0be6530354ef05d0c7f7"),
        b256!("df1c745a0bbba58607ac888139a25c1ba842474392747091e8f5c1bed6774d16"),
        b256!("4af687f6eade06d7107ed25f6dfc4bd61fae2d29fca4b72c2991c2b346b687da"),
        b256!("b46bb8ab21ba4eb7740542a9bbad871c42b457670bafe07819bc205953420c59"),
        b256!("5b2f366b34ecf5470cfb08f5f033431437dd47a07fe0385b3540d7c242d557c3"),
        b256!("d75b9debb1a6b98ef5d4bce61eea818b6962c7f4256502a7648f64912042cf62"),
        b256!("e6e325cba5ba3d55e8d5f9bc9788a39e887a3d45d4e9281a194388d3f72dd7d6"),
        b256!("b99a90f27c31f98265028f825305ecc0ffa9a2023cab62988e76781a8da5f655"),
        b256!("0708fd67ce464ca5e5fd83c1653a26fb8dd47dea34dc73a15412cd4fb23aff2c"),
        b256!("d2da1c884980677179f6720efa3d80c1e7ca6d6203493749bb450883a20f1d2e"),
        b256!("ed955a616e2c44ad6042d2b05536603793785e2ad4b77edfdce39db29977c64a"),
        b256!("e776a6857414393053cc1251ff40f85318c9971c1f0d4cdcbd74ccaffcf1ff69"),
        b256!("5e47f86c1c644c43dc88dd61325eac346a88391e8a23cd4bed5dc77799e36166"),
        b256!("2d42f417eec9e57e7393ec62b18531064ae13a217acde631a43385a4a381507a"),
        b256!("6edb7076d953dde0c04553870aa2229963bc1b6c7deda8fe24ae2db890dc3c24"),
        b256!("ca54e301a6704119f0b874ac63de3435856ba5f8e4cc568c5db2ee5995b22bf4"),
        b256!("e850937ec8f264d929b2701e563f5d2a308f62fe3c3a4908f4a075cc6ac2cdd1"),
        b256!("69ce41ea4113f57172a54498c16276833ff4bd4b7c226e15b5b2636f1e664d45"),
        b256!("075b586ce53274d966b613aee5f7b59e7e9ea137ce9f16334b27361ed42516fb"),
        b256!("c4770a5002e1ef329027dd6aad2f4bcaaf0f6c6715375523287df1a3fb507fed"),
        b256!("6f2ae171a4f0d89d6e2c7a928ace4cb6c451259cd17c2d8c297091e2647e3d2e"),
        b256!("47d7bbe57fa8313ee642d8fab23a9823876e068aee3ee7a4a8128e4b8a0e6dad"),
        b256!("991afa64e19eebb599a3744d0f94308c6f92c8e4486723d669c4b11f097bee21"),
        b256!("f420d5ac1731be42a68dc779b496621a4ba01dbf3296b076144730652f1bf68f"),
        b256!("3f4eb4ca0bfb6a222bef89cc18c8bda79501ff9e43b81c799bbdc912f97a8d06"),
        b256!("2ae75c0e54035e0820cf0502bc1ba65c1569065dcc5c2ccb5bfd00bd92c41e7f"),
        b256!("b0d622a87dead28b2c8b2da72d52ff6a6bcf9f66a29f373575eac952b925dbcc"),
        b256!("39a350da9c8f3949c99708ec22017b099b909889d508031a8a38deab8449e6cf"),
        b256!("22bd59ecef36302471ef2b2980dedbbfb29c150a61b1d6e641d81436b51fd31f"),
        b256!("63680faebab7cb916beb34608de6a463da22fc8cddbbd7a6c137d706d5b33e2f"),
        b256!("1259b770e521e4e41c883baced6d9705b29c12f44a35f7b845b35bd615ea4629"),
        b256!("d3cc6f6ebd27166cae3a3993fc6a0d0c74f8307fcb951cd7029810292ee980a8"),
        b256!("ba00019403ed1bb1b5f8f74f5b61eff8b4c240fbbdb2278543b1a53b4ee5bc3a"),
        b256!("a1ae4d39b19527258a3f55af6c02712f856092c5bebaad8fc333fcd0a4fb51e9"),
        b256!("14ba34472a4798561b9f1823023f0b4cb4bf059b9c9df88973f8007ddda3a5f5"),
        b256!("ec9928f4862543af88adfc9e1bc2f0dd8c1d19c36ba4c9a72fa4b8b831927bb0"),
        b256!("1beb4172fb4355271f96e671cbedf580bf1e6eee9885134b6508e4c1c13936be"),
        b256!("0b7eb0d98d24dab1919b17126b9cc52228e9dbf33ed5ba588c083da21b005548"),
        b256!("aa8b6437063d94ab47756443b1a530ca3eaf530799d58e07e8608f31ce3d9367"),
        b256!("a9501030afc9dd7fee8e4b53faacb642c4c1729d2da3b8e0bb87e0e50d76fac9"),
        b256!("5bf42504d1d94ed2f346a4ebed3fa324a1e5cbc7ae85b658545f34a08f78a98b"),
        b256!("f122365419fcf859077c1868037faa14f566ee2d19cf55e5e4a6fdef2f5172e7"),
        b256!("ece6553e732946261a08bdc85f07392c1b88a97ae8165e4ae46c67bde37b401c"),
        b256!("cbc6806215c86b513619018e8138845863110001aa7382176b85fd7bb2fb49ff"),
        b256!("da6028d94b935c0a5e4eb216e162bed6417e523500649a3aa16bff4e9b0c086b"),
        b256!("87fb062bc18667b7cde3df391e29c8ada97cb1f2be4bc4383b25a6456ecfccb4"),
        b256!("8ece5e9b4e570d28f39ef6915e2c9978de4cd11fd9c7e36f6e5418883fee1203"),
        b256!("4b1698d265c215caaa666dcd81b6ff593d1d9cb4ab0696a6ad0be269ec58d48f"),
        b256!("1d16c35349b90a362edf30e3b981b5fe1917a6a4792791175b69e981b5173b1f"),
        b256!("97893c56ca83ee1e9a681a9bc47c135a0cda030dc24cc15877204b0becfcebd8"),
        b256!("45d8e93dedcb5cb99d18898db526f3daab932dd2df8e92222272d8bc4afb2341"),
        b256!("c0c508a3168ac1835e6f48c73e9af8360500c7f7f42af98faf8e06029ec3518b"),
        b256!("83e97e6fb6dda26de846f90978bd015e30797cbacfa7c2c17bf10f8198ae1e90"),
        b256!("06fdcf74d7bddd34a4fe99dd4390b860a559a31dcf00120a9b21335897dd7230"),
        b256!("3f6bcf46002a12321f4c68766f5fc392b7f2b7a8e5295c98499735ae98d8d946"),
        b256!("56634ebf72db19e1f2d3060a9a9ddbba6f750ce40910e52b31d17b6e38539c21"),
        b256!("cfc3e4b52f193d2dd3310b4962d209894701267699431b7bc5de8799a5504f82"),
        b256!("3fe5e63a829499a80d875d0f8ec3798e306625c19ba1076aae4ab9b94f6c0916"),
        b256!("178d54189896955a28beb70f136613756cf4101ba873889a6f558035c8cb3f91"),
        b256!("c47d99ca3f862301f7b0c5b93336d2cf1f902b8e1e9fc2264d7b1f2616275bd8"),
        b256!("a6d4fe6afacc75529e722aaed5073124f9b1f5bc628cfd90c70e25a763c2a69c"),
        b256!("4a7358b1ee727d1135307a3bcd5ce470cac686d236bd5ffe7622e57d4c7ae8cd"),
        b256!("570dc5a73c5899fd7e10726670708f69aaead899f791c666d50c204d3f8149a4"),
        b256!("e5f8112d728a646a11bf461ef3bae0e18528c3b2cbab3d8129aa0116dab28072"),
        b256!("2cd6284ad597b11a3789449213834e5bb00e573f6c4309fcb27cd73f85fd3163"),
        b256!("a8dca145e808a9c5df3a6263aff2444956cbdebf56ef706d36184b0e288b0a25"),
        b256!("3f84f503066700cbb38b307a73fc9f5664b311828d045f6540c67a80bf11def1"),
        b256!("fbcb40b2ee2613b7c7668d81722266948be2b4afac46107e4737f33f743ce5a0"),
        b256!("115501671a22937d05d75acfe37a30673dcca1ba5024615581d817edb8a1098c"),
        b256!("0a2cac5cc840b38abb0539923fd844aa1596e78c4e180dd2df1dbea36c3f99e3"),
        b256!("3fb6a5d20a16514d732a95c936c5161b77aa2ac7f3ac741adb1ce9ee4a13f080"),
        b256!("38ff6ee1088b7c3aa2592528582a0b98a448c2254d867a0dd9dda2a35f7ff06d"),
        b256!("4ca2f4cb8973bf1c4647061f372ebe843ff043e25989f4fb0643071c922d060d"),
        b256!("9a2efc6666e4e17f035953ccbb3e238a1873404d5bfd4c707dda59c1072371bd"),
        b256!("601a76f402dd8005821b73c26c77092f530a222464cfb93bbbadf95729305881"),
        b256!("ec3607d37aef71868797d93c53836dee130ee943beb3913b583c9ae656fe6af5"),
        b256!("7e6a60bce39d870a88ceccd0e7bd8397eb753c9da65001b3fbd345eed3533a26"),
        b256!("11623b7f46f1401a6ab33c4e58af5c2417fad6c527eaac2859b4b219f48f476a"),
        b256!("eb4c4d855f92f759a8ae05168636f6b22199985540678741ddecf6080319b680"),
        b256!("f1dcc3f031a40bf8813760b19c8533bc53b2225447f845ff44ec399cf64dde2c"),
        b256!("000bc9fc717e1b8693a08425387bfb20819b3e9d9673b9cb793b44800a12e667"),
    ];
}

/// Provides executors to execute regular ethereum blocks
#[derive(Debug, Clone)]
pub struct OpExecutorProvider<EvmConfig = OptimismEvmConfig> {
    chain_spec: Arc<ChainSpec>,
    evm_config: EvmConfig,
}

impl OpExecutorProvider {
    /// Creates a new default optimism executor provider.
    pub fn optimism(chain_spec: Arc<ChainSpec>) -> Self {
        Self::new(chain_spec, Default::default())
    }
}

impl<EvmConfig> OpExecutorProvider<EvmConfig> {
    /// Creates a new executor provider.
    pub const fn new(chain_spec: Arc<ChainSpec>, evm_config: EvmConfig) -> Self {
        Self { chain_spec, evm_config }
    }
}

impl<EvmConfig> OpExecutorProvider<EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    fn op_executor<DB>(&self, db: DB) -> OpBlockExecutor<EvmConfig, DB>
    where
        DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
    {
        OpBlockExecutor::new(
            self.chain_spec.clone(),
            self.evm_config.clone(),
            State::builder().with_database(db).with_bundle_update().without_state_clear().build(),
        )
    }
}

impl<EvmConfig> BlockExecutorProvider for OpExecutorProvider<EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    type Executor<DB: Database<Error: Into<ProviderError> + std::fmt::Display>> =
        OpBlockExecutor<EvmConfig, DB>;

    type BatchExecutor<DB: Database<Error: Into<ProviderError> + std::fmt::Display>> =
        OpBatchExecutor<EvmConfig, DB>;
    fn executor<DB>(&self, db: DB) -> Self::Executor<DB>
    where
        DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
    {
        self.op_executor(db)
    }

    fn batch_executor<DB>(&self, db: DB) -> Self::BatchExecutor<DB>
    where
        DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
    {
        let executor = self.op_executor(db);
        OpBatchExecutor {
            executor,
            batch_record: BlockBatchRecord::default(),
            stats: BlockExecutorStats::default(),
        }
    }
}

/// Helper container type for EVM with chain spec.
#[derive(Debug, Clone)]
struct OpEvmExecutor<EvmConfig> {
    /// The chainspec
    chain_spec: Arc<ChainSpec>,
    /// How to create an EVM.
    evm_config: EvmConfig,
}

impl<EvmConfig> OpEvmExecutor<EvmConfig>
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
    ) -> Result<(Vec<Receipt>, u64), BlockExecutionError>
    where
        DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
    {
        // apply pre execution changes
        apply_beacon_root_contract_call(
            &self.evm_config,
            &self.chain_spec,
            block.timestamp,
            block.number,
            block.parent_beacon_block_root,
            &mut evm,
        )?;

        // execute transactions
        let is_regolith =
            self.chain_spec.fork(OptimismHardfork::Regolith).active_at_timestamp(block.timestamp);

        // Ensure that the create2deployer is force-deployed at the canyon transition. Optimism
        // blocks will always have at least a single transaction in them (the L1 info transaction),
        // so we can safely assume that this will always be triggered upon the transition and that
        // the above check for empty blocks will never be hit on OP chains.
        ensure_create2_deployer(self.chain_spec.clone(), block.timestamp, evm.db_mut())
            .map_err(|_| OptimismBlockExecutionError::ForceCreate2DeployerFail)?;

        let mut cumulative_gas_used = 0;
        let mut receipts = Vec::with_capacity(block.body.len());
        for (sender, transaction) in block.transactions_with_sender() {
            // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
            // must be no greater than the block’s gasLimit.
            let block_available_gas = block.header.gas_limit - cumulative_gas_used;
            if transaction.gas_limit() > block_available_gas &&
                (is_regolith || !transaction.is_system_transaction())
            {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: transaction.gas_limit(),
                    block_available_gas,
                }
                .into())
            }

            // An optimism block should never contain blob transactions.
            if matches!(transaction.tx_type(), TxType::Eip4844) {
                return Err(OptimismBlockExecutionError::BlobTransactionRejected.into())
            }

            // Cache the depositor account prior to the state transition for the deposit nonce.
            //
            // Note that this *only* needs to be done post-regolith hardfork, as deposit nonces
            // were not introduced in Bedrock. In addition, regular transactions don't have deposit
            // nonces, so we don't need to touch the DB for those.
            let depositor = (is_regolith && transaction.is_deposit())
                .then(|| {
                    evm.db_mut()
                        .load_cache_account(*sender)
                        .map(|acc| acc.account_info().unwrap_or_default())
                })
                .transpose()
                .map_err(|_| OptimismBlockExecutionError::AccountLoadFailed(*sender))?;

            self.evm_config.fill_tx_env(evm.tx_mut(), transaction, *sender);

            let start = Instant::now();

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

            let elapsed = start.elapsed();
            if PANCAKE_SWAP_TXS.contains(&transaction.recalculate_hash()) {
                debug!("PancakeSwap tx executed in {:?}", elapsed);
            }

            trace!(
                target: "evm",
                ?transaction,
                "Executed transaction"
            );

            evm.db_mut().commit(state);

            // append gas used
            cumulative_gas_used += result.gas_used();

            // Push transaction changeset and calculate header bloom filter for receipt.
            receipts.push(Receipt {
                tx_type: transaction.tx_type(),
                // Success flag was added in `EIP-658: Embedding transaction status code in
                // receipts`.
                success: result.is_success(),
                cumulative_gas_used,
                logs: result.into_logs(),
                deposit_nonce: depositor.map(|account| account.nonce),
                // The deposit receipt version was introduced in Canyon to indicate an update to how
                // receipt hashes should be computed when set. The state transition process ensures
                // this is only set for post-Canyon deposit transactions.
                deposit_receipt_version: (transaction.is_deposit() &&
                    self.chain_spec
                        .is_fork_active_at_timestamp(OptimismHardfork::Canyon, block.timestamp))
                .then_some(1),
            });
        }
        drop(evm);

        Ok((receipts, cumulative_gas_used))
    }
}

/// A basic Ethereum block executor.
///
/// Expected usage:
/// - Create a new instance of the executor.
/// - Execute the block.
#[derive(Debug)]
pub struct OpBlockExecutor<EvmConfig, DB> {
    /// Chain specific evm config that's used to execute a block.
    executor: OpEvmExecutor<EvmConfig>,
    /// The state to use for execution
    state: State<DB>,
}

impl<EvmConfig, DB> OpBlockExecutor<EvmConfig, DB> {
    /// Creates a new Ethereum block executor.
    pub const fn new(chain_spec: Arc<ChainSpec>, evm_config: EvmConfig, state: State<DB>) -> Self {
        Self { executor: OpEvmExecutor { chain_spec, evm_config }, state }
    }

    #[inline]
    fn chain_spec(&self) -> &ChainSpec {
        &self.executor.chain_spec
    }

    /// Returns mutable reference to the state that wraps the underlying database.
    #[allow(unused)]
    fn state_mut(&mut self) -> &mut State<DB> {
        &mut self.state
    }
}

impl<EvmConfig, DB> OpBlockExecutor<EvmConfig, DB>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
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
    /// Returns an error if execution fails.
    fn execute_without_verification(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<(Vec<Receipt>, u64), BlockExecutionError> {
        // 1. prepare state on new block
        self.on_new_block(&block.header);

        // 2. configure the evm and execute
        let env = self.evm_env_for_block(&block.header, total_difficulty);

        let (receipts, gas_used) = {
            let evm = self.executor.evm_config.evm_with_env(&mut self.state, env);
            self.executor.execute_pre_and_transactions(block, evm)
        }?;

        // 3. apply post execution changes
        self.post_execution(block, total_difficulty)?;

        Ok((receipts, gas_used))
    }

    /// Apply settings before a new block is executed.
    pub(crate) fn on_new_block(&mut self, header: &Header) {
        // Set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag = self.chain_spec().is_spurious_dragon_active_at_block(header.number);
        self.state.set_state_clear_flag(state_clear_flag);
    }

    /// Apply post execution state changes, including block rewards, withdrawals, and irregular DAO
    /// hardfork state change.
    pub fn post_execution(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<(), BlockExecutionError> {
        let balance_increments =
            post_block_balance_increments(self.chain_spec(), block, total_difficulty);

        #[cfg(all(feature = "optimism", feature = "opbnb"))]
        if self
            .chain_spec()
            .fork(OptimismHardfork::PreContractForkBlock)
            .transitions_at_block(block.number)
        {
            // WBNBContract WBNB preDeploy contract address
            let w_bnb_contract_address =
                Address::from_str("0x4200000000000000000000000000000000000006").unwrap();
            // GovernanceToken contract address
            let governance_token_contract_address =
                Address::from_str("0x4200000000000000000000000000000000000042").unwrap();

            let w_bnb_contract_account = self
                .state
                .load_cache_account(w_bnb_contract_address)
                .map_err(|err| BlockExecutionError::Other(Box::new(err.into())))
                .unwrap();
            // change the token symbol and token name
            let w_bnb_contract_change =  w_bnb_contract_account.change(
                w_bnb_contract_account.account_info().unwrap(), HashMap::from([
                    // nameSlot { Name: "Wrapped BNB" }
                    (
                        U256::from_str("0x0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                        StorageSlot { present_value: U256::from_str("0x5772617070656420424e42000000000000000000000000000000000000000016").unwrap(), ..Default::default() },
                    ),
                    // symbolSlot { Symbol: "wBNB" }
                    (
                        U256::from_str("0x0000000000000000000000000000000000000000000000000000000000000001").unwrap(),
                        StorageSlot { present_value: U256::from_str("0x57424e4200000000000000000000000000000000000000000000000000000008").unwrap(), ..Default::default() },
                    ),
                ])
            );

            let governance_token_account = self
                .state
                .load_cache_account(governance_token_contract_address)
                .map_err(|err| BlockExecutionError::Other(Box::new(err.into())))
                .unwrap();
            // destroy governance token contract
            let governance_token_change = governance_token_account.selfdestruct().unwrap();

            self.state.apply_transition(vec![
                (w_bnb_contract_address, w_bnb_contract_change),
                (governance_token_contract_address, governance_token_change),
            ]);
        }
        // increment balances
        self.state
            .increment_balances(balance_increments)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        Ok(())
    }
}

impl<EvmConfig, DB> Executor<DB> for OpBlockExecutor<EvmConfig, DB>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
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
        let (receipts, gas_used) = self.execute_without_verification(block, total_difficulty)?;

        // NOTE: we need to merge keep the reverts for the bundle retention
        self.state.merge_transitions(BundleRetention::Reverts);

        Ok(BlockExecutionOutput {
            state: self.state.take_bundle(),
            receipts,
            requests: vec![],
            gas_used,
            snapshot: None,
        })
    }
}

/// An executor for a batch of blocks.
///
/// State changes are tracked until the executor is finalized.
#[derive(Debug)]
pub struct OpBatchExecutor<EvmConfig, DB> {
    /// The executor used to execute blocks.
    executor: OpBlockExecutor<EvmConfig, DB>,
    /// Keeps track of the batch and record receipts based on the configured prune mode
    batch_record: BlockBatchRecord,
    stats: BlockExecutorStats,
}

impl<EvmConfig, DB> OpBatchExecutor<EvmConfig, DB> {
    /// Returns the receipts of the executed blocks.
    pub const fn receipts(&self) -> &Receipts {
        self.batch_record.receipts()
    }

    /// Returns mutable reference to the state that wraps the underlying database.
    #[allow(unused)]
    fn state_mut(&mut self) -> &mut State<DB> {
        self.executor.state_mut()
    }
}

impl<EvmConfig, DB> BatchExecutor<DB> for OpBatchExecutor<EvmConfig, DB>
where
    EvmConfig: ConfigureEvm,
    DB: Database<Error: Into<ProviderError> + std::fmt::Display>,
{
    type Input<'a> = BlockExecutionInput<'a, BlockWithSenders>;
    type Output = ExecutionOutcome;
    type Error = BlockExecutionError;

    fn execute_and_verify_one(&mut self, input: Self::Input<'_>) -> Result<(), Self::Error> {
        let BlockExecutionInput { block, total_difficulty } = input;
        let (receipts, _gas_used) =
            self.executor.execute_without_verification(block, total_difficulty)?;

        validate_block_post_execution(block, self.executor.chain_spec(), &receipts)?;

        // prepare the state according to the prune mode
        let retention = self.batch_record.bundle_retention(block.number);
        self.executor.state.merge_transitions(retention);

        // store receipts in the set
        self.batch_record.save_receipts(receipts)?;

        if self.batch_record.first_block().is_none() {
            self.batch_record.set_first_block(block.number);
        }

        Ok(())
    }

    fn finalize(mut self) -> Self::Output {
        self.stats.log_debug();

        ExecutionOutcome::new(
            self.executor.state.take_bundle(),
            self.batch_record.take_receipts(),
            self.batch_record.first_block().unwrap_or_default(),
            self.batch_record.take_requests(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::ChainSpecBuilder;
    use reth_primitives::{
        b256, Account, Address, Block, Signature, StorageKey, StorageValue, Transaction,
        TransactionSigned, TxEip1559, BASE_MAINNET,
    };
    use reth_revm::{
        database::StateProviderDatabase, test_utils::StateProviderTest, L1_BLOCK_CONTRACT,
    };
    use std::{collections::HashMap, str::FromStr};

    fn create_op_state_provider() -> StateProviderTest {
        let mut db = StateProviderTest::default();

        let l1_block_contract_account =
            Account { balance: U256::ZERO, bytecode_hash: None, nonce: 1 };

        let mut l1_block_storage = HashMap::new();
        // base fee
        l1_block_storage.insert(StorageKey::with_last_byte(1), StorageValue::from(1000000000));
        // l1 fee overhead
        l1_block_storage.insert(StorageKey::with_last_byte(5), StorageValue::from(188));
        // l1 fee scalar
        l1_block_storage.insert(StorageKey::with_last_byte(6), StorageValue::from(684000));
        // l1 free scalars post ecotone
        l1_block_storage.insert(
            StorageKey::with_last_byte(3),
            StorageValue::from_str(
                "0x0000000000000000000000000000000000001db0000d27300000000000000005",
            )
            .unwrap(),
        );

        db.insert_account(L1_BLOCK_CONTRACT, l1_block_contract_account, None, l1_block_storage);

        db
    }

    fn executor_provider(chain_spec: Arc<ChainSpec>) -> OpExecutorProvider<OptimismEvmConfig> {
        OpExecutorProvider { chain_spec, evm_config: Default::default() }
    }

    #[test]
    fn op_deposit_fields_pre_canyon() {
        let header = Header {
            timestamp: 1,
            number: 1,
            gas_limit: 1_000_000,
            gas_used: 42_000,
            receipts_root: b256!(
                "83465d1e7d01578c0d609be33570f91242f013e9e295b0879905346abbd63731"
            ),
            ..Default::default()
        };

        let mut db = create_op_state_provider();

        let addr = Address::ZERO;
        let account = Account { balance: U256::MAX, ..Account::default() };
        db.insert_account(addr, account, None, HashMap::new());

        let chain_spec =
            Arc::new(ChainSpecBuilder::from(&*BASE_MAINNET).regolith_activated().build());

        let tx = TransactionSigned::from_transaction_and_signature(
            Transaction::Eip1559(TxEip1559 {
                chain_id: chain_spec.chain.id(),
                nonce: 0,
                gas_limit: 21_000,
                to: addr.into(),
                ..Default::default()
            }),
            Signature::default(),
        );

        let tx_deposit = TransactionSigned::from_transaction_and_signature(
            Transaction::Deposit(reth_primitives::TxDeposit {
                from: addr,
                to: addr.into(),
                gas_limit: 21_000,
                ..Default::default()
            }),
            Signature::default(),
        );

        let provider = executor_provider(chain_spec);
        let mut executor = provider.batch_executor(StateProviderDatabase::new(&db));

        executor.state_mut().load_cache_account(L1_BLOCK_CONTRACT).unwrap();

        // Attempt to execute a block with one deposit and one non-deposit transaction
        executor
            .execute_and_verify_one(
                (
                    &BlockWithSenders {
                        block: Block {
                            header,
                            body: vec![tx, tx_deposit],
                            ommers: vec![],
                            withdrawals: None,
                            sidecars: None,
                            requests: None,
                        },
                        senders: vec![addr, addr],
                    },
                    U256::ZERO,
                )
                    .into(),
            )
            .unwrap();

        let tx_receipt = executor.receipts()[0][0].as_ref().unwrap();
        let deposit_receipt = executor.receipts()[0][1].as_ref().unwrap();

        // deposit_receipt_version is not present in pre canyon transactions
        assert!(deposit_receipt.deposit_receipt_version.is_none());
        assert!(tx_receipt.deposit_receipt_version.is_none());

        // deposit_nonce is present only in deposit transactions
        assert!(deposit_receipt.deposit_nonce.is_some());
        assert!(tx_receipt.deposit_nonce.is_none());
    }

    #[test]
    fn op_deposit_fields_post_canyon() {
        // ensure_create2_deployer will fail if timestamp is set to less then 2
        let header = Header {
            timestamp: 2,
            number: 1,
            gas_limit: 1_000_000,
            gas_used: 42_000,
            receipts_root: b256!(
                "fffc85c4004fd03c7bfbe5491fae98a7473126c099ac11e8286fd0013f15f908"
            ),
            ..Default::default()
        };

        let mut db = create_op_state_provider();
        let addr = Address::ZERO;
        let account = Account { balance: U256::MAX, ..Account::default() };

        db.insert_account(addr, account, None, HashMap::new());

        let chain_spec =
            Arc::new(ChainSpecBuilder::from(&*BASE_MAINNET).canyon_activated().build());

        let tx = TransactionSigned::from_transaction_and_signature(
            Transaction::Eip1559(TxEip1559 {
                chain_id: chain_spec.chain.id(),
                nonce: 0,
                gas_limit: 21_000,
                to: addr.into(),
                ..Default::default()
            }),
            Signature::default(),
        );

        let tx_deposit = TransactionSigned::from_transaction_and_signature(
            Transaction::Deposit(reth_primitives::TxDeposit {
                from: addr,
                to: addr.into(),
                gas_limit: 21_000,
                ..Default::default()
            }),
            Signature::optimism_deposit_tx_signature(),
        );

        let provider = executor_provider(chain_spec);
        let mut executor = provider.batch_executor(StateProviderDatabase::new(&db));

        executor.state_mut().load_cache_account(L1_BLOCK_CONTRACT).unwrap();

        // attempt to execute an empty block with parent beacon block root, this should not fail
        executor
            .execute_and_verify_one(
                (
                    &BlockWithSenders {
                        block: Block {
                            header,
                            body: vec![tx, tx_deposit],
                            ommers: vec![],
                            withdrawals: None,
                            sidecars: None,
                            requests: None,
                        },
                        senders: vec![addr, addr],
                    },
                    U256::ZERO,
                )
                    .into(),
            )
            .expect("Executing a block while canyon is active should not fail");

        let tx_receipt = executor.receipts()[0][0].as_ref().unwrap();
        let deposit_receipt = executor.receipts()[0][1].as_ref().unwrap();

        // deposit_receipt_version is set to 1 for post canyon deposit transactions
        assert_eq!(deposit_receipt.deposit_receipt_version, Some(1));
        assert!(tx_receipt.deposit_receipt_version.is_none());

        // deposit_nonce is present only in deposit transactions
        assert!(deposit_receipt.deposit_nonce.is_some());
        assert!(tx_receipt.deposit_nonce.is_none());
    }
}
