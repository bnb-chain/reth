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
        b256!("d75b9debb1a6b98ef5d4bce61eea818b6962c7f4256502a7648f64912042cf62"),
        b256!("e6e325cba5ba3d55e8d5f9bc9788a39e887a3d45d4e9281a194388d3f72dd7d6"),
        b256!("23ef8321624271f84b5800496dca62e4f11aa2a822cfabd562d688c24c590c65"),
        b256!("3131865c7e2b1c000516d17ef3fab91e289733b430a780c865c9c7e7fe1a5cf1"),
        b256!("0eac2c3aa92f8efea7266a889b655814c93c9bf937701453e2a64017ececa065"),
        b256!("0dd99ef667750e130d3b0cccd03d814c79aeec8238e6aa07c861dfa8111eedee"),
        b256!("287441befd88d745439dffb735723face67969cac90f70541e3e69b9df54e644"),
        b256!("15680348d0aafda5fa3e60180ec71bf47c27e86b254823d4c7fc059df00ec095"),
        b256!("cb61d59a662339c4f682f8cd5fa4829d8624b996e0e83581b3d33f6136570986"),
        b256!("0807139ad6f8f24d319f5a6b9d0570c3772db374017645683cfa04aee581762a"),
        b256!("455a9aaf7f1c2dcf9efb7d7fd893a9740f6781e0496d27b94b9f4acd6fc3d790"),
        b256!("0fc4a22733b348cfd09aa237cbbffcc2fca3d3744b1c792a987d2f36626d2efc"),
        b256!("751fbafc0781320ac2db6f08f7a5312ead86cc41451e0c60137c29a76a323678"),
        b256!("f153b0897e16c048f254cd93e26ea536de48c4e5ff0ae7b1fa3a97c5d9257eca"),
        b256!("aa6ce6a5bacf5c1bae94032a88519110145ebfa69d04d7d011c9e4c02a1331b1"),
        b256!("102576b7ae658bbb9409c56819eef8c35f2889a34c575053073e059126b6b94f"),
        b256!("ff85b9e2d9448870acfb9b6221c09ad9409f5c95229f90ef700f2bae3b179388"),
        b256!("a5260f4af19151e7ec24203fbd6de04f956846f3a0b95590b18fa61165cf8e42"),
        b256!("480560b9f5979794f78a39de0d4bcf3e1cf1d18713592b018223a5582f7c44d1"),
        b256!("de13ad57c7b36689240b6c502737e2ccef83b090b6d01587dc68f2a75a0e8b4b"),
        b256!("cac415e6c7fae4a8287a18e7c97d87c935d3ccd3f52ca4186df74a133904dc96"),
        b256!("d7fa1cfae97d760f9bb784a7d2cd076c03b8c16bbc4b0ba50f77b8f60d9790e8"),
        b256!("28224fefc30d2002d566fbc439a44bf07d5a1824037d0a5499a91d25faaad675"),
        b256!("ab05b1d9b319b1fa4ae1f1777a6cea77e8df07adb949c4f583474e45b6c58976"),
        b256!("23ffecf84409fb41ab957d132df5cc4ad830e5757a4ba29a979227826eaa98be"),
        b256!("e8016690275e3c55ed52ac419fe48efeca3aa5bb0a76547c324ee011d22b1bf2"),
        b256!("94610fbc6287273d56379fce06b75d0a5d10c29ed865ebedc405012c3cf4ea61"),
        b256!("7051d667fb45ba17c9ffafd82de0e19135bfa6838c7e00b3369f4754f5259e04"),
        b256!("217f517553f58081843f7fb8cc1bc80377501d5309fc6246f6372dbd2843ff39"),
        b256!("600668f9690cf748422b69cfc36e6e9d22a53db0c5874563de66988079d8deb7"),
        b256!("15903c23ca1724cfb727768d9fcc65405717f2649e9c038866abc85a7b2405f7"),
        b256!("5bed0536e8e5067a172ab92c81029f33bcc847f6d22b8348d2e1323984fe9fef"),
        b256!("cdf44cf95b2678c03ea2bb45f5021b5b9d7fa2c096d665b26101b307eee03c5e"),
        b256!("560fddc209abad630cc8157b915fe74c03bb7e462c8a8f3d0c8051da5bdccd02"),
        b256!("c42e221d0f32b8d1b1eaf9042490c42f61fb92553ee180465184ccaa9a1a34da"),
        b256!("ec03f5dbecdeed4ed74ba67c033040f72d3c288413dbe5c61d780fb1b9888d4f"),
        b256!("80c73c518685810c591280c684f11778ac9bb73cbb60ec67b0e253e44312dc22"),
        b256!("9fadac0a68b471f6ed8889fbe016e6603670564843404a040e5706192a0d385e"),
        b256!("bab4f4ea438f1bb6010e03b1a04b812ef215b6b22ff07af25f2914bdbcbc6a08"),
        b256!("27ff6d11c643640d3abeff32e3c4b9bb85b3ea94406ba10973b752b001bad5b8"),
        b256!("45aad2b83c29dff01fe3f83ee6bf845ca593a8d5984e332ef594a1b1e044cb00"),
        b256!("f4828257c18d7f940374a76c006d931a9f2831be90e9cc6f5487e5100e46d4eb"),
        b256!("d0a68ac6dc71c79ea01c0e31330a63bc64cf946c0e29a151115c6ed6181139d2"),
        b256!("3dfd5f54df6538fa281ff2545719d92c4bcd2fbf730921bacab9e5f076c96833"),
        b256!("6e6dbf11e79eb17102eb40520892fa238d808d8f5135d58ce42b4e71df24b5ba"),
        b256!("466d5ba092ed3fcd9233dfebb8e81f8cce41036e311df956bc05bd8d0f2b4800"),
        b256!("276aef65c5c260b6d7e5cf4028e58ec3565f23dc1f8e8057640cde7ffd472585"),
        b256!("49ee9aede2e71464813ed609e61e72ed764724116683af9e611325e46fc9c9bf"),
        b256!("82a514e00ffcfd353b6e645d1e3335d180317d9b65fa791797e860b2d3ee931b"),
        b256!("a2046a656bac46bd355ef7fbce24cfe4f2d8260d0f07442087c13cdf38bd0012"),
        b256!("2cbd13ad4831847e63a9c3147a9b419f0be4a21c3f3906bf939eb184409c3971"),
        b256!("10d29f9d1c6c5bfa9f72444e34abf1e137013dcef64f2856eb4032f78929f27b"),
        b256!("4c05bfd49aa335d218f7770336d7ad4e9587bd39c021ec08c7906c1e07d1ee16"),
        b256!("748c070c6f9ad8a1a51bb25ac43189074a8111d9dccb9531d0677fd840bc61b6"),
        b256!("51b9c24687bd8b8837ec84a43cc4216d90519049653b327c6fdef0b81d2acb87"),
        b256!("f18f32b385c227ee280c1ed4254a7c41b0c3eeea904a39f4dbef9b2c54685199"),
        b256!("9c96904efeed178aec44076145c9357f3c9027e9bb0f9b0199ec08e3b9d6234f"),
        b256!("79b2a3d1e75d75174c5ddcef873c5552d64f6eca35fef18eba3f7062849f66fa"),
        b256!("ec432bbf528f36ebf306136d90aecff5f9bb8ccafee53a269e2c3b6b5126100a"),
        b256!("5c2d5a08d7f1e450605c3a27331937c57c5903cbbe2c5b706833745866402ebc"),
        b256!("eb0f760b3216dfbde70fac9472d305eb4fff0e992a36038d86227dc1539e15dd"),
        b256!("08228ae9f0789f1e7fdca94678b3c1d28c968870b93e9022d6539e9c8fe523db"),
        b256!("1a16d9eea247d840b856c0e113c711627dddf58f0aa9bf6b3d0b566c1d6d80b3"),
        b256!("393ba669040b260b21e4257e19642e318cab1e54fe048079ae0d5c7ed9b88983"),
        b256!("f5444ec4214ddb9c8c7397baf0261f205f6bc8a46e2ce9876df3c4230bc9201b"),
        b256!("0d6802fb1c95b1939050f8d6111982ded7dbf50636769218a1a035032b5bed01"),
        b256!("30cd5369b58313f95e4a7b584df13c98e48ff8d878e82c998f3ef606544995b9"),
        b256!("c84908a9b0a8b931f7c48413a35d8b9dc6c05cadaca7f0f32e4a4c655b11f1c4"),
        b256!("6299da611d49f467d90236ab6dd84bcc62caf3471a5584f40e98018b8eef3dc4"),
        b256!("3cb0d3f41eb61bf0bc470d1d7e51856b796b537260fd218ba6984f92e9e1c772"),
        b256!("7bbd7c601103988c33ac8cdc0debd339d5336af2c35d18eb7fc0c93156314d67"),
        b256!("cdfa388d66223eb7ab294cd59f5f4a2e45ebb3e8c68175bc3a50e891348ba86e"),
        b256!("2438c16e7dbca50848277eee0138470e7b0899a0410edbbf2b7653ef13cfd03a"),
        b256!("e9b7d4ab397dffcc83962a27d03be43afcbb15cdec6cc1a6d29502ea9ef6526b"),
        b256!("bbc7b01ff68dee7551c53a82d571a0e20637300b837a49d859a629cd8c575118"),
        b256!("1bccc73a1e6718dfe6eeb9ed071598985e3c6ab04cbe71f0a28cc4a83a73a580"),
        b256!("04e002a0e7a594154a9e10d3da7ff724433d15a84b612f1d7ee709487b91c63e"),
        b256!("1e71fc90358288b8bdbd737c9ecb1f956d7e2479c45bf4d928182e61f1bb74a3"),
        b256!("6fd8a0a9b5b99f973f897bbc18ad9cf2ce466118c58c99c6121d0261248e62b0"),
        b256!("78d4424d2cb8284f8f2b42193daafa3bd478204f666432b47c9faa97301fbf4f"),
        b256!("21600770d8a4951c002727e2a7b9dc092867d5ce9511e72a3829a29ab55d69c3"),
        b256!("644f8b82e00feb185938b7b85e003840d0c1fcbb7570e7355c680c175bf95a8a"),
        b256!("5f16cc61c66e250a32bb72280f595afced61dc410c088cab08ab96bfd43940a2"),
        b256!("9d703524912e6eddabaaa4ba0e06da158153f997d1d00745a67113fda37cc20f"),
        b256!("324643dd6a5bddec16044f7e22e778170bf33b1439ec891e728fd06a4cc14805"),
        b256!("9c8da2ce2dae645278edccaf0eab4830a69d7fb5b483b123602d47c83ae3239c"),
        b256!("3012d070480835ed85e19eac1a74b712cf95a5b2dd73f33a0b500d54a12a610b"),
        b256!("9e0934ce8d1cb3b812d6482d0497b578a6aeee9f791e09dd2eb5af52dffb56a4"),
        b256!("dee43fac7f58a3c38d29223f096e0d87cc9e685c703cddf4205ffc018ea53887"),
        b256!("967f886dbd60ddaa6e8ba6d782a492de4a5a84435428fef69a6d3fd9b154e869"),
        b256!("3afadbdb865182315b0f412332e9d576feaf1a9bb7c517a3841089c117e7f3ff"),
        b256!("bc96ba391593a58efcabb270629427751779e78c72007a1b58d64b8eee2bdc05"),
        b256!("ee58d292ba82bbaedcf81c4a05f93648479e56e89f6c2b8279a8012ecba94ac1"),
        b256!("ad485ea4b65dce69ddc2e015dcdd07e03e3b0549f0ba20399dceca8b9c01bb6c"),
        b256!("f23794b840a751cd17db66d2e02ade22f88276382ff9df66fdb63add9ab86ba3"),
        b256!("507e510ce761c05368d4c3ddb0afa3e8c333810852ab54de33ab86e7082054b7"),
        b256!("095d2ec1865aa0173cd0019e31b845309daf470f83da63900dc0f26a8cf76889"),
        b256!("9642dd2920df0efd8af7f3d004025c28768850e7ebdb0fe1558ad0307914d2eb"),
        b256!("9537b55abbe5f29c4c6c4fd703ea6231f41c3041775a656510bdc935f1c22a6e"),
        b256!("a0ae1de3a501acf31a17b0cd7f0d84d358a2fa0cf6eb5de5e2051dc7048db3f6"),
        b256!("006af45051b4f8437a4f772998e6fa552cb2907386cdf7a94ffb1e20981c2355"),
        b256!("060c9bd0155f9b8e6475250cadad3c9a384cf678cc4b6e4dc39b91bd92a8d3d0"),
        b256!("e48e53489147d8e7fc6750ad89e4d5519a13f7e247673c02ed8698abae14fcb7"),
        b256!("8075125ff65f581fd94021bf4771fee9dcbf5157fad2743b8aab6308418f66e4"),
        b256!("8257927d1d678b7e50803569e78e0158a738d23b9cb52bf842e502746ee58219"),
        b256!("e9a8ab3447d514f635bc3d6a7b9eae0f8565008ba04df0ff653de4ca9fb265db"),
        b256!("bc54a9d042d5c842e75671c780c7f1b56177b4e1e851fcdae7bff4a7f78d2a16"),
        b256!("a66f2420ce6fe242bf9cc11908552f787273db9cc271a5695dcbfd53dd49972c"),
        b256!("9fbae6fd935354d303ac15ba292f03ffacf72b80e16beb83b329dcd2ea73402e"),
        b256!("7473bb42cbf3ca531a0a6319a48861be82cdd50370c4da538546b558e3a76c8b"),
        b256!("85a6e5d5c695a8bc9381513eaffbcd1a823551844c5d95120476b88cf66280c2"),
        b256!("e83c622dbd855a4df016809443d7c0839ac1308b3d7b6213cb201b37cd962ad4"),
        b256!("67ba685a31df63ae43b5661b04e9e8f4801008b05920e7c75c622ea6f6d78356"),
        b256!("d02f8f0f0274e625eb15e32847abb734a5c5104cc870b9506eeecbe64ac39bd3"),
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
