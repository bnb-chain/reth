//! Bsc block executor.

use crate::{verify::verify_receipts, BscBlockExecutionError, BscEvmConfig};
use bitset::BitSet;
use blst::{
    min_pk::{PublicKey, Signature},
    BLST_ERROR,
};
use lazy_static::lazy_static;
use lru::LruCache;
use parking_lot::RwLock;
use reth_bsc_consensus::{
    get_top_validators_by_voting_power, is_breathe_block, is_system_transaction, Parlia,
    ParliaConfig, COLLECT_ADDITIONAL_VOTES_REWARD_RATIO, DIFF_INTURN, DIFF_NOTURN,
    MAX_SYSTEM_REWARD, NATURALLY_JUSTIFIED_DIST, SYSTEM_REWARD_CONTRACT, SYSTEM_REWARD_PERCENT,
    SYSTEM_TXS_GAS,
};
use reth_db::{
    models::parlia::{Snapshot, VoteAddress, CHECKPOINT_INTERVAL, MAX_ATTESTATION_EXTRA_LENGTH},
    table::Compress,
};
use reth_evm::{
    execute::{
        BatchBlockExecutionOutput, BatchExecutor, BlockExecutionInput, BlockExecutionOutput,
        BlockExecutorProvider, Executor,
    },
    ConfigureEvm,
};
use reth_interfaces::{
    executor::{BlockExecutionError, BlockValidationError},
    provider::ProviderError,
};
use reth_primitives::{
    address, b256, constants::SYSTEM_ADDRESS, hex, system_contracts::get_upgrade_system_contracts,
    Address, BlockNumber, BlockWithSenders, Bytes, ChainSpec, GotExpected, Header, PruneModes,
    Receipt, Receipts, Transaction, TransactionSigned, B256, U256,
};
use reth_provider::ParliaProvider;
use reth_revm::{
    batch::{BlockBatchRecord, BlockExecutorStats},
    db::{states::bundle_state::BundleRetention, AccountStatus},
    Evm, State,
};
use revm_primitives::{
    db::{Database, DatabaseCommit},
    BlockEnv, CfgEnvWithHandlerCfg, EnvWithHandlerCfg, ResultAndState, TransactTo,
};
use std::{collections::HashMap, num::NonZeroUsize, str::FromStr, sync::Arc, time::Instant};
use tracing::{debug, trace};

const SNAP_CACHE_NUM: usize = 2048;
const BLST_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

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

impl<P, EvmConfig> BscExecutorProvider<P, EvmConfig> {
    /// Creates a new executor provider.
    pub fn new(
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

/// Helper container type for EVM with chain spec.
#[derive(Debug, Clone)]
struct BscEvmExecutor<EvmConfig> {
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
        // reserve gas for system transactions
        let gas_limit = block.header.gas_limit - SYSTEM_TXS_GAS;

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
            if self.chain_spec.is_cancun_active_at_timestamp(block.timestamp) {
                if system_txs.len() > 0 {
                    return Err(BscBlockExecutionError::UnexpectedNormalTx.into());
                }
            }

            // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
            // must be no greater than the block’s gasLimit.
            let block_available_gas = gas_limit - cumulative_gas_used;
            if transaction.gas_limit() > block_available_gas {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: transaction.gas_limit(),
                    block_available_gas,
                }
                .into());
            }

            EvmConfig::fill_tx_env(evm.tx_mut(), transaction, *sender);

            // Execute transaction.
            let ResultAndState { result, mut state } = evm.transact().map_err(move |err| {
                // Ensure hash is calculated for error log, if not already done
                BlockValidationError::EVM {
                    hash: transaction.recalculate_hash(),
                    error: err.into(),
                }
            })?;

            trace!(
                target: "evm",
                ?transaction,
                "Executed transaction"
            );

            self.patch_geth_hot_fix_mainnet(&block.header, transaction, &mut state);
            self.patch_geth_hot_fix_chapel(&block.header, transaction, &mut state);

            evm.db_mut().commit(state);

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

    fn patch_geth_hot_fix_mainnet(
        &self,
        header: &Header,
        transaction: &TransactionSigned,
        state: &mut revm_primitives::State,
    ) {
        let patches = vec![
            // patch 1: BlockNum 33851236, txIndex 89
            (
                b256!("022296e50021d7225b75f3873e7bc5a2bf6376a08079b4368f9dee81946d623b"),
                b256!("7eba4edc7c1806d6ee1691d43513838931de5c94f9da56ec865721b402f775b0"),
                address!("00000000001f8b68515EfB546542397d3293CCfd"),
                HashMap::from([
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x00000000000000000000000052db206170b430da8223651d28830e56ba3cdc04",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000002",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x000000000000000000000000bb45f138499734bf5c0948d490c65903676ea1de",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x65c95177950b486c2071bf2304da1427b9136564150fb97266ffb318b03a71cc",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x245e58a02bec784ccbdb9e022a84af83227a4125a22a5e68fcc596c7e436434e",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x1c4534c86090a60a9120f34c7b15254913c00bda3d4b276d6edb65c9f48a913f",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000004",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000019",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd1b4",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd1b5",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd1b6",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000005",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x00000000000000000000000000000000000000000000000000000000000fc248",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000006",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x00000000000000000000000000000000000000000000000000000000000fc132",
                        )
                        .unwrap(),
                    ),
                ]),
            ),
            // patch 2: BlockNum 33851236, txIndex 90
            (
                b256!("022296e50021d7225b75f3873e7bc5a2bf6376a08079b4368f9dee81946d623b"),
                b256!("5217324f0711af744fe8e12d73f13fdb11805c8e29c0c095ac747b7e4563e935"),
                address!("00000000001f8b68515EfB546542397d3293CCfd"),
                HashMap::from([
                    (
                        U256::from_str(
                            "0xbcfc62ca570bdb58cf9828ac51ae8d7e063a1cc0fa1aee57691220a7cd78b1c8",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x30dce49ce1a4014301bf21aad0ee16893e4dcc4a4e4be8aa10e442dd13259837",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xc0582628d787ee16fe03c8e5b5f5644d3b81989686f8312280b7a1f733145525",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xfca5cf22ff2e8d58aece8e4370cce33cd0144d48d00f40a5841df4a42527694b",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xb189302b37865d2ae522a492ff1f61a5addc1db44acbdcc4b6814c312c815f46",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xfe1f1986775fc2ac905aeaecc7b1aa8b0d6722b852c90e26edacd2dac7382489",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x36052a8ddb27fecd20e2e09da15494a0f2186bf8db36deebbbe701993f8c4aae",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x4959a566d8396b889ff4bc20e18d2497602e01e5c6013af5af7a7c4657ece3e2",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xe0b5aeb100569add952966f803cb67aca86dc6ec8b638f5a49f9e0760efa9a7a",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x632467ad388b91583f956f76488afc42846e283c962cbb215d288033ffc4fb71",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x9ad4e69f52519f7b7b8ee5ae3326d57061b429428ea0c056dd32e7a7102e79a7",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x35e130c7071699eae5288b12374ef157a15e4294e2b3a352160b7c1cd4641d82",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xa0d8279f845f63979dc292228adfa0bda117de27e44d90ac2adcd44465b225e7",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x9a100b70ffda9ed9769becdadca2b2936b217e3da4c9b9817bad30d85eab25ff",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x28d67156746295d901005e2d95ce589e7093decb638f8c132d9971fd0a37e176",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x297c4e115b5df76bcd5a1654b8032661680a1803e30a0774cb42bb01891e6d97",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x5f71b88f1032d27d8866948fc9c49525f3e584bdd52a66de6060a7b1f767326f",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xe6d8ddf6a0bbeb4840f48f0c4ffda9affa4675354bdb7d721235297f5a094f54",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x30ba10aef6238bf19667aaa988b18b72adb4724c016e19eb64bbb52808d1a842",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x9c6806a4d6a99e4869b9a4aaf80b0a3bf5f5240a1d6032ed82edf0e86f2a2467",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xe8480d613bbf3b979aee2de4487496167735bb73df024d988e1795b3c7fa559a",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xebfaec01f898f7f0e2abdb4b0aee3dfbf5ec2b287b1e92f9b62940f85d5f5bac",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                ]),
            ),
        ];

        for (block_hash, tx_hash, address, storage) in patches {
            if header.hash_slow() == block_hash && transaction.recalculate_hash() == tx_hash {
                let account = state.get(&address).unwrap();
                let mut new_account = account.clone();
                for (k, v) in storage {
                    for (&index, slot) in &account.storage {
                        if index == k {
                            let mut slot = slot.clone();
                            slot.present_value = v;
                            new_account.storage.insert(index, slot);
                        }
                    }
                }
                HashMap::insert(state, address, new_account);
            }
        }
    }

    fn patch_geth_hot_fix_chapel(
        &self,
        header: &Header,
        transaction: &TransactionSigned,
        state: &mut revm_primitives::State,
    ) {
        let patches = vec![
            // patch 1: BlockNum 35547779, txIndex 196
            (
                b256!("1237cb09a7d08c187a78e777853b70be28a41bb188c5341987408623c1a4f4aa"),
                b256!("7ce9a3cf77108fcc85c1e84e88e363e3335eca515dfcf2feb2011729878b13a7"),
                address!("89791428868131eb109e42340ad01eb8987526b2"),
                HashMap::from([(
                    U256::from_str(
                        "0xf1e9242398de526b8dd9c25d38e65fbb01926b8940377762d7884b8b0dcdc3b0",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x0000000000000000000000000000000000000000000000f6a7831804efd2cd0a",
                    )
                    .unwrap(),
                )]),
            ),
            // patch 2: BlockNum 35548081, txIndex 486
            (
                b256!("cdd38b3681c8f3f1da5569a893231466ab35f47d58ba85dbd7d9217f304983bf"),
                b256!("e3895eb95605d6b43ceec7876e6ff5d1c903e572bf83a08675cb684c047a695c"),
                address!("89791428868131eb109e42340ad01eb8987526b2"),
                HashMap::from([(
                    U256::from_str(
                        "0xf1e9242398de526b8dd9c25d38e65fbb01926b8940377762d7884b8b0dcdc3b0",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x0000000000000000000000000000000000000000000000114be8ecea72b64003",
                    )
                    .unwrap(),
                )]),
            ),
        ];

        for (block_hash, tx_hash, address, storage) in patches {
            if header.hash_slow() == block_hash && transaction.recalculate_hash() == tx_hash {
                let account = state.get(&address).unwrap();
                let mut new_account = account.clone();
                for (k, v) in storage {
                    for (&index, slot) in &account.storage {
                        if index == k {
                            let mut slot = slot.clone();
                            slot.present_value = v;
                            new_account.storage.insert(index, slot);
                        }
                    }
                }
                HashMap::insert(state, address, new_account);
            }
        }
    }
}

/// A basic Ethereum block executor.
///
/// Expected usage:
/// - Create a new instance of the executor.
/// - Execute the block.
#[derive(Debug)]
pub struct BscBlockExecutor<EvmConfig, DB, P> {
    /// Chain specific evm config that's used to execute a block.
    executor: BscEvmExecutor<EvmConfig>,
    /// The state to use for execution
    state: State<DB>,
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
    fn chain_spec(&self) -> &ChainSpec {
        &self.executor.chain_spec
    }

    #[allow(unused)]
    #[inline]
    fn parlia(&self) -> &Parlia {
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
    /// Returns an error if execution fails or receipt verification fails.
    fn execute_and_verify(
        &mut self,
        block: &BlockWithSenders,
        total_difficulty: U256,
    ) -> Result<(Vec<Receipt>, u64, Option<Snapshot>), BlockExecutionError> {
        // 1. get parent header and snapshot
        let ref parent = self.get_header_by_hash(block.number - 1, block.parent_hash)?;
        let ref snap = self.snapshot(parent, None)?;

        // 2. prepare state on new block
        self.on_new_block(&block.header, parent, snap)?;

        // 3. configure the evm and execute normal transactions
        let env = self.evm_env_for_block(&block.header, total_difficulty);

        if !self.parlia.is_feynman(block.timestamp) {
            let parent = self.get_header_by_hash(block.number - 1, block.parent_hash)?;
            // apply system contract upgrade
            self.upgrade_system_contracts(block.number, block.timestamp, parent.timestamp)?;
        }

        let (mut system_txs, mut receipts, mut gas_used) = {
            let evm = self.executor.evm_config.evm_with_env(&mut self.state, env.clone());
            self.executor.execute_pre_and_transactions(block, evm)
        }?;

        // 4. apply post execution changes
        self.post_execution(
            block,
            parent,
            snap,
            &mut system_txs,
            &mut receipts,
            &mut gas_used,
            env.clone(),
        )?;

        // Check if gas used matches the value set in header.
        if block.gas_used != gas_used {
            let receipts = Receipts::from_block_receipt(receipts);
            return Err(BlockValidationError::BlockGasUsed {
                gas: GotExpected { got: gas_used, expected: block.gas_used },
                gas_spent_by_tx: receipts.gas_spent_by_tx()?,
            }
            .into());
        }

        // Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is required for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if self.chain_spec().is_byzantium_active_at_block(block.header.number) {
            if let Err(error) = verify_receipts(
                block.header.receipts_root,
                block.header.logs_bloom,
                receipts.iter(),
            ) {
                debug!(target: "evm", %error, ?receipts, "receipts verification failed");
                return Err(error);
            };
        }

        if snap.block_number % CHECKPOINT_INTERVAL == 0 {
            return Ok((receipts, gas_used, Some(snap.clone())));
        }

        Ok((receipts, gas_used, None))
    }

    /// Apply settings and verify headers before a new block is executed.
    pub(crate) fn on_new_block(
        &mut self,
        header: &Header,
        parent: &Header,
        snap: &Snapshot,
    ) -> Result<(), BlockExecutionError> {
        // Set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag = self.chain_spec().is_spurious_dragon_active_at_block(header.number);
        self.state.set_state_clear_flag(state_clear_flag);

        self.verify_cascading_fields(header, parent, snap)
    }

    /// Apply post execution state changes, including system txs and other state change.
    pub fn post_execution(
        &mut self,
        block: &BlockWithSenders,
        parent: &Header,
        snap: &Snapshot,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let number = block.number;
        let validator = block.beneficiary;
        let header = &block.header;

        //TODO: isMajorityFork ?

        self.verify_validators(header, env.clone())?;

        if number == 1 {
            self.init_genesis_contracts(
                validator,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        if self.parlia.is_feynman(block.timestamp) {
            let parent = self.get_header_by_hash(block.number - 1, block.parent_hash)?;
            // apply system contract upgrade
            self.upgrade_system_contracts(block.number, block.timestamp, parent.timestamp)?;
        }

        if self.parlia.is_on_feynman(block.timestamp, parent.timestamp) {
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
            let signed_recently: bool;
            if self.parlia.is_plato(number) {
                signed_recently = snap.sign_recently(spoiled_val);
            } else {
                signed_recently = snap
                    .recent_proposers
                    .iter()
                    .find(|(_, v)| **v == spoiled_val)
                    .map(|_| true)
                    .unwrap_or(false);
            }

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

        if self.parlia.is_plato(number) {
            self.distribute_finality_reward(
                header,
                system_txs,
                receipts,
                cumulative_gas_used,
                env.clone(),
            )?;
        }

        // update validator set after Feynman upgrade
        if self.parlia.is_feynman(header.timestamp) &&
            is_breathe_block(parent.timestamp, header.timestamp)
        {
            if !self.parlia.is_on_feynman(header.timestamp, parent.timestamp) {
                self.update_validator_set_v2(
                    validator,
                    system_txs,
                    receipts,
                    cumulative_gas_used,
                    env.clone(),
                )?;
            }
        }

        if !system_txs.is_empty() {
            return Err(BscBlockExecutionError::UnexpectedSystemTx.into());
        }

        Ok(())
    }

    fn verify_cascading_fields(
        &self,
        header: &Header,
        parent: &Header,
        snap: &Snapshot,
    ) -> Result<(), BlockExecutionError> {
        self.verify_block_time_for_ramanujan(snap, header, parent)?;
        self.verify_vote_attestation(snap, header, parent)?;
        self.verify_seal(snap, header)?;

        Ok(())
    }

    fn verify_block_time_for_ramanujan(
        &self,
        snapshot: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        if self.parlia.is_ramanujan(header.number) {
            if header.timestamp <
                parent.timestamp +
                    self.parlia.period() +
                    self.parlia.back_off_time(snapshot, header)
            {
                return Err(BscBlockExecutionError::FutureBlock {
                    block_number: header.number,
                    hash: header.hash_slow(),
                }
                .into());
            }
        }

        Ok(())
    }

    fn verify_vote_attestation(
        &self,
        snap: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        let attestation = self.parlia.get_vote_attestation_from_header(header).map_err(|err| {
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
            let ref justified: Header = self.get_justified_header(snap, parent)?;
            if source_block != justified.number || source_hash != justified.hash_slow() {
                return Err(BscBlockExecutionError::InvalidAttestationSource {
                    block_number: GotExpected { got: source_block, expected: justified.number },
                    block_hash: GotExpected { got: source_hash, expected: justified.hash_slow() }
                        .into(),
                }
                .into());
            }

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
                vote_addrs.push(val_info.vote_addr.clone());
            }

            // check if voted validator count satisfied 2/3+1
            let at_least_votes = validators_count * 2 / 3;
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
            let vote_addrs: Vec<&PublicKey> = vote_addrs.iter().collect();

            let sig = Signature::from_bytes(&attestation.agg_signature[..])
                .map_err(|_| BscBlockExecutionError::BLSTInnerError)?;
            let err = sig.fast_aggregate_verify(
                true,
                attestation.data.hash().as_slice(),
                BLST_DST,
                &vote_addrs,
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
        let proposer = self.parlia.recover_proposer(header).map_err(|err| {
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

        for (seen, recent) in snap.recent_proposers.iter() {
            if *recent == proposer {
                // Signer is among recent_proposers, only fail if the current block doesn't shift it
                // out
                let limit =
                    self.parlia.get_recently_proposal_limit(header, snap.validators.len() as u64);
                if block_number < limit || *seen > block_number - limit {
                    return Err(BscBlockExecutionError::SignerOverLimit { proposer }.into());
                }
            }
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

    fn snapshot(
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
                let (next_validators, bls_keys) =
                    self.parlia.parse_validators_from_header(&header).map_err(|err| {
                        BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                    })?;
                snap = Some(Snapshot::new(
                    next_validators,
                    block_number,
                    block_hash,
                    self.parlia.epoch(),
                    bls_keys,
                ));
                break;
            }

            // No snapshot for this header, gather the header and move backward
            skip_headers.push(header.clone());
            if let Some(parent) = parent {
                block_number = parent.number;
                block_hash = parent.hash_slow();
                header = parent.clone();
            } else if let Some(h) = self
                .provider
                .header_by_number(block_number - 1)
                .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })?
            {
                let hash = h.hash_slow();
                if hash != header.parent_hash {
                    return Err(BscBlockExecutionError::ParentUnknown { hash: block_hash }.into());
                }
                block_number = h.number;
                block_hash = hash;
                header = h;
            }
        }

        let mut snap = snap.ok_or_else(|| BscBlockExecutionError::SnapshotNotFound)?;

        // apply skip headers
        skip_headers.reverse();
        for header in skip_headers.iter() {
            let (new_validators, bls_keys) = if header.number > 0 &&
                header.number % self.parlia.epoch() == (snap.validators.len() / 2) as u64
            {
                // change validator set
                let checkpoint_header =
                    self.find_ancient_header(header, (snap.validators.len() / 2) as u64)?;

                self.parlia.parse_validators_from_header(&checkpoint_header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?
            } else {
                (Vec::new(), None)
            };

            let validator = self.parlia.recover_proposer(header).map_err(|err| {
                BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
            })?;
            let attestation =
                self.parlia.get_vote_attestation_from_header(header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?;

            snap = snap
                .apply(validator, header, new_validators, bls_keys, attestation)
                .ok_or_else(|| BscBlockExecutionError::ApplySnapshotFailed)?;
        }

        cache.put(snap.block_hash, snap.clone());
        Ok(snap)
    }

    fn get_justified_header(
        &self,
        snap: &Snapshot,
        header: &Header,
    ) -> Result<Header, BlockExecutionError> {
        // If there has vote justified block, find it or return naturally justified block.
        if snap.vote_data.source_hash != B256::ZERO && snap.vote_data.target_hash != B256::ZERO {
            if snap.block_number - snap.vote_data.target_number > NATURALLY_JUSTIFIED_DIST {
                return self.find_ancient_header(header, NATURALLY_JUSTIFIED_DIST);
            }
            return self
                .get_header_by_hash(snap.vote_data.target_number, snap.vote_data.target_hash);
        }

        // If there is no vote justified block, then return root block or naturally justified block.
        if header.number < NATURALLY_JUSTIFIED_DIST {
            return Ok(self
                .provider
                .header_by_number(0)
                .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })?
                .ok_or_else(|| BscBlockExecutionError::UnknownHeader {
                    block_number: 0,
                    hash: Default::default(),
                })?);
        }

        self.find_ancient_header(header, NATURALLY_JUSTIFIED_DIST)
    }

    fn find_ancient_header(
        &self,
        header: &Header,
        count: u64,
    ) -> Result<Header, BlockExecutionError> {
        let mut result = header.clone();
        for _ in 0..count {
            result = self.get_header_by_hash(result.number - 1, result.parent_hash)?;
        }
        Ok(result)
    }

    fn get_header_by_hash(
        &self,
        block_number: BlockNumber,
        hash: B256,
    ) -> Result<Header, BlockExecutionError> {
        let header = self
            .provider
            .header_by_number(block_number)
            .map_err(|err| BscBlockExecutionError::ProviderInnerError { error: err.into() })?
            .ok_or_else(|| BscBlockExecutionError::UnknownHeader { block_number, hash })?;

        return if header.hash_slow() == hash {
            Ok(header)
        } else {
            Err(BscBlockExecutionError::UnknownHeader { block_number, hash }.into())
        };
    }

    fn verify_validators(
        &mut self,
        header: &Header,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let number = header.number;
        if number % self.parlia().epoch() != 0 {
            return Ok(())
        };

        let (mut validators, mut vote_addrs_map) = self.get_current_validators(number, env.clone());
        validators.sort();

        let validator_num = validators.len();
        let validator_bytes = if !self.parlia.is_luban(number) {
            let mut validator_bytes = Vec::new();
            for v in validators {
                validator_bytes.extend_from_slice(v.as_ref());
            }

            validator_bytes
        } else {
            if self.parlia.is_on_luban(number) {
                vote_addrs_map = Vec::with_capacity(validator_num);
                for _ in 0..validator_num {
                    vote_addrs_map.push(VoteAddress::default());
                }
            }

            let mut validator_bytes = Vec::new();
            for i in 0..validator_num {
                validator_bytes.extend_from_slice(validators[i].as_ref());
                validator_bytes.extend_from_slice(vote_addrs_map[i].as_ref());
            }

            validator_bytes
        };

        if !validator_bytes.as_slice().eq(self
            .parlia
            .get_validator_bytes_from_header(header)
            .unwrap()
            .as_slice())
        {
            return Err(BscBlockExecutionError::InvalidValidators.into());
        }

        Ok(())
    }

    fn get_current_validators(
        &mut self,
        number: BlockNumber,
        env: EnvWithHandlerCfg,
    ) -> (Vec<Address>, Vec<VoteAddress>) {
        if !self.parlia.is_luban(number) {
            let (to, data) = self.parlia.get_current_validators_before_luban(number);
            let output = self.eth_call(to, data, env.clone()).unwrap();

            (self.parlia.unpack_data_into_validator_set_before_luban(output.as_ref()), Vec::new())
        } else {
            let (to, data) = self.parlia.get_current_validators();
            let output = self.eth_call(to, data, env.clone()).unwrap();

            self.parlia.unpack_data_into_validator_set(output.as_ref())
        }
    }

    fn init_genesis_contracts(
        &mut self,
        validator: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let transactions = self.parlia.init_genesis_contracts();
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
        let transactions = self.parlia.init_feynman_contracts();
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
            self.parlia.slash(spoiled_val),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env.clone(),
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
        if system_account.status == AccountStatus::LoadedNotExisting ||
            system_account.status == AccountStatus::DestroyedAgain
        {
            return Ok(());
        }
        let (mut block_reward, transition) = system_account.drain_balance();
        if let Some(s) = self.state.transition_state.as_mut() {
            s.add_transitions(vec![(SYSTEM_ADDRESS, transition)]);
        }
        if block_reward == 0 {
            return Ok(());
        }

        let mut balance_increment = HashMap::new();
        balance_increment.insert(validator, block_reward);
        self.state
            .increment_balances(balance_increment)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        let system_reward_balance =
            self.state.basic(*SYSTEM_REWARD_CONTRACT).unwrap().unwrap_or_default().balance;

        if !self.parlia.is_kepler(header.timestamp) {
            if system_reward_balance < U256::from(MAX_SYSTEM_REWARD) {
                let reward_to_system = block_reward >> SYSTEM_REWARD_PERCENT;
                if reward_to_system > 0 {
                    self.transact_system_tx(
                        self.parlia.distribute_to_system(reward_to_system),
                        validator,
                        system_txs,
                        receipts,
                        cumulative_gas_used,
                        env.clone(),
                    )?;
                }

                block_reward -= reward_to_system;
            }
        }

        self.transact_system_tx(
            self.parlia.distribute_to_validator(validator, block_reward),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env.clone(),
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
        if header.number % self.parlia.epoch() != 0 {
            return Ok(());
        }

        let validator = header.beneficiary;

        let mut accumulated_weights: HashMap<Address, U256> = HashMap::new();
        let start = (header.number - self.parlia.epoch()).max(1);
        for height in (start..header.number).rev() {
            let header = self.get_header_by_hash(height, header.parent_hash)?;
            if let Some(attestation) =
                self.parlia.get_vote_attestation_from_header(&header).map_err(|err| {
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?
            {
                let justified_header = self.get_header_by_hash(
                    attestation.data.target_number,
                    attestation.data.target_hash,
                )?;
                let parent = self.get_header_by_hash(
                    justified_header.number - 1,
                    justified_header.parent_hash,
                )?;
                let snap = self.snapshot(&parent, None)?;
                let validators = &snap.validators;
                let validators_bit_set = BitSet::from_u64(attestation.vote_address_set);
                if validators_bit_set.count() as usize > validators.len() {
                    return Err(BscBlockExecutionError::InvalidAttestationVoteCount(GotExpected {
                        got: validators_bit_set.count(),
                        expected: validators.len() as u64,
                    })
                    .into());
                }

                let mut valid_vote_count = 0;
                for (index, val) in validators.iter().enumerate() {
                    if validators_bit_set.test(index) {
                        *accumulated_weights.entry(*val).or_insert(U256::ZERO) += U256::from(1);
                        valid_vote_count += 1;
                    }
                }
                let quorum = (snap.validators.len() * 2 + 2) / 3; // ceil div
                if valid_vote_count > quorum {
                    *accumulated_weights.entry(header.beneficiary).or_insert(U256::ZERO) +=
                        U256::from(
                            ((valid_vote_count - quorum) * COLLECT_ADDITIONAL_VOTES_REWARD_RATIO) /
                                100,
                        );
                }
            }
        }

        let mut validators: Vec<Address> = accumulated_weights.keys().cloned().collect();
        validators.sort();
        let weights: Vec<U256> =
            validators.iter().map(|val| accumulated_weights[val].clone()).collect();

        self.transact_system_tx(
            self.parlia.distribute_finality_reward(validators, weights),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env.clone(),
        )?;

        Ok(())
    }

    fn update_validator_set_v2(
        &mut self,
        validator: Address,
        system_txs: &mut Vec<TransactionSigned>,
        receipts: &mut Vec<Receipt>,
        cumulative_gas_used: &mut u64,
        env: EnvWithHandlerCfg,
    ) -> Result<(), BlockExecutionError> {
        let (to, data) = self.parlia.get_max_elected_validators();
        let output = self.eth_call(to, data, env.clone())?;
        let max_elected_validators =
            self.parlia.unpack_data_into_max_elected_validators(output.as_ref());

        let (to, data) = self.parlia.get_validator_election_info();
        let output = self.eth_call(to, data, env.clone())?;
        let (consensus_addrs, voting_powers, vote_addrs, total_length) =
            self.parlia.unpack_data_into_validator_election_info(output.as_ref());

        let (e_validators, e_voting_powers, e_vote_addrs) = get_top_validators_by_voting_power(
            consensus_addrs,
            voting_powers,
            vote_addrs,
            total_length,
            max_elected_validators,
        )
        .ok_or_else(|| BscBlockExecutionError::GetTopValidatorsFailed)?;

        self.transact_system_tx(
            self.parlia.update_validator_set_v2(e_validators, e_voting_powers, e_vote_addrs),
            validator,
            system_txs,
            receipts,
            cumulative_gas_used,
            env.clone(),
        )?;

        Ok(())
    }

    /// Upgrade system contracts based on the hardfork rules.
    fn upgrade_system_contracts(
        &mut self,
        block_number: BlockNumber,
        block_time: u64,
        parent_block_time: u64,
    ) -> Result<bool, BscBlockExecutionError> {
        if let Ok(contracts) = get_upgrade_system_contracts(
            &self.parlia.chain_spec(),
            block_number,
            block_time,
            parent_block_time,
        ) {
            for (k, v) in contracts {
                let account = self.state.load_cache_account(k).map_err(|err| {
                    BscBlockExecutionError::ProviderInnerError { error: err.into() }
                })?;

                let mut new_info = account.account_info().unwrap_or_default();
                new_info.code_hash = v.clone().unwrap().hash_slow();
                new_info.code = v;
                let transition = account.change(new_info, HashMap::new());

                if let Some(s) = self.state.transition_state.as_mut() {
                    s.add_transitions(vec![(k, transition)]);
                }
            }

            Ok(true)
        } else {
            Err(BscBlockExecutionError::SystemContractUpgradeError)
        }
    }

    fn eth_call(
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

    fn transact_system_tx(
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
        if hash != system_txs[0].signature_hash() {
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
        let (receipts, gas_used, snapshot) = self.execute_and_verify(block, total_difficulty)?;

        // NOTE: we need to merge keep the reverts for the bundle retention
        self.state.merge_transitions(BundleRetention::Reverts);

        Ok(BlockExecutionOutput { state: self.state.take_bundle(), receipts, gas_used, snapshot })
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
    /// Returns the receipts of the executed blocks.
    pub fn receipts(&self) -> &Receipts {
        self.batch_record.receipts()
    }

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
    type Output = BatchBlockExecutionOutput;
    type Error = BlockExecutionError;

    fn execute_one(&mut self, input: Self::Input<'_>) -> Result<(), Self::Error> {
        let BlockExecutionInput { block, total_difficulty } = input;
        let execute_start = Instant::now();
        let (receipts, _gas_used, snapshot) =
            self.executor.execute_and_verify(block, total_difficulty)?;
        self.stats.execution_duration += execute_start.elapsed();

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

        BatchBlockExecutionOutput::new(
            self.executor.state.take_bundle(),
            self.batch_record.take_receipts(),
            self.batch_record.first_block().unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::BLST_DST;
    use blst::min_pk::{PublicKey, Signature};
    use reth_db::models::parlia::{VoteAddress, VoteData, VoteSignature};
    use reth_primitives::{b256, hex};

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
