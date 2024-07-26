//! Ethereum EVM and executor builder with bytecode compiler support.

use crate::primitives::{
    revm_primitives,
    revm_primitives::{AnalysisKind, CfgEnvWithHandlerCfg, Env, OptimismFields, TxEnv},
    Address, Bytes, EthereumHardfork, Head, Header, OptimismHardfork, TxKind, U256,
};
use reth_chainspec::ChainSpec;
pub use reth_evm_compiler::*;
use reth_node_api::{ConfigureEvm, ConfigureEvmEnv, FullNodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};
use reth_node_optimism::OpExecutorProvider;
use reth_primitives::{transaction::FillTxEnv, TransactionSigned, B256};
use reth_revm::{
    handler::register::EvmHandler,
    interpreter::{InterpreterAction, SharedMemory},
    primitives::SpecId,
    Context, Database, Evm, Frame,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
};

/// About 10MiB of cached `bytecode_hash -> function_pointer` (40 bytes).
const MAX_CACHE_SIZE: usize = 250_000;

/// Ethereum EVM and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct CompilerExecutorBuilder;

impl<Node: FullNodeTypes> ExecutorBuilder<Node> for CompilerExecutorBuilder {
    type EVM = CompilerEvmConfig;
    type Executor = OpExecutorProvider<Self::EVM>;

    async fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<(Self::EVM, Self::Executor)> {
        let mk_return =
            |config: Self::EVM| (config.clone(), OpExecutorProvider::new(ctx.chain_spec(), config));

        let compiler_config = &ctx.config().experimental.compiler;
        let compiler_dir = ctx.config().datadir().compiler();
        if !compiler_config.compiler {
            tracing::debug!("EVM bytecode compiler is disabled");
            return Ok(mk_return(CompilerEvmConfig::disabled()));
        }
        tracing::info!("EVM bytecode compiler initialized");

        let out_dir =
            compiler_config.out_dir.clone().unwrap_or_else(|| compiler_dir.join("artifacts"));
        let mut compiler = EvmParCompiler::new(out_dir.clone())?;

        let contracts_path = compiler_config
            .contracts_file
            .clone()
            .unwrap_or_else(|| compiler_dir.join("contracts.toml"));
        let contracts_config = ContractsConfig::load(&contracts_path)?;

        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let handle = ctx.task_executor().spawn_blocking(async move {
            if let Err(err) = compiler.run_to_end(&contracts_config) {
                tracing::error!(%err, "failed to run compiler");
            }
            done2.store(true, Ordering::Relaxed);
        });
        if compiler_config.block_on_compiler {
            tracing::info!("Blocking on EVM bytecode compiler");
            handle.await?;
            tracing::info!("Done blocking on EVM bytecode compiler");
        }
        Ok(mk_return(CompilerEvmConfig::new(done, out_dir)))
    }
}

/// Ethereum EVM configuration with bytecode compiler support.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct CompilerEvmConfig(Option<Arc<Mutex<CompilerEvmConfigInner>>>);

struct CompilerEvmConfigInner {
    compiler_is_done: Arc<AtomicBool>,
    out_dir: PathBuf,
    evm_version: Option<SpecId>,
    dll: Option<EvmCompilerDll>,
}

impl CompilerEvmConfig {
    /// Return new compiler EVM configuration.
    pub fn new(compiler_is_done: Arc<AtomicBool>, out_dir: PathBuf) -> Self {
        Self(Some(Arc::new(Mutex::new(CompilerEvmConfigInner {
            compiler_is_done,
            out_dir,
            evm_version: None,
            dll: None,
        }))))
    }

    /// Return configuration with compiler disabled.
    pub fn disabled() -> Self {
        Self(None)
    }

    fn make_context(&self) -> CompilerEvmContext<'_> {
        CompilerEvmContext::new(
            self.0.as_ref().map(|c| c.lock().unwrap_or_else(PoisonError::into_inner)),
        )
    }
}

impl CompilerEvmConfigInner {
    /// Clears the cache if it's too big.
    fn gc(&mut self) {
        if let Some(dll) = &mut self.dll {
            if dll.cache.len() > MAX_CACHE_SIZE {
                dll.cache = Default::default();
            }
        }
    }

    fn get_or_load_library(&mut self, spec_id: SpecId) -> Option<&mut EvmCompilerDll> {
        self.maybe_load_library(spec_id);
        self.dll.as_mut()
    }

    fn maybe_load_library(&mut self, spec_id: SpecId) {
        if !self.compiler_is_done.load(Ordering::Relaxed) {
            return;
        }

        let evm_version = spec_id_to_evm_version(spec_id);
        if let Some(v) = self.evm_version {
            if v == evm_version {
                return;
            }
        }

        self.load_library(evm_version);
    }

    #[cold]
    #[inline(never)]
    fn load_library(&mut self, evm_version: SpecId) {
        if let Some(dll) = self.dll.take() {
            if let Err(err) = dll.close() {
                tracing::error!(%err, ?self.evm_version, "failed to close shared library");
            }
        }
        self.evm_version = Some(evm_version);
        self.dll = match unsafe { EvmCompilerDll::open_in(&self.out_dir, evm_version) } {
            Ok(library) => Some(library),
            // TODO: This can happen if the library is not found, but we should handle it better.
            Err(err) => {
                tracing::warn!(%err, ?evm_version, "failed to load shared library");
                None
            }
        };
    }
}

impl ConfigureEvmEnv for CompilerEvmConfig {
    fn fill_tx_env(&self, tx_env: &mut TxEnv, transaction: &TransactionSigned, sender: Address) {
        transaction.fill_tx_env(tx_env, sender);
    }

    fn fill_tx_env_system_contract_call(
        &self,
        env: &mut Env,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) {
        env.tx = TxEnv {
            caller,
            transact_to: TxKind::Call(contract),
            // Explicitly set nonce to None so revm does not do any nonce checks
            nonce: None,
            gas_limit: 30_000_000,
            value: U256::ZERO,
            data,
            // Setting the gas price to zero enforces that no value is transferred as part of the
            // call, and that the call will not count against the block's gas limit
            gas_price: U256::ZERO,
            // The chain ID check is not relevant here and is disabled if set to None
            chain_id: None,
            // Setting the gas priority fee to None ensures the effective gas price is derived from
            // the `gas_price` field, which we need to be zero
            gas_priority_fee: None,
            access_list: Vec::new(),
            // blob fields can be None for this tx
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: None,
            authorization_list: None,
            optimism: OptimismFields {
                source_hash: None,
                mint: None,
                is_system_transaction: Some(false),
                // The L1 fee is not charged for the EIP-4788 transaction, submit zero bytes for the
                // enveloped tx size.
                enveloped_tx: Some(Bytes::default()),
            },
        };

        // ensure the block gas limit is >= the tx
        env.block.gas_limit = U256::from(env.tx.gas_limit);

        // disable the base fee check for this call by setting the base fee to zero
        env.block.basefee = U256::ZERO;
    }

    fn fill_cfg_env(
        &self,
        cfg_env: &mut CfgEnvWithHandlerCfg,
        chain_spec: &ChainSpec,
        header: &Header,
        total_difficulty: U256,
    ) {
        let spec_id = revm_spec(
            chain_spec,
            &Head {
                number: header.number,
                timestamp: header.timestamp,
                difficulty: header.difficulty,
                total_difficulty,
                hash: Default::default(),
            },
        );

        cfg_env.chain_id = chain_spec.chain().id();
        cfg_env.perf_analyse_created_bytecodes = AnalysisKind::Analyse;

        cfg_env.handler_cfg.spec_id = spec_id;
        cfg_env.handler_cfg.is_optimism = chain_spec.is_optimism();
    }
}

impl ConfigureEvm for CompilerEvmConfig {
    type DefaultExternalContext<'a> = CompilerEvmContext<'a>;

    fn evm<'a, DB: Database + 'a>(
        &'a self,
        db: DB,
    ) -> Evm<'a, Self::DefaultExternalContext<'a>, DB> {
        let builder =
            Evm::builder().with_db(db).with_external_context(self.make_context()).optimism();
        if self.0.is_some() {
            builder.append_handler_register(register_compiler_handler).build()
        } else {
            builder.build()
        }
    }
}

/// [`CompilerEvmConfig`] EVM context.
#[allow(missing_debug_implementations)]
pub struct CompilerEvmContext<'a> {
    config: Option<MutexGuard<'a, CompilerEvmConfigInner>>,
}

impl<'a> CompilerEvmContext<'a> {
    fn new(mut config: Option<MutexGuard<'a, CompilerEvmConfigInner>>) -> Self {
        if let Some(config) = &mut config {
            config.gc();
        }
        Self { config }
    }

    #[inline]
    fn get_or_load_library(&mut self, spec_id: SpecId) -> Option<&mut EvmCompilerDll> {
        match self.config.as_mut() {
            Some(config) => config.get_or_load_library(spec_id),
            None => unreachable_misconfigured(),
        }
    }
}

fn register_compiler_handler<DB: Database>(
    handler: &mut EvmHandler<'_, CompilerEvmContext<'_>, DB>,
) {
    let previous = handler.execution.execute_frame.clone();
    handler.execution.execute_frame = Arc::new(move |frame, memory, table, context| {
        if let Some(action) = execute_frame(frame, memory, context) {
            Ok(action)
        } else {
            previous(frame, memory, table, context)
        }
    });
}

fn execute_frame<DB: Database>(
    frame: &mut Frame,
    memory: &mut SharedMemory,
    context: &mut Context<CompilerEvmContext<'_>, DB>,
) -> Option<InterpreterAction> {
    let library = context.external.get_or_load_library(context.evm.spec_id())?;
    let interpreter = frame.interpreter_mut();
    let hash = match interpreter.contract.hash {
        Some(hash) => hash,
        None => unreachable_no_hash(),
    };
    let f = match library.get_function(hash) {
        Ok(Some(f)) => f,
        Ok(None) => return None,
        // Shouldn't happen.
        Err(err) => {
            unlikely_log_get_function_error(err, &hash);
            return None;
        }
    };

    interpreter.shared_memory =
        std::mem::replace(memory, reth_revm::interpreter::EMPTY_SHARED_MEMORY);
    let result = unsafe { f.call_with_interpreter(interpreter, context) };
    *memory = interpreter.take_memory();
    Some(result)
}

#[cold]
#[inline(never)]
const fn unreachable_no_hash() -> ! {
    panic!("unreachable: bytecode hash is not set in the interpreter")
}

#[cold]
#[inline(never)]
const fn unreachable_misconfigured() -> ! {
    panic!("unreachable: AOT EVM is misconfigured")
}

#[cold]
#[inline(never)]
fn unlikely_log_get_function_error(err: impl std::error::Error, hash: &B256) {
    tracing::error!(%err, %hash, "failed getting function from shared library");
}

fn revm_spec(chain_spec: &ChainSpec, block: &Head) -> revm_primitives::SpecId {
    if chain_spec.fork(OptimismHardfork::Fjord).active_at_head(block) {
        revm_primitives::FJORD
    } else if chain_spec.fork(OptimismHardfork::Haber).active_at_head(block) {
        revm_primitives::HABER
    } else if chain_spec.fork(OptimismHardfork::Ecotone).active_at_head(block) {
        revm_primitives::ECOTONE
    } else if chain_spec.fork(OptimismHardfork::Canyon).active_at_head(block) {
        revm_primitives::CANYON
    } else if chain_spec.fork(OptimismHardfork::Fermat).active_at_head(block) {
        revm_primitives::FERMAT
    } else if chain_spec.fork(OptimismHardfork::Regolith).active_at_head(block) {
        revm_primitives::REGOLITH
    } else if chain_spec.fork(OptimismHardfork::Bedrock).active_at_head(block) {
        revm_primitives::BEDROCK
    } else if chain_spec.fork(EthereumHardfork::Prague).active_at_head(block) {
        revm_primitives::PRAGUE
    } else if chain_spec.fork(EthereumHardfork::Cancun).active_at_head(block) {
        revm_primitives::CANCUN
    } else if chain_spec.fork(EthereumHardfork::Shanghai).active_at_head(block) {
        revm_primitives::SHANGHAI
    } else if chain_spec.fork(EthereumHardfork::Paris).active_at_head(block) {
        revm_primitives::MERGE
    } else if chain_spec.fork(EthereumHardfork::London).active_at_head(block) {
        revm_primitives::LONDON
    } else if chain_spec.fork(EthereumHardfork::Berlin).active_at_head(block) {
        revm_primitives::BERLIN
    } else if chain_spec.fork(EthereumHardfork::Istanbul).active_at_head(block) {
        revm_primitives::ISTANBUL
    } else if chain_spec.fork(EthereumHardfork::Petersburg).active_at_head(block) {
        revm_primitives::PETERSBURG
    } else if chain_spec.fork(EthereumHardfork::Byzantium).active_at_head(block) {
        revm_primitives::BYZANTIUM
    } else if chain_spec.fork(EthereumHardfork::SpuriousDragon).active_at_head(block) {
        revm_primitives::SPURIOUS_DRAGON
    } else if chain_spec.fork(EthereumHardfork::Tangerine).active_at_head(block) {
        revm_primitives::TANGERINE
    } else if chain_spec.fork(EthereumHardfork::Homestead).active_at_head(block) {
        revm_primitives::HOMESTEAD
    } else if chain_spec.fork(EthereumHardfork::Frontier).active_at_head(block) {
        revm_primitives::FRONTIER
    } else {
        panic!(
            "invalid hardfork chainspec: expected at least one hardfork, got {:?}",
            chain_spec.hardforks
        )
    }
}
