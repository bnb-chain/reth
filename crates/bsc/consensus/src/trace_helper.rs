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

// define here to avoid dependency on revm/bsc
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
        before_system_tx: bool,
    ) {
        let is_feynman_active =
            self.parlia.chain_spec().is_feynman_active_at_timestamp(block_env.timestamp.to());

        if (before_system_tx && !is_feynman_active) || (!before_system_tx && is_feynman_active) {
            self.do_upgrade(db, block_env, parent_timestamp);
        }
    }

    pub fn add_block_reward<ExtDB: DatabaseRef>(
        &self,
        db: &mut CacheDB<ExtDB>,
        block_env: &BlockEnv,
    ) {
        let sys_acc =
            db.load_account(SYSTEM_ADDRESS).map_err(|_| "load system account failed").unwrap();
        let balance = sys_acc.info.balance;
        if balance > U256::ZERO {
            sys_acc.info.balance = U256::ZERO;

            let val_acc = db
                .load_account(block_env.coinbase)
                .map_err(|_| "load validator account failed")
                .unwrap();
            if val_acc.account_state == NotExisting {
                val_acc.account_state = Touched;
            }
            val_acc.info.balance += balance;
        }
    }

    fn do_upgrade<ExtDB: DatabaseRef>(
        &self,
        db: &mut CacheDB<ExtDB>,
        block_env: &BlockEnv,
        parent_timestamp: u64,
    ) {
        let contracts = get_upgrade_system_contracts(
            self.parlia.chain_spec(),
            block_env.number.to(),
            block_env.timestamp.to(),
            parent_timestamp,
        )
        .map_err(|_| "get upgrade system contracts failed")
        .unwrap();

        for (k, v) in contracts {
            let account = db.load_account(k).map_err(|_| "load system contract failed").unwrap();
            if account.account_state == NotExisting {
                account.account_state = Touched;
            }
            account.info.code_hash = v.clone().unwrap().hash_slow();
            account.info.code = v;
        }
    }
}
