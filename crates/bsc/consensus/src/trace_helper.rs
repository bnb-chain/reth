use std::sync::Arc;

use alloy_primitives::{address, Address, U256};
use reth_bsc_forks::BscHardforks;
use reth_bsc_primitives::system_contracts::get_upgrade_system_contracts;
use reth_primitives::revm_primitives::{db::DatabaseRef, BlockEnv};
use reth_revm::db::{
    AccountState::{NotExisting, Touched},
    CacheDB,
};

use crate::Parlia;

// redefine to avoid dependency on revm/bsc
const SYSTEM_ADDRESS: Address = address!("fffffffffffffffffffffffffffffffffffffffe");

#[derive(Debug, Clone)]
pub struct BscTraceHelper {
    parlia: Arc<Parlia>,
}

impl BscTraceHelper {
    pub const fn new(parlia: Arc<Parlia>) -> Self {
        Self { parlia }
    }

    pub fn upgrade_system_contracts<ExtDB: DatabaseRef>(
        &self,
        db: &mut CacheDB<ExtDB>,
        block_env: &BlockEnv,
        parent_timestamp: u64,
        before_tx: bool,
    ) -> Result<(), BscTraceHelperError> {
        let is_feynman_active =
            self.parlia.chain_spec().is_feynman_active_at_timestamp(block_env.timestamp.to());

        if (before_tx && !is_feynman_active) || (!before_tx && is_feynman_active) {
            let contracts = get_upgrade_system_contracts(
                self.parlia.chain_spec(),
                block_env.number.to(),
                block_env.timestamp.to(),
                parent_timestamp,
            )
            .map_err(|_| BscTraceHelperError::GetUpgradeSystemContractsFailed)?;

            for (k, v) in contracts {
                let account =
                    db.load_account(k).map_err(|_| BscTraceHelperError::LoadAccountFailed).unwrap();
                if account.account_state == NotExisting {
                    account.account_state = Touched;
                }
                account.info.code_hash = v.clone().unwrap().hash_slow();
                account.info.code = v;
            }
        }

        Ok(())
    }

    pub fn add_block_reward<ExtDB: DatabaseRef>(
        &self,
        db: &mut CacheDB<ExtDB>,
        block_env: &BlockEnv,
    ) -> Result<(), BscTraceHelperError> {
        let sys_acc =
            db.load_account(SYSTEM_ADDRESS).map_err(|_| BscTraceHelperError::LoadAccountFailed)?;
        let balance = sys_acc.info.balance;
        if balance > U256::ZERO {
            sys_acc.info.balance = U256::ZERO;

            let val_acc = db
                .load_account(block_env.coinbase)
                .map_err(|_| BscTraceHelperError::LoadAccountFailed)?;
            if val_acc.account_state == NotExisting {
                val_acc.account_state = Touched;
            }
            val_acc.info.balance += balance;
        }

        Ok(())
    }
}

/// Errors that can occur when calling `BscTraceHelper` methods
#[derive(Debug, thiserror::Error)]
pub enum BscTraceHelperError {
    #[error("Failed to load account from db")]
    LoadAccountFailed,
    #[error("Failed to get upgrade system contracts")]
    GetUpgradeSystemContractsFailed,
}
