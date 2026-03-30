use reth_rpc::{EthFilter, EthPubSub};
use reth_rpc_eth_api::EthApiTypes;
use reth_rpc_eth_types::{logs_utils::ReceiptFilter, EthConfig};
use reth_tasks::TaskSpawner;
use std::sync::Arc;

/// Handlers for core, filter and pubsub `eth` namespace APIs.
#[derive(Debug, Clone)]
pub struct EthHandlers<EthApi: EthApiTypes> {
    /// Main `eth_` request handler
    pub api: EthApi,
    /// Polling based filter handler available on all transports
    pub filter: EthFilter<EthApi>,
    /// Handler for subscriptions only available for transports that support it (ws, ipc)
    pub pubsub: EthPubSub<EthApi>,
}

impl<EthApi> EthHandlers<EthApi>
where
    EthApi: EthApiTypes + 'static,
{
    /// Returns a new instance with the additional handlers for the `eth` namespace.
    ///
    /// This will spawn all necessary tasks for the additional handlers.
    pub fn bootstrap(
        config: EthConfig,
        executor: Box<dyn TaskSpawner + 'static>,
        eth_api: EthApi,
    ) -> Self {
        Self::bootstrap_with_receipt_filter(config, executor, eth_api, None)
    }

    /// Returns a new instance with an optional [`ReceiptFilter`].
    ///
    /// The receipt filter allows excluding certain receipts from log queries
    /// and `PubSub` log subscriptions (e.g., BSC system transaction logs).
    pub fn bootstrap_with_receipt_filter(
        config: EthConfig,
        executor: Box<dyn TaskSpawner + 'static>,
        eth_api: EthApi,
        receipt_filter: Option<Arc<dyn ReceiptFilter>>,
    ) -> Self {
        let filter = EthFilter::new_with_receipt_filter(
            eth_api.clone(),
            config.filter_config(),
            executor.clone(),
            receipt_filter.clone(),
        );

        let pubsub =
            EthPubSub::with_spawner_and_receipt_filter(eth_api.clone(), executor, receipt_filter);

        Self { api: eth_api, filter, pubsub }
    }
}
