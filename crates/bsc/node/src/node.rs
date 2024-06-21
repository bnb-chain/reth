//! BSC Node types config.

use crate::EthEngineTypes;
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_bsc_consensus::Parlia;
use reth_evm_bsc::{BscEvmConfig, BscExecutorProvider};
use reth_network::NetworkHandle;
use reth_node_builder::{
    components::{
        ComponentsBuilder, ConsensusBuilder, ExecutorBuilder, NetworkBuilder,
        PayloadServiceBuilder, PoolBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, Node, PayloadBuilderConfig,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, EthTransactionPool, TransactionPool,
    TransactionValidationTaskExecutor,
};

/// Type configuration for a regular BSC node.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscNode;

impl BscNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular BSC node.
    pub fn components<Node>() -> ComponentsBuilder<
        Node,
        BscPoolBuilder,
        BscPayloadBuilder,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Engine = EthEngineTypes>,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(BscPoolBuilder::default())
            .payload(BscPayloadBuilder::default())
            .network(BscNetworkBuilder::default())
            .executor(BscExecutorBuilder::default())
            .consensus(BscConsensusBuilder::default())
    }
}

impl NodeTypes for BscNode {
    type Primitives = ();
    type Engine = EthEngineTypes;
}

impl<N> Node<N> for BscNode
where
    N: FullNodeTypes<Engine = EthEngineTypes>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        BscPoolBuilder,
        BscPayloadBuilder,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
    >;

    fn components_builder(self) -> Self::ComponentsBuilder {
        Self::components()
    }
}

/// A regular bsc evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for BscExecutorBuilder
where
    Node: FullNodeTypes,
{
    type EVM = BscEvmConfig;

    type Executor = BscExecutorProvider<Node::Provider, Self::EVM>;

    async fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<(Self::EVM, Self::Executor)> {
        let chain_spec = ctx.chain_spec();
        let evm_config = BscEvmConfig::default();
        let executor = BscExecutorProvider::new(
            chain_spec,
            evm_config,
            ctx.reth_config().parlia.clone(),
            ctx.provider().clone(),
        );

        Ok((evm_config, executor))
    }
}

/// A basic bsc transaction pool.
///
/// This contains various settings that can be configured and take precedence over the node's
/// config.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscPoolBuilder {
    // TODO add options for txpool args
}

impl<Node> PoolBuilder<Node> for BscPoolBuilder
where
    Node: FullNodeTypes,
{
    type Pool = EthTransactionPool<Node::Provider, DiskFileBlobStore>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.chain_spec())
            .with_head_timestamp(ctx.head().timestamp)
            .kzg_settings(ctx.kzg_settings()?)
            .with_additional_tasks(1)
            .build_with_tasks(
                ctx.provider().clone(),
                ctx.task_executor().clone(),
                blob_store.clone(),
            );

        let transaction_pool =
            reth_transaction_pool::Pool::eth_pool(validator, blob_store, ctx.pool_config());
        info!(target: "reth::cli", "Transaction pool initialized");
        let transactions_path = data_dir.txpool_transactions();

        // spawn txpool maintenance task
        {
            let pool = transaction_pool.clone();
            let chain_events = ctx.provider().canonical_state_stream();
            let client = ctx.provider().clone();
            let transactions_backup_config =
                reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

            ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
                "local transactions backup task",
                |shutdown| {
                    reth_transaction_pool::maintain::backup_local_transactions_task(
                        shutdown,
                        pool.clone(),
                        transactions_backup_config,
                    )
                },
            );

            // spawn the maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client,
                    pool,
                    chain_events,
                    ctx.task_executor().clone(),
                    Default::default(),
                ),
            );
            debug!(target: "reth::cli", "Spawned txpool maintenance task");
        }

        Ok(transaction_pool)
    }
}

/// A basic bsc payload service.
// TODO: bsc
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BscPayloadBuilder;

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for BscPayloadBuilder
where
    Node: FullNodeTypes<Engine = EthEngineTypes>,
    Pool: TransactionPool + Unpin + 'static,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<Node::Engine>> {
        let payload_builder = reth_ethereum_payload_builder::EthereumPayloadBuilder::default();
        let conf = ctx.payload_builder_config();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval())
            .deadline(conf.deadline())
            .max_payload_tasks(conf.max_payload_tasks())
            .extradata(conf.extradata_bytes());

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            ctx.chain_spec(),
            payload_builder,
        );
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor().spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

/// A basic bsc payload service.
#[derive(Debug, Default, Clone, Copy)]
pub struct BscNetworkBuilder {
    // TODO bsc
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for BscNetworkBuilder
where
    Node: FullNodeTypes,
    Pool: TransactionPool + Unpin + 'static,
{
    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<NetworkHandle> {
        let network = ctx.network_builder().await?;
        let handle = ctx.start_network(network, pool);

        Ok(handle)
    }
}

/// A basic bsc consensus builder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BscConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for BscConsensusBuilder
where
    Node: FullNodeTypes,
{
    type Consensus = Parlia;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Parlia::new(ctx.chain_spec(), ctx.reth_config().parlia.clone()))
    }
}
