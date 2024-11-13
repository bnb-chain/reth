//! BSC Node types config.

use std::sync::Arc;

use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_bsc_chainspec::BscChainSpec;
use reth_bsc_consensus::Parlia;
use reth_bsc_engine::{BscEngineTypes, BscEngineValidator};
use reth_bsc_evm::{BscEvmConfig, BscExecutorProvider};
use reth_bsc_payload_builder::{BscBuiltPayload, BscPayloadBuilderAttributes};
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_network::NetworkHandle;
use reth_node_api::{
    AddOnsContext, ConfigureEvm, EngineValidator, FullNodeComponents, NodePrimitives,
    NodeTypesWithDB,
};
use reth_node_builder::{
    components::{
        ComponentsBuilder, ConsensusBuilder, ExecutorBuilder, NetworkBuilder, ParliaBuilder,
        PayloadServiceBuilder, PoolBuilder,
    },
    node::{FullNodeTypes, NodeTypes, NodeTypesWithEngine},
    rpc::{EngineValidatorBuilder, RpcAddOns},
    BuilderContext, Node, NodeAdapter, NodeComponentsBuilder, PayloadBuilderConfig, PayloadTypes,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_primitives::{Block, Header};
use reth_provider::CanonStateSubscriptions;
use reth_rpc::EthApi;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, EthTransactionPool, TransactionPool,
    TransactionValidationTaskExecutor,
};
use reth_trie_db::MerklePatriciaTrie;

/// Ethereum primitive types.
#[derive(Debug)]
pub struct BscPrimitives;

impl NodePrimitives for BscPrimitives {
    type Block = Block;
}

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
        BscParliaBuilder,
    >
    where
        Node: FullNodeTypes<Types: NodeTypes<ChainSpec = BscChainSpec>>,
        <Node::Types as NodeTypesWithEngine>::Engine: PayloadTypes<
            BuiltPayload = BscBuiltPayload,
            PayloadAttributes = EthPayloadAttributes,
            PayloadBuilderAttributes = BscPayloadBuilderAttributes,
        >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(BscPoolBuilder::default())
            .payload(BscPayloadBuilder::default())
            .network(BscNetworkBuilder::default())
            .executor(BscExecutorBuilder::default())
            .consensus(BscConsensusBuilder::default())
            .parlia(BscParliaBuilder::default())
    }
}

impl NodeTypes for BscNode {
    type Primitives = BscPrimitives;
    type ChainSpec = BscChainSpec;
    type StateCommitment = MerklePatriciaTrie;
}

impl NodeTypesWithEngine for BscNode {
    type Engine = BscEngineTypes;
}

/// Add-ons w.r.t. l1 bsc.
pub type BscAddOns<N> = RpcAddOns<
    N,
    EthApi<
        <N as FullNodeTypes>::Provider,
        <N as FullNodeComponents>::Pool,
        NetworkHandle,
        <N as FullNodeComponents>::Evm,
    >,
    BscEngineValidatorBuilder,
>;

impl<Types, N> Node<N> for BscNode
where
    Types: NodeTypesWithDB + NodeTypesWithEngine<Engine = BscEngineTypes, ChainSpec = BscChainSpec>,
    N: FullNodeTypes<Types = Types>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        BscPoolBuilder,
        BscPayloadBuilder,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
        BscParliaBuilder,
    >;

    type AddOns = BscAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components()
    }

    fn add_ons(&self) -> Self::AddOns {
        BscAddOns::default()
    }
}

/// A regular bsc evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for BscExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = BscChainSpec>>,
{
    type EVM = BscEvmConfig;

    type Executor = BscExecutorProvider<Node::Provider, Self::EVM>;

    async fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<(Self::EVM, Self::Executor)> {
        let chain_spec = ctx.chain_spec();
        let evm_config = BscEvmConfig::new(ctx.chain_spec());
        let executor = BscExecutorProvider::new(
            chain_spec,
            evm_config.clone(),
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
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = BscChainSpec>>,
{
    type Pool = EthTransactionPool<Node::Provider, DiskFileBlobStore>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;
        let validator = TransactionValidationTaskExecutor::eth_builder(Arc::new(
            ctx.chain_spec().inner.clone(),
        ))
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

impl BscPayloadBuilder {
    /// A helper method initializing [`PayloadBuilderService`] with the given EVM config.
    pub fn spawn<Types, Node, Evm, Pool>(
        self,
        evm_config: Evm,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<Types::Engine>>
    where
        Types: NodeTypesWithEngine<ChainSpec = BscChainSpec>,
        Node: FullNodeTypes<Types = Types>,
        Evm: ConfigureEvm<Header = Header>,
        Pool: TransactionPool + Unpin + 'static,
        Types::Engine: PayloadTypes<
            BuiltPayload = BscBuiltPayload,
            PayloadBuilderAttributes = BscPayloadBuilderAttributes,
        >,
    {
        let payload_builder = reth_bsc_payload_builder::BscPayloadBuilder::new(evm_config);
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
            payload_builder,
        );
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor().spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

impl<Types, Node, Pool> PayloadServiceBuilder<Node, Pool> for BscPayloadBuilder
where
    Types: NodeTypesWithEngine<ChainSpec = BscChainSpec>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool + Unpin + 'static,
    Types::Engine: PayloadTypes<
        BuiltPayload = BscBuiltPayload,
        PayloadBuilderAttributes = BscPayloadBuilderAttributes,
    >,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypesWithEngine>::Engine>> {
        self.spawn(BscEvmConfig::new(ctx.chain_spec()), ctx, pool)
    }
}

/// A basic bsc payload service.
#[derive(Debug, Default, Clone, Copy)]
pub struct BscNetworkBuilder {
    // TODO bsc
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for BscNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = BscChainSpec>>,
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
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = BscChainSpec>>,
{
    type Consensus = Parlia;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Parlia::new(ctx.chain_spec(), ctx.reth_config().parlia.clone()))
    }
}

/// Builder for [`BscEngineValidatorBuilder`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BscEngineValidatorBuilder;

impl<Node, Types> EngineValidatorBuilder<Node> for BscEngineValidatorBuilder
where
    Types: NodeTypesWithEngine<ChainSpec = BscChainSpec>,
    Node: FullNodeComponents<Types = Types>,
    BscEngineValidator: EngineValidator<Types::Engine>,
{
    type Validator = BscEngineValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(BscEngineValidator {})
    }
}

/// A basic bsc parlia builder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BscParliaBuilder;

impl<Node> ParliaBuilder<Node> for BscParliaBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = BscChainSpec>>,
{
    async fn build_parlia(self, ctx: &BuilderContext<Node>) -> eyre::Result<Parlia> {
        Ok(Parlia::new(ctx.chain_spec(), ctx.reth_config().parlia.clone()))
    }
}
