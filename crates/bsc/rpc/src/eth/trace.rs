use std::future::Future;

use alloy_primitives::B256;
use alloy_rpc_types_eth::TransactionInfo;
use reth_evm::ConfigureEvm;
use reth_node_api::FullNodeComponents;
use reth_primitives::{
    revm_primitives::{db::Database, EnvWithHandlerCfg, ResultAndState},
    system_contracts::is_system_transaction,
    Header,
};
use reth_revm::database::StateProviderDatabase;
use reth_rpc_eth_api::helpers::{
    Call, LoadBlock, LoadPendingBlock, LoadState, LoadTransaction, Trace,
};
use reth_rpc_eth_types::{cache::db::StateCacheDbRefMutWrapper, EthApiError, StateCacheDb};
use revm::{db::CacheDB, Inspector};

use crate::BscEthApi;

impl<N> Trace for BscEthApi<N>
where
    Self: LoadState,
    N: FullNodeComponents,
{
    #[inline]
    fn evm_config(&self) -> &impl ConfigureEvm<Header = Header> {
        self.inner.evm_config()
    }

    fn spawn_trace_transaction_in_block_with_inspector<Insp, F, R>(
        &self,
        hash: B256,
        mut inspector: Insp,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, Self::Error>> + Send
    where
        Self: LoadPendingBlock + LoadTransaction + Call,
        F: FnOnce(
                TransactionInfo,
                Insp,
                ResultAndState,
                StateCacheDb<'_>,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Insp: for<'a, 'b> Inspector<StateCacheDbRefMutWrapper<'a, 'b>> + Send + 'static,
        R: Send + 'static,
    {
        async move {
            let (transaction, block) = match self.transaction_and_block(hash).await? {
                None => return Ok(None),
                Some(res) => res,
            };
            let (tx, tx_info) = transaction.split();

            let (cfg, block_env, _) = self.evm_env_at(block.hash().into()).await?;

            // we need to get the state of the parent block because we're essentially replaying the
            // block the transaction is included in
            let parent_block = block.parent_hash;
            let block_txs = block.into_transactions_ecrecovered();

            let parent_timestamp = LoadState::cache(self)
                .get_block(parent_block)
                .await?
                .map(|block| block.timestamp)
                .ok_or_else(|| EthApiError::UnknownParentBlock)?;

            let this = self.clone();
            self.spawn_with_state_at_block(parent_block.into(), move |state| {
                let mut db = CacheDB::new(StateProviderDatabase::new(state));

                // replay all transactions prior to the targeted transaction
                this.replay_transactions_until(
                    &mut db,
                    cfg.clone(),
                    block_env.clone(),
                    block_txs,
                    tx.hash,
                    parent_timestamp,
                )?;

                let mut tx_env = Call::evm_config(&this).tx_env(&tx);
                if is_system_transaction(&tx, tx.signer(), block_env.coinbase) {
                    tx_env.bsc.is_system_transaction = Some(true);
                };

                let env = EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env, tx_env);
                let (res, _) =
                    this.inspect(StateCacheDbRefMutWrapper(&mut db), env, &mut inspector)?;
                f(tx_info, inspector, res, db)
            })
            .await
            .map(Some)
        }
    }
}
