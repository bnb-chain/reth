use std::future::Future;

use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::TransactionInfo;
use reth_chainspec::EthereumHardforks;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_primitives::{
    revm_primitives::{
        db::DatabaseRef, BlockEnv, CfgEnvWithHandlerCfg, EnvWithHandlerCfg, ResultAndState,
    },
    system_contracts::{get_upgrade_system_contracts, is_system_transaction},
    Header, TransactionSignedEcRecovered,
};
use reth_revm::database::StateProviderDatabase;
use reth_rpc_eth_api::helpers::{
    Call, EthCall, LoadBlock, LoadPendingBlock, LoadState, LoadTransaction, SpawnBlocking,
};
use reth_rpc_eth_types::{EthApiError, StateCacheDb};
use revm::{
    bsc::SYSTEM_ADDRESS,
    db::{
        AccountState::{NotExisting, Touched},
        CacheDB,
    },
};

use crate::{BscEthApi, BscEthApiError};

impl<N> EthCall for BscEthApi<N>
where
    Self: Call,
    N: FullNodeComponents<Types: NodeTypes<ChainSpec: EthereumHardforks>>,
{
}

impl<N> Call for BscEthApi<N>
where
    Self: LoadState + SpawnBlocking,
    Self::Error: From<BscEthApiError>,
    N: FullNodeComponents,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }

    #[inline]
    fn evm_config(&self) -> &impl ConfigureEvm<Header = Header> {
        self.inner.evm_config()
    }

    fn spawn_replay_transaction<F, R>(
        &self,
        hash: B256,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, Self::Error>> + Send
    where
        Self: LoadBlock + LoadPendingBlock + LoadTransaction,
        F: FnOnce(TransactionInfo, ResultAndState, StateCacheDb<'_>) -> Result<R, Self::Error>
            + Send
            + 'static,
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

            let parent_timestamp = self
                .block(parent_block.into())
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

                let (res, _) = this.transact(&mut db, env)?;
                f(tx_info, res, db)
            })
            .await
            .map(Some)
        }
    }

    fn replay_transactions_until<DB>(
        &self,
        db: &mut CacheDB<DB>,
        cfg: CfgEnvWithHandlerCfg,
        block_env: BlockEnv,
        transactions: impl IntoIterator<Item = TransactionSignedEcRecovered>,
        target_tx_hash: B256,
        parent_timestamp: u64,
    ) -> Result<usize, Self::Error>
    where
        DB: DatabaseRef,
        EthApiError: From<DB::Error>,
    {
        #[allow(clippy::redundant_clone)]
        let env = EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env.clone(), Default::default());

        let mut evm = self.evm_config().evm_with_env(db, env);
        let mut index = 0;
        let mut before_system_tx = true;

        // try to upgrade system contracts before all txs if feynman is not active
        if !self.provider().chain_spec().is_feynman_active_at_timestamp(block_env.timestamp.to()) {
            let contracts = get_upgrade_system_contracts(
                self.provider().chain_spec().as_ref(),
                block_env.number.to(),
                block_env.timestamp.to(),
                parent_timestamp,
            )
            .expect("get upgrade system contracts failed");

            for (k, v) in contracts {
                let account =
                    evm.db_mut().load_account(k).map_err(|| BscEthApiError::LoadAccountFailed)?;
                if account.account_state == NotExisting {
                    account.account_state = Touched;
                }
                account.info.code_hash = v.clone().unwrap().hash_slow();
                account.info.code = v;
            }
        }

        for tx in transactions {
            // check if the transaction is a system transaction
            // this should be done before return
            if before_system_tx && is_system_transaction(&tx, tx.signer(), block_env.coinbase) {
                let sys_acc = evm
                    .db_mut()
                    .load_account(SYSTEM_ADDRESS)
                    .map_err(|| BscEthApiError::LoadAccountFailed)?;
                let balance = sys_acc.info.balance;
                if balance > U256::ZERO {
                    sys_acc.info.balance = U256::ZERO;

                    let val_acc = evm
                        .db_mut()
                        .load_account(block_env.coinbase)
                        .map_err(|| BscEthApiError::LoadAccountFailed)?;
                    if val_acc.account_state == NotExisting {
                        val_acc.account_state = Touched;
                    }
                    val_acc.info.balance += balance;
                }

                // try to upgrade system contracts between normal txs and system txs
                // if feynman is active
                if !self
                    .provider()
                    .chain_spec()
                    .is_feynman_active_at_timestamp(block_env.timestamp.to())
                {
                    let contracts = get_upgrade_system_contracts(
                        self.provider().chain_spec().as_ref(),
                        block_env.number.to(),
                        block_env.timestamp.to(),
                        parent_timestamp,
                    )
                    .expect("get upgrade system contracts failed");

                    for (k, v) in contracts {
                        let account = evm
                            .db_mut()
                            .load_account(k)
                            .map_err(|| BscEthApiError::LoadAccountFailed)?;
                        if account.account_state == NotExisting {
                            account.account_state = Touched;
                        }
                        account.info.code_hash = v.clone().unwrap().hash_slow();
                        account.info.code = v;
                    }
                }

                before_system_tx = false;
            }

            if tx.hash() == target_tx_hash {
                // reached the target transaction
                break
            }

            let sender = tx.signer();
            self.evm_config().fill_tx_env(evm.tx_mut(), &tx.into_signed(), sender);

            if !before_system_tx {
                evm.tx_mut().bsc.is_system_transaction = Some(true);
            };

            evm.transact_commit().map_err(BscEthApiError::from)?;
            index += 1;
        }
        Ok(index)
    }
}
