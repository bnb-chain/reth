//! Contains RPC handler implementations specific to endpoints that call/execute within evm.

use crate::EthApi;
use alloy_primitives::{Address, B256};
use reth_bsc_primitives::system_contracts::is_system_transaction;
use reth_evm::{ConfigureEvm, ConfigureEvmEnv};
use reth_primitives::{Header, TransactionSigned};
use reth_rpc_eth_api::{
    helpers::{Call, EthCall, LoadPendingBlock, LoadState, SpawnBlocking},
    FromEvmError,
};
use reth_rpc_eth_types::EthApiError;
use revm_primitives::{
    db::{Database, DatabaseCommit},
    BlockEnv, CfgEnvWithHandlerCfg, EnvWithHandlerCfg,
};

impl<Provider, Pool, Network, EvmConfig> EthCall for EthApi<Provider, Pool, Network, EvmConfig> where
    Self: Call + LoadPendingBlock
{
}

impl<Provider, Pool, Network, EvmConfig> Call for EthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadState<Evm: ConfigureEvm<Header = Header>> + SpawnBlocking,
    EvmConfig: ConfigureEvm<Header = Header>,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }

    /// Replays all the transactions until the target transaction is found.
    fn replay_transactions_until<'a, DB, I>(
        &self,
        db: &mut DB,
        cfg: CfgEnvWithHandlerCfg,
        block_env: BlockEnv,
        transactions: I,
        target_tx_hash: B256,
        parent_timestamp: u64,
    ) -> Result<usize, Self::Error>
    where
        DB: Database + DatabaseCommit,
        EthApiError: From<DB::Error>,
        I: IntoIterator<Item = (&'a Address, &'a TransactionSigned)>,
    {
        #[allow(clippy::redundant_clone)]
        let env = EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env.clone(), Default::default());

        let mut evm = self.evm_config().evm_with_env(db, env);
        let mut index = 0;

        let is_bsc = self.bsc_trace_helper.is_some();
        let mut before_system_tx = is_bsc;

        // try to upgrade system contracts for bsc before all txs if feynman is not active
        if is_bsc {
            if let Some(trace_helper) = self.bsc_trace_helper.as_ref() {
                trace_helper
                    .upgrade_system_contracts(evm.db_mut(), &block_env, parent_timestamp, true)
                    .map_err(|e| e.into())?;
            }
        }

        for (sender, tx) in transactions {
            // check if the transaction is a system transaction
            // this should be done before return
            if is_bsc && before_system_tx && is_system_transaction(&tx, *sender, block_env.coinbase)
            {
                if let Some(trace_helper) = self.bsc_trace_helper.as_ref() {
                    // move block reward from the system address to the coinbase
                    trace_helper
                        .add_block_reward(evm.db_mut(), &block_env)
                        .map_err(|e| e.into())?;

                    // try to upgrade system contracts between normal txs and system txs
                    // if feynman is active
                    trace_helper
                        .upgrade_system_contracts(evm.db_mut(), &block_env, parent_timestamp, false)
                        .map_err(|e| e.into())?;
                }

                before_system_tx = false;
            }

            if tx.hash() == target_tx_hash {
                // reached the target transaction
                break
            }

            self.evm_config().fill_tx_env(evm.tx_mut(), &tx, *sender);

            #[cfg(feature = "bsc")]
            if !before_system_tx {
                evm.tx_mut().bsc.is_system_transaction = Some(true);
            };

            evm.transact_commit().map_err(Self::Error::from_evm_err)?;
            index += 1;
        }
        Ok(index)
    }
}
