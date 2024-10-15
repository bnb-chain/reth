//! Bsc-Reth `eth_` endpoint implementation.

mod call;
mod trace;

use std::{fmt, sync::Arc};

use alloy_network::AnyNetwork;
use alloy_primitives::U256;
use derive_more::Deref;
use reth_bsc_forks::BscHardforks;
use reth_chainspec::EthereumHardforks;
use reth_evm::{provider::EvmEnvProvider, ConfigureEvm};
use reth_network_api::NetworkInfo;
use reth_node_api::{BuilderProvider, FullNodeComponents, FullNodeTypes, NodeTypes};
use reth_node_builder::EthApiBuilderCtx;
use reth_primitives::Header;
use reth_provider::{
    BlockIdReader, BlockNumReader, BlockReaderIdExt, ChainSpecProvider, HeaderProvider,
    StageCheckpointReader, StateProviderFactory,
};
use reth_rpc::eth::{core::EthApiInner, DevSigner, EthTxBuilder};
use reth_rpc_eth_api::{
    helpers::{
        AddDevSigners, EthApiSpec, EthFees, EthSigner, EthState, LoadBlock, LoadFee,
        LoadPendingBlock, LoadState, SpawnBlocking, Trace,
    },
    EthApiTypes,
};
use reth_rpc_eth_types::{EthStateCache, FeeHistoryCache, GasPriceOracle, PendingBlock};
use reth_tasks::{
    pool::{BlockingTaskGuard, BlockingTaskPool},
    TaskSpawner,
};
use reth_transaction_pool::TransactionPool;

use crate::BscEthApiError;

/// Adapter for [`EthApiInner`], which holds all the data required to serve core `eth_` API.
pub type EthApiNodeBackend<N> = EthApiInner<
    <N as FullNodeTypes>::Provider,
    <N as FullNodeComponents>::Pool,
    <N as FullNodeComponents>::Network,
    <N as FullNodeComponents>::Evm,
>;

/// Bsc-Reth `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
///
/// This wraps a default `Eth` implementation, and provides additional functionality where the
/// optimism spec deviates from the default (ethereum) spec, e.g. transaction forwarding to the
/// sequencer, receipts, additional RPC fields for transaction receipts.
///
/// This type implements the [`FullEthApi`](reth_rpc_eth_api::helpers::FullEthApi) by implemented
/// all the `Eth` helper traits and prerequisite traits.
#[derive(Clone, Deref)]
pub struct BscEthApi<N: FullNodeComponents> {
    /// Gateway to node's core components.
    #[deref]
    inner: Arc<EthApiNodeBackend<N>>,
}

impl<N: FullNodeComponents> BscEthApi<N> {
    /// Creates a new instance for given context.
    #[allow(clippy::type_complexity)]
    pub fn with_spawner(ctx: &EthApiBuilderCtx<N, Self>) -> Self {
        let blocking_task_pool =
            BlockingTaskPool::build().expect("failed to build blocking task pool");

        let inner = EthApiInner::new(
            ctx.provider.clone(),
            ctx.pool.clone(),
            ctx.network.clone(),
            ctx.cache.clone(),
            ctx.new_gas_price_oracle(),
            ctx.config.rpc_gas_cap,
            ctx.config.rpc_max_simulate_blocks,
            ctx.config.eth_proof_window,
            blocking_task_pool,
            ctx.new_fee_history_cache(),
            ctx.evm_config.clone(),
            ctx.executor.clone(),
            ctx.config.proof_permits,
        );

        Self { inner: Arc::new(inner) }
    }
}

impl<N> EthApiTypes for BscEthApi<N>
where
    Self: Send + Sync,
    N: FullNodeComponents,
{
    type Error = BscEthApiError;
    // todo: replace with alloy_network::Bsc
    type NetworkTypes = AnyNetwork;
    type TransactionCompat = EthTxBuilder;
}

impl<N> EthApiSpec for BscEthApi<N>
where
    Self: Send + Sync,
    N: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
{
    #[inline]
    fn provider(
        &self,
    ) -> impl ChainSpecProvider<ChainSpec: EthereumHardforks> + BlockNumReader + StageCheckpointReader
    {
        self.inner.provider()
    }

    #[inline]
    fn network(&self) -> impl NetworkInfo {
        self.inner.network()
    }

    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.starting_block()
    }

    #[inline]
    fn signers(&self) -> &parking_lot::RwLock<Vec<Box<dyn EthSigner>>> {
        self.inner.signers()
    }
}

impl<N> SpawnBlocking for BscEthApi<N>
where
    Self: Send + Sync + Clone + 'static,
    N: FullNodeComponents,
{
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.inner.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.blocking_task_guard()
    }
}

impl<N> LoadFee for BscEthApi<N>
where
    Self: LoadBlock,
    N: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
{
    #[inline]
    fn provider(
        &self,
    ) -> impl BlockIdReader + HeaderProvider + ChainSpecProvider<ChainSpec: EthereumHardforks> {
        self.inner.provider()
    }

    #[inline]
    fn cache(&self) -> &EthStateCache {
        self.inner.cache()
    }

    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<impl BlockReaderIdExt> {
        self.inner.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache {
        self.inner.fee_history_cache()
    }
}

impl<N> LoadPendingBlock for BscEthApi<N>
where
    Self: SpawnBlocking,
    N: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
{
    #[inline]
    fn provider(
        &self,
    ) -> impl BlockReaderIdExt
           + EvmEnvProvider
           + ChainSpecProvider<ChainSpec: EthereumHardforks>
           + StateProviderFactory {
        self.inner.provider()
    }

    #[inline]
    fn pool(&self) -> impl TransactionPool {
        self.inner.pool()
    }

    #[inline]
    fn pending_block(&self) -> &tokio::sync::Mutex<Option<PendingBlock>> {
        self.inner.pending_block()
    }

    #[inline]
    fn evm_config(&self) -> &impl ConfigureEvm<Header = Header> {
        self.inner.evm_config()
    }
}

impl<N> LoadState for BscEthApi<N>
where
    Self: Send + Sync + Clone,
    N: FullNodeComponents<Types: NodeTypes<ChainSpec: BscHardforks>>,
{
    #[inline]
    fn provider(&self) -> impl StateProviderFactory + ChainSpecProvider<ChainSpec: BscHardforks> {
        self.inner.provider()
    }

    #[inline]
    fn cache(&self) -> &EthStateCache {
        self.inner.cache()
    }

    #[inline]
    fn pool(&self) -> impl TransactionPool {
        self.inner.pool()
    }
}

impl<N> EthState for BscEthApi<N>
where
    Self: LoadState + SpawnBlocking,
    N: FullNodeComponents,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_proof_window()
    }
}

impl<N> EthFees for BscEthApi<N>
where
    Self: LoadFee,
    N: FullNodeComponents,
{
}

impl<N> AddDevSigners for BscEthApi<N>
where
    N: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
{
    fn with_dev_accounts(&self) {
        *self.signers().write() = DevSigner::random_signers(20)
    }
}

impl<N> BuilderProvider<N> for BscEthApi<N>
where
    Self: Send,
    N: FullNodeComponents,
{
    type Ctx<'a> = &'a EthApiBuilderCtx<N, Self>;

    fn builder() -> Box<dyn for<'a> Fn(Self::Ctx<'a>) -> Self + Send> {
        Box::new(Self::with_spawner)
    }
}

impl<N: FullNodeComponents> fmt::Debug for BscEthApi<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BscEthApi").finish_non_exhaustive()
    }
}
