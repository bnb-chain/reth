//! Support for maintaining the state of the transaction pool

use crate::{
    blobstore::{BlobSidecarConverter, BlobStoreCanonTracker, BlobStoreUpdates},
    error::PoolError,
    metrics::MaintainPoolMetrics,
    traits::{CanonicalStateUpdate, EthPoolTransaction, TransactionPool, TransactionPoolExt},
    AllPoolTransactions, BlobTransactionSidecarVariant, BlockInfo, PoolTransaction, PoolUpdateKind,
    TransactionOrigin,
};
use alloy_consensus::{transaction::TxHashRef, BlockHeader, Typed2718};
use alloy_eips::{BlockNumberOrTag, Decodable2718, Encodable2718};
use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes};
use alloy_rlp::Encodable;
use futures_util::{
    future::{BoxFuture, Fuse, FusedFuture},
    FutureExt, Stream, StreamExt,
};
use reth_chain_state::CanonStateNotification;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_execution_types::ChangedAccount;
use reth_fs_util::FsPathError;
use reth_primitives_traits::{
    transaction::signed::SignedTransaction, NodePrimitives, SealedHeader,
};
use reth_storage_api::{errors::provider::ProviderError, BlockReaderIdExt, StateProviderFactory};
use reth_tasks::TaskSpawner;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Borrow,
    collections::HashSet,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    sync::oneshot,
    time::{self, Duration},
};
use tracing::{debug, error, info, trace, warn};

/// Maximum amount of time non-executable transaction are queued.
pub const MAX_QUEUED_TRANSACTION_LIFETIME: Duration = Duration::from_secs(3 * 60 * 60);

// The storage time of Sidecar is 19.2 days 19.2*86400/0.75 = 2211840
const FINALIZED_BLOCK_OFFSET: u64 = 2211840;

/// Additional settings for maintaining the transaction pool
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintainPoolConfig {
    /// Maximum (reorg) depth we handle when updating the transaction pool: `new.number -
    /// last_seen.number`
    ///
    /// Default: 64 (2 epochs)
    pub max_update_depth: u64,
    /// Maximum number of accounts to reload from state at once when updating the transaction pool.
    ///
    /// Default: 100
    pub max_reload_accounts: usize,

    /// Maximum amount of time non-executable, non local transactions are queued.
    /// Default: 3 hours
    pub max_tx_lifetime: Duration,

    /// Apply no exemptions to the locally received transactions.
    ///
    /// This includes:
    ///   - no price exemptions
    ///   - no eviction exemptions
    pub no_local_exemptions: bool,
}

impl Default for MaintainPoolConfig {
    fn default() -> Self {
        Self {
            max_update_depth: 64,
            max_reload_accounts: 100,
            max_tx_lifetime: MAX_QUEUED_TRANSACTION_LIFETIME,
            no_local_exemptions: false,
        }
    }
}

/// Settings for local transaction backup task
#[derive(Debug, Clone, Default)]
pub struct LocalTransactionBackupConfig {
    /// Path to transactions backup file
    pub transactions_path: Option<PathBuf>,
}

impl LocalTransactionBackupConfig {
    /// Receive path to transactions backup and return initialized config
    pub const fn with_local_txs_backup(transactions_path: PathBuf) -> Self {
        Self { transactions_path: Some(transactions_path) }
    }
}

/// Returns a spawnable future for maintaining the state of the transaction pool.
pub fn maintain_transaction_pool_future<N, Client, P, St, Tasks>(
    client: Client,
    pool: P,
    events: St,
    task_spawner: Tasks,
    config: MaintainPoolConfig,
) -> BoxFuture<'static, ()>
where
    N: NodePrimitives,
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = N::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks>
        + Clone
        + 'static,
    P: TransactionPoolExt<Transaction: PoolTransaction<Consensus = N::SignedTx>> + 'static,
    St: Stream<Item = CanonStateNotification<N>> + Send + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
{
    async move {
        maintain_transaction_pool(client, pool, events, task_spawner, config).await;
    }
    .boxed()
}

/// Maintains the state of the transaction pool by handling new blocks and reorgs.
///
/// This listens for any new blocks and reorgs and updates the transaction pool's state accordingly
pub async fn maintain_transaction_pool<N, Client, P, St, Tasks>(
    client: Client,
    pool: P,
    mut events: St,
    task_spawner: Tasks,
    config: MaintainPoolConfig,
) where
    N: NodePrimitives,
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = N::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks>
        + Clone
        + 'static,
    P: TransactionPoolExt<Transaction: PoolTransaction<Consensus = N::SignedTx>> + 'static,
    St: Stream<Item = CanonStateNotification<N>> + Send + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
{
    let metrics = MaintainPoolMetrics::default();
    let MaintainPoolConfig { max_update_depth, max_reload_accounts, .. } = config;
    // ensure the pool points to latest state
    if let Ok(Some(latest)) = client.header_by_number_or_tag(BlockNumberOrTag::Latest) {
        let latest = SealedHeader::seal_slow(latest);
        let chain_spec = client.chain_spec();
        let info = BlockInfo {
            block_gas_limit: latest.gas_limit(),
            last_seen_block_hash: latest.hash(),
            last_seen_block_number: latest.number(),
            pending_basefee: chain_spec
                .next_block_base_fee(latest.header(), latest.timestamp())
                .unwrap_or_default(),
            pending_blob_fee: latest
                .maybe_next_block_blob_fee(chain_spec.blob_params_at_timestamp(latest.timestamp())),
        };
        pool.set_block_info(info);
    }

    // keeps track of mined blob transaction so we can clean finalized transactions
    let mut blob_store_tracker = BlobStoreCanonTracker::default();

    // keeps track of the latest finalized block
    let mut last_finalized_block =
        FinalizedBlockTracker::new(client.finalized_block_number().ok().flatten());

    // keeps track of any dirty accounts that we know of are out of sync with the pool
    let mut dirty_addresses = HashSet::default();

    // keeps track of the state of the pool wrt to blocks
    let mut maintained_state = MaintainedPoolState::InSync;

    // the future that reloads accounts from state
    let mut reload_accounts_fut = Fuse::terminated();

    // eviction interval for stale non local txs
    let mut stale_eviction_interval = time::interval(config.max_tx_lifetime);

    // toggle for the first notification
    let mut first_event = true;

    // The update loop that waits for new blocks and reorgs and performs pool updated
    // Listen for new chain events and derive the update action for the pool
    loop {
        trace!(target: "txpool", state=?maintained_state, "awaiting new block or reorg");

        metrics.set_dirty_accounts_len(dirty_addresses.len());
        let pool_info = pool.block_info();

        // after performing a pool update after a new block we have some time to properly update
        // dirty accounts and correct if the pool drifted from current state, for example after
        // restart or a pipeline run
        if maintained_state.is_drifted() {
            metrics.inc_drift();
            // assuming all senders are dirty
            dirty_addresses = pool.unique_senders();
            // make sure we toggle the state back to in sync
            maintained_state = MaintainedPoolState::InSync;
        }

        // if we have accounts that are out of sync with the pool, we reload them in chunks
        if !dirty_addresses.is_empty() && reload_accounts_fut.is_terminated() {
            let (tx, rx) = oneshot::channel();
            let c = client.clone();
            let at = pool_info.last_seen_block_hash;
            let fut = if dirty_addresses.len() > max_reload_accounts {
                // need to chunk accounts to reload
                let accs_to_reload =
                    dirty_addresses.iter().copied().take(max_reload_accounts).collect::<Vec<_>>();
                for acc in &accs_to_reload {
                    // make sure we remove them from the dirty set
                    dirty_addresses.remove(acc);
                }
                async move {
                    let res = load_accounts(c, at, accs_to_reload.into_iter());
                    let _ = tx.send(res);
                }
                .boxed()
            } else {
                // can fetch all dirty accounts at once
                let accs_to_reload = std::mem::take(&mut dirty_addresses);
                async move {
                    let res = load_accounts(c, at, accs_to_reload.into_iter());
                    let _ = tx.send(res);
                }
                .boxed()
            };
            reload_accounts_fut = rx.fuse();
            task_spawner.spawn_blocking(fut);
        }

        // check if we have a new finalized block
        if let Some(finalized) =
            last_finalized_block.update(client.finalized_block_number().ok().flatten()) &&
            finalized > FINALIZED_BLOCK_OFFSET
        {
            debug!(target: "txpool", finalized_block = %finalized, "finalized block");
            if let BlobStoreUpdates::Finalized(blobs) =
                blob_store_tracker.on_finalized_block(finalized - FINALIZED_BLOCK_OFFSET)
            {
                let num_blobs = blobs.len();
                metrics.inc_deleted_tracked_blobs(num_blobs);
                // remove all finalized blobs from the blob store
                pool.delete_blobs(blobs);
                // and also do periodic cleanup
                let pool = pool.clone();
                task_spawner.spawn_blocking(Box::pin(async move {
                    debug!(target: "txpool", finalized_block = %finalized, num_blobs = %num_blobs, "cleaning up blob store");
                    pool.cleanup_blobs();
                }));
            }
        }

        // outcomes of the futures we are waiting on
        let mut event = None;
        let mut reloaded = None;

        // select of account reloads and new canonical state updates which should arrive at the rate
        // of the block time
        tokio::select! {
            res = &mut reload_accounts_fut =>  {
                reloaded = Some(res);
            }
            ev = events.next() =>  {
                 if ev.is_none() {
                    // the stream ended, we are done
                    break;
                }
                event = ev;
                // on receiving the first event on start up, mark the pool as drifted to explicitly
                // trigger revalidation and clear out outdated txs.
                if first_event {
                    maintained_state = MaintainedPoolState::Drifted;
                    first_event = false
                }
            }
            _ = stale_eviction_interval.tick() => {
                let queued = pool
                    .queued_transactions();
                let mut stale_blobs = Vec::new();
                let now = std::time::Instant::now();
                let stale_txs: Vec<_> = queued
                    .into_iter()
                    .filter(|tx| {
                        // filter stale transactions based on config
                        (tx.origin.is_external() || config.no_local_exemptions) && now - tx.timestamp > config.max_tx_lifetime
                    })
                    .map(|tx| {
                        if tx.is_eip4844() {
                            stale_blobs.push(*tx.hash());
                        }
                        *tx.hash()
                    })
                    .collect();
                debug!(target: "txpool", count=%stale_txs.len(), "removing stale transactions");
                pool.remove_transactions(stale_txs);
                pool.delete_blobs(stale_blobs);
            }
        }
        // handle the result of the account reload
        match reloaded {
            Some(Ok(Ok(LoadedAccounts { accounts, failed_to_load }))) => {
                // reloaded accounts successfully
                // extend accounts we failed to load from database
                dirty_addresses.extend(failed_to_load);
                // update the pool with the loaded accounts
                pool.update_accounts(accounts);
            }
            Some(Ok(Err(res))) => {
                // Failed to load accounts from state
                let (accs, err) = *res;
                debug!(target: "txpool", %err, "failed to load accounts");
                dirty_addresses.extend(accs);
            }
            Some(Err(_)) => {
                // failed to receive the accounts, sender dropped, only possible if task panicked
                maintained_state = MaintainedPoolState::Drifted;
            }
            None => {}
        }

        // handle the new block or reorg
        let Some(event) = event else { continue };
        match event {
            CanonStateNotification::Reorg { old, new } => {
                let (old_blocks, old_state) = old.inner();
                let (new_blocks, new_state) = new.inner();
                let new_tip = new_blocks.tip();
                let new_first = new_blocks.first();
                let old_first = old_blocks.first();

                // check if the reorg is not canonical with the pool's block
                if !(old_first.parent_hash() == pool_info.last_seen_block_hash ||
                    new_first.parent_hash() == pool_info.last_seen_block_hash)
                {
                    // the new block points to a higher block than the oldest block in the old chain
                    maintained_state = MaintainedPoolState::Drifted;
                }

                let chain_spec = client.chain_spec();

                // fees for the next block: `new_tip+1`
                let pending_block_base_fee = chain_spec
                    .next_block_base_fee(new_tip.header(), new_tip.timestamp())
                    .unwrap_or_default();
                let pending_block_blob_fee = new_tip.header().maybe_next_block_blob_fee(
                    chain_spec.blob_params_at_timestamp(new_tip.timestamp()),
                );

                // we know all changed account in the new chain
                let new_changed_accounts: HashSet<_> =
                    new_state.changed_accounts().map(ChangedAccountEntry).collect();

                // find all accounts that were changed in the old chain but _not_ in the new chain
                let missing_changed_acc = old_state
                    .accounts_iter()
                    .map(|(a, _)| a)
                    .filter(|addr| !new_changed_accounts.contains(addr));

                // for these we need to fetch the nonce+balance from the db at the new tip
                let mut changed_accounts =
                    match load_accounts(client.clone(), new_tip.hash(), missing_changed_acc) {
                        Ok(LoadedAccounts { accounts, failed_to_load }) => {
                            // extend accounts we failed to load from database
                            dirty_addresses.extend(failed_to_load);

                            accounts
                        }
                        Err(err) => {
                            let (addresses, err) = *err;
                            debug!(
                                target: "txpool",
                                %err,
                                "failed to load missing changed accounts at new tip: {:?}",
                                new_tip.hash()
                            );
                            dirty_addresses.extend(addresses);
                            vec![]
                        }
                    };

                // also include all accounts from new chain
                // we can use extend here because they are unique
                changed_accounts.extend(new_changed_accounts.into_iter().map(|entry| entry.0));

                // all transactions mined in the new chain
                let new_mined_transactions: HashSet<_> = new_blocks.transaction_hashes().collect();

                // update the pool then re-inject the pruned transactions
                // find all transactions that were mined in the old chain but not in the new chain
                let pruned_old_transactions = old_blocks
                    .transactions_ecrecovered()
                    .filter(|tx| !new_mined_transactions.contains(tx.tx_hash()))
                    .filter_map(|tx| {
                        if tx.is_eip4844() {
                            // reorged blobs no longer include the blob, which is necessary for
                            // validating the transaction. Even though the transaction could have
                            // been validated previously, we still need the blob in order to
                            // accurately set the transaction's
                            // encoded-length which is propagated over the network.
                            pool.get_blob(*tx.tx_hash())
                                .ok()
                                .flatten()
                                .map(Arc::unwrap_or_clone)
                                .and_then(|sidecar| {
                                    <P as TransactionPool>::Transaction::try_from_eip4844(
                                        tx, sidecar,
                                    )
                                })
                        } else {
                            <P as TransactionPool>::Transaction::try_from_consensus(tx).ok()
                        }
                    })
                    .collect::<Vec<_>>();

                // update the pool first
                let update = CanonicalStateUpdate {
                    new_tip: new_tip.sealed_block(),
                    pending_block_base_fee,
                    pending_block_blob_fee,
                    changed_accounts,
                    // all transactions mined in the new chain need to be removed from the pool
                    mined_transactions: new_blocks.transaction_hashes().collect(),
                    update_kind: PoolUpdateKind::Reorg,
                };
                pool.on_canonical_state_change(update);

                // all transactions that were mined in the old chain but not in the new chain need
                // to be re-injected
                //
                // Note: we no longer know if the tx was local or external
                // Because the transactions are not finalized, the corresponding blobs are still in
                // blob store (if we previously received them from the network)
                metrics.inc_reinserted_transactions(pruned_old_transactions.len());
                let _ = pool.add_external_transactions(pruned_old_transactions).await;

                // keep track of new mined blob transactions
                blob_store_tracker.add_new_chain_blocks(&new_blocks);
            }
            CanonStateNotification::Commit { new } => {
                let (blocks, state) = new.inner();
                let tip = blocks.tip();
                let chain_spec = client.chain_spec();

                // fees for the next block: `tip+1`
                let pending_block_base_fee = chain_spec
                    .next_block_base_fee(tip.header(), tip.timestamp())
                    .unwrap_or_default();
                let pending_block_blob_fee = tip.header().maybe_next_block_blob_fee(
                    chain_spec.blob_params_at_timestamp(tip.timestamp()),
                );

                let first_block = blocks.first();
                trace!(
                    target: "txpool",
                    first = first_block.number(),
                    tip = tip.number(),
                    pool_block = pool_info.last_seen_block_number,
                    "update pool on new commit"
                );

                // check if the depth is too large and should be skipped, this could happen after
                // initial sync or long re-sync
                let depth = tip.number().abs_diff(pool_info.last_seen_block_number);
                if depth > max_update_depth {
                    maintained_state = MaintainedPoolState::Drifted;
                    debug!(target: "txpool", ?depth, "skipping deep canonical update");
                    let info = BlockInfo {
                        block_gas_limit: tip.header().gas_limit(),
                        last_seen_block_hash: tip.hash(),
                        last_seen_block_number: tip.number(),
                        pending_basefee: pending_block_base_fee,
                        pending_blob_fee: pending_block_blob_fee,
                    };
                    pool.set_block_info(info);

                    // keep track of mined blob transactions
                    blob_store_tracker.add_new_chain_blocks(&blocks);

                    continue
                }

                let mut changed_accounts = Vec::with_capacity(state.state().len());
                for acc in state.changed_accounts() {
                    // we can always clear the dirty flag for this account
                    dirty_addresses.remove(&acc.address);
                    changed_accounts.push(acc);
                }

                let mined_transactions = blocks.transaction_hashes().collect();

                // check if the range of the commit is canonical with the pool's block
                if first_block.parent_hash() != pool_info.last_seen_block_hash {
                    // we received a new canonical chain commit but the commit is not canonical with
                    // the pool's block, this could happen after initial sync or
                    // long re-sync
                    maintained_state = MaintainedPoolState::Drifted;
                }

                // Canonical update
                let update = CanonicalStateUpdate {
                    new_tip: tip.sealed_block(),
                    pending_block_base_fee,
                    pending_block_blob_fee,
                    changed_accounts,
                    mined_transactions,
                    update_kind: PoolUpdateKind::Commit,
                };
                pool.on_canonical_state_change(update);

                // keep track of mined blob transactions
                blob_store_tracker.add_new_chain_blocks(&blocks);

                // If Osaka activates in 2 slots we need to convert blobs to new format.
                if !chain_spec.is_osaka_active_at_timestamp(tip.timestamp()) &&
                    !chain_spec.is_osaka_active_at_timestamp(tip.timestamp().saturating_add(12)) &&
                    chain_spec.is_osaka_active_at_timestamp(tip.timestamp().saturating_add(24))
                {
                    let pool = pool.clone();
                    let spawner = task_spawner.clone();
                    let client = client.clone();
                    task_spawner.spawn(Box::pin(async move {
                        // Start converting not eaerlier than 4 seconds into current slot to ensure
                        // that our pool only contains valid transactions for the next block (as
                        // it's not Osaka yet).
                        tokio::time::sleep(Duration::from_secs(4)).await;

                        let mut interval = tokio::time::interval(Duration::from_secs(1));
                        loop {
                            // Loop and replace blob transactions until we reach Osaka transition
                            // block after which no legacy blobs are going to be accepted.
                            let last_iteration =
                                client.latest_header().ok().flatten().is_none_or(|header| {
                                    client
                                        .chain_spec()
                                        .is_osaka_active_at_timestamp(header.timestamp())
                                });

                            let AllPoolTransactions { pending, queued } = pool.all_transactions();
                            for tx in pending
                                .into_iter()
                                .chain(queued)
                                .filter(|tx| tx.transaction.is_eip4844())
                            {
                                let tx_hash = *tx.transaction.hash();

                                // Fetch sidecar from the pool
                                let Ok(Some(sidecar)) = pool.get_blob(tx_hash) else {
                                    continue;
                                };
                                // Ensure it is a legacy blob
                                if !sidecar.is_eip4844() {
                                    continue;
                                }
                                // Remove transaction and sidecar from the pool, both are in memory
                                // now
                                let Some(tx) = pool.remove_transactions(vec![tx_hash]).pop() else {
                                    continue;
                                };
                                pool.delete_blob(tx_hash);

                                let BlobTransactionSidecarVariant::Eip4844(sidecar) =
                                    Arc::unwrap_or_clone(sidecar)
                                else {
                                    continue;
                                };

                                let converter = BlobSidecarConverter::new();
                                let pool = pool.clone();
                                spawner.spawn(Box::pin(async move {
                                    // Convert sidecar to EIP-7594 format
                                    let Some(sidecar) = converter.convert(sidecar).await else {
                                        return;
                                    };

                                    // Re-insert transaction with the new sidecar
                                    let origin = tx.origin;
                                    let Some(tx) = EthPoolTransaction::try_from_eip4844(
                                        tx.transaction.clone_into_consensus(),
                                        sidecar.into(),
                                    ) else {
                                        return;
                                    };
                                    let _ = pool.add_transaction(origin, tx).await;
                                }));
                            }

                            if last_iteration {
                                break;
                            }

                            interval.tick().await;
                        }
                    }));
                }
            }
        }
    }
}

struct FinalizedBlockTracker {
    last_finalized_block: Option<BlockNumber>,
}

impl FinalizedBlockTracker {
    const fn new(last_finalized_block: Option<BlockNumber>) -> Self {
        Self { last_finalized_block }
    }

    /// Updates the tracked finalized block and returns the new finalized block if it changed
    fn update(&mut self, finalized_block: Option<BlockNumber>) -> Option<BlockNumber> {
        let finalized = finalized_block?;
        self.last_finalized_block.is_none_or(|last| last < finalized).then(|| {
            self.last_finalized_block = Some(finalized);
            finalized
        })
    }
}

/// Keeps track of the pool's state, whether the accounts in the pool are in sync with the actual
/// state.
#[derive(Debug, PartialEq, Eq)]
enum MaintainedPoolState {
    /// Pool is assumed to be in sync with the current state
    InSync,
    /// Pool could be out of sync with the state
    Drifted,
}

impl MaintainedPoolState {
    /// Returns `true` if the pool is assumed to be out of sync with the current state.
    #[inline]
    const fn is_drifted(&self) -> bool {
        matches!(self, Self::Drifted)
    }
}

/// A unique [`ChangedAccount`] identified by its address that can be used for deduplication
#[derive(Eq)]
struct ChangedAccountEntry(ChangedAccount);

impl PartialEq for ChangedAccountEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0.address == other.0.address
    }
}

impl Hash for ChangedAccountEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.address.hash(state);
    }
}

impl Borrow<Address> for ChangedAccountEntry {
    fn borrow(&self) -> &Address {
        &self.0.address
    }
}

#[derive(Default)]
struct LoadedAccounts {
    /// All accounts that were loaded
    accounts: Vec<ChangedAccount>,
    /// All accounts that failed to load
    failed_to_load: Vec<Address>,
}

/// Loads all accounts at the given state
///
/// Returns an error with all given addresses if the state is not available.
///
/// Note: this expects _unique_ addresses
fn load_accounts<Client, I>(
    client: Client,
    at: BlockHash,
    addresses: I,
) -> Result<LoadedAccounts, Box<(HashSet<Address>, ProviderError)>>
where
    I: IntoIterator<Item = Address>,
    Client: StateProviderFactory,
{
    let addresses = addresses.into_iter();
    let mut res = LoadedAccounts::default();
    let state = match client.history_by_block_hash(at) {
        Ok(state) => state,
        Err(err) => return Err(Box::new((addresses.collect(), err))),
    };
    for addr in addresses {
        if let Ok(maybe_acc) = state.basic_account(&addr) {
            let acc = maybe_acc
                .map(|acc| ChangedAccount { address: addr, nonce: acc.nonce, balance: acc.balance })
                .unwrap_or_else(|| ChangedAccount::empty(addr));
            res.accounts.push(acc)
        } else {
            // failed to load account.
            res.failed_to_load.push(addr);
        }
    }
    Ok(res)
}

/// Loads transactions from a file, decodes them from the JSON or RLP format, and
/// inserts them into the transaction pool on node boot up.
/// The file is removed after the transactions have been successfully processed.
async fn load_and_reinsert_transactions<P>(
    pool: P,
    file_path: &Path,
) -> Result<(), TransactionsBackupError>
where
    P: TransactionPool<Transaction: PoolTransaction<Consensus: SignedTransaction>>,
{
    if !file_path.exists() {
        return Ok(())
    }

    debug!(target: "txpool", txs_file =?file_path, "Check local persistent storage for saved transactions");
    let data = reth_fs_util::read(file_path)?;

    if data.is_empty() {
        return Ok(())
    }

    let pool_transactions: Vec<(TransactionOrigin, <P as TransactionPool>::Transaction)> =
        if let Ok(tx_backups) = serde_json::from_slice::<Vec<TxBackup>>(&data) {
            tx_backups
                .into_iter()
                .filter_map(|backup| {
                    let tx_signed =
                        <P::Transaction as PoolTransaction>::Consensus::decode_2718_exact(
                            backup.rlp.as_ref(),
                        )
                        .ok()?;
                    let recovered = tx_signed.try_into_recovered().ok()?;
                    let pool_tx =
                        <P::Transaction as PoolTransaction>::try_from_consensus(recovered).ok()?;

                    Some((backup.origin, pool_tx))
                })
                .collect()
        } else {
            let txs_signed: Vec<<P::Transaction as PoolTransaction>::Consensus> =
                alloy_rlp::Decodable::decode(&mut data.as_slice())?;

            txs_signed
                .into_iter()
                .filter_map(|tx| tx.try_into_recovered().ok())
                .filter_map(|tx| {
                    <P::Transaction as PoolTransaction>::try_from_consensus(tx)
                        .ok()
                        .map(|pool_tx| (TransactionOrigin::Local, pool_tx))
                })
                .collect()
        };

    let inserted = futures_util::future::join_all(
        pool_transactions.into_iter().map(|(origin, tx)| pool.add_transaction(origin, tx)),
    )
    .await;

    info!(target: "txpool", txs_file =?file_path, num_txs=%inserted.len(), "Successfully reinserted local transactions from file");
    reth_fs_util::remove_file(file_path)?;
    Ok(())
}

fn save_local_txs_backup<P>(pool: P, file_path: &Path)
where
    P: TransactionPool<Transaction: PoolTransaction<Consensus: Encodable>>,
{
    let local_transactions = pool.get_local_transactions();
    if local_transactions.is_empty() {
        trace!(target: "txpool", "no local transactions to save");
        return
    }

    let local_transactions = local_transactions
        .into_iter()
        .map(|tx| {
            let consensus_tx = tx.transaction.clone_into_consensus().into_inner();
            let rlp_data = consensus_tx.encoded_2718();

            TxBackup { rlp: rlp_data.into(), origin: tx.origin }
        })
        .collect::<Vec<_>>();

    let json_data = match serde_json::to_string(&local_transactions) {
        Ok(data) => data,
        Err(err) => {
            warn!(target: "txpool", %err, txs_file=?file_path, "failed to serialize local transactions to json");
            return
        }
    };

    info!(target: "txpool", txs_file =?file_path, num_txs=%local_transactions.len(), "Saving current local transactions");
    let parent_dir = file_path.parent().map(std::fs::create_dir_all).transpose();

    match parent_dir.map(|_| reth_fs_util::write(file_path, json_data)) {
        Ok(_) => {
            info!(target: "txpool", txs_file=?file_path, "Wrote local transactions to file");
        }
        Err(err) => {
            warn!(target: "txpool", %err, txs_file=?file_path, "Failed to write local transactions to file");
        }
    }
}

/// A transaction backup that is saved as json to a file for
/// reinsertion into the pool
#[derive(Debug, Deserialize, Serialize)]
pub struct TxBackup {
    /// Encoded transaction
    pub rlp: Bytes,
    /// The origin of the transaction
    pub origin: TransactionOrigin,
}

/// Errors possible during txs backup load and decode
#[derive(thiserror::Error, Debug)]
pub enum TransactionsBackupError {
    /// Error during RLP decoding of transactions
    #[error("failed to apply transactions backup. Encountered RLP decode error: {0}")]
    Decode(#[from] alloy_rlp::Error),
    /// Error during json decoding of transactions
    #[error("failed to apply transactions backup. Encountered JSON decode error: {0}")]
    Json(#[from] serde_json::Error),
    /// Error during file upload
    #[error("failed to apply transactions backup. Encountered file error: {0}")]
    FsPath(#[from] FsPathError),
    /// Error adding transactions to the transaction pool
    #[error("failed to insert transactions to the transactions pool. Encountered pool error: {0}")]
    Pool(#[from] PoolError),
}

/// Task which manages saving local transactions to the persistent file in case of shutdown.
/// Reloads the transactions from the file on the boot up and inserts them into the pool.
pub async fn backup_local_transactions_task<P>(
    shutdown: reth_tasks::shutdown::GracefulShutdown,
    pool: P,
    config: LocalTransactionBackupConfig,
) where
    P: TransactionPool<Transaction: PoolTransaction<Consensus: SignedTransaction>> + Clone,
{
    let Some(transactions_path) = config.transactions_path else {
        // nothing to do
        return
    };

    if let Err(err) = load_and_reinsert_transactions(pool.clone(), &transactions_path).await {
        error!(target: "txpool", "{}", err)
    }

    let graceful_guard = shutdown.await;

    // write transactions to disk
    save_local_txs_backup(pool, &transactions_path);

    drop(graceful_guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
        CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionOrigin,
    };
    use alloy_consensus::Transaction;
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{hex, U256};
    use reth_ethereum_primitives::PooledTransactionVariant;
    use reth_fs_util as fs;
    use reth_primitives_traits::SignedTransaction;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_tasks::TaskManager;

    #[test]
    fn changed_acc_entry() {
        let changed_acc = ChangedAccountEntry(ChangedAccount::empty(Address::random()));
        let mut copy = changed_acc.0;
        copy.nonce = 10;
        assert!(changed_acc.eq(&ChangedAccountEntry(copy)));
    }

    const EXTENSION: &str = "json";
    const FILENAME: &str = "test_transactions_backup";

    #[tokio::test(flavor = "multi_thread")]
    async fn test_save_local_txs_backup() {
        let temp_dir = tempfile::tempdir().unwrap();
        let transactions_path = temp_dir.path().join(FILENAME).with_extension(EXTENSION);
        // Use a transaction with max_priority_fee_per_gas > 0 to pass BSC TipZero validation.
        let raw = "0x02f914950181ad84b2d05e0085117553845b830f7df88080b9143a6040608081523462000414576200133a803803806200001e8162000419565b9283398101608082820312620004145781516001600160401b03908181116200041457826200004f9185016200043f565b92602092838201519083821162000414576200006d9183016200043f565b8186015190946001600160a01b03821692909183900362000414576060015190805193808511620003145760038054956001938488811c9816801562000409575b89891014620003f3578190601f988981116200039d575b50899089831160011462000336576000926200032a575b505060001982841b1c191690841b1781555b8751918211620003145760049788548481811c9116801562000309575b89821014620002f457878111620002a9575b5087908784116001146200023e5793839491849260009562000232575b50501b92600019911b1c19161785555b6005556007805460ff60a01b19169055600880546001600160a01b0319169190911790553015620001f3575060025469d3c21bcecceda100000092838201809211620001de57506000917fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9160025530835282815284832084815401905584519384523093a351610e889081620004b28239f35b601190634e487b7160e01b6000525260246000fd5b90606493519262461bcd60e51b845283015260248201527f45524332303a206d696e7420746f20746865207a65726f2061646472657373006044820152fd5b0151935038806200013a565b9190601f198416928a600052848a6000209460005b8c8983831062000291575050501062000276575b50505050811b0185556200014a565b01519060f884600019921b161c191690553880808062000267565b86860151895590970196948501948893500162000253565b89600052886000208880860160051c8201928b8710620002ea575b0160051c019085905b828110620002dd5750506200011d565b60008155018590620002cd565b92508192620002c4565b60228a634e487b7160e01b6000525260246000fd5b90607f16906200010b565b634e487b7160e01b600052604160045260246000fd5b015190503880620000dc565b90869350601f19831691856000528b6000209260005b8d8282106200038657505084116200036d575b505050811b018155620000ee565b015160001983861b60f8161c191690553880806200035f565b8385015186558a979095019493840193016200034c565b90915083600052896000208980850160051c8201928c8610620003e9575b918891869594930160051c01915b828110620003d9575050620000c5565b60008155859450889101620003c9565b92508192620003bb565b634e487b7160e01b600052602260045260246000fd5b97607f1697620000ae565b600080fd5b6040519190601f01601f191682016001600160401b038111838210176200031457604052565b919080601f84011215620004145782516001600160401b038111620003145760209062000475601f8201601f1916830162000419565b92818452828287010111620004145760005b8181106200049d57508260009394955001015290565b85810183015184820184015282016200048756fe608060408181526004918236101561001657600080fd5b600092833560e01c91826306fdde0314610a1c57508163095ea7b3146109f257816318160ddd146109d35781631b4c84d2146109ac57816323b872dd14610833578163313ce5671461081757816339509351146107c357816370a082311461078c578163715018a6146107685781638124f7ac146107495781638da5cb5b1461072057816395d89b411461061d578163a457c2d714610575578163a9059cbb146104e4578163c9567bf914610120575063dd62ed3e146100d557600080fd5b3461011c578060031936011261011c57806020926100f1610b5a565b6100f9610b75565b6001600160a01b0391821683526001865283832091168252845220549051908152f35b5080fd5b905082600319360112610338576008546001600160a01b039190821633036104975760079283549160ff8360a01c1661045557737a250d5630b4cf539739df2c5dacb4c659f2488d92836bffffffffffffffffffffffff60a01b8092161786553087526020938785528388205430156104065730895260018652848920828a52865280858a205584519081527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925863092a38554835163c45a015560e01b815290861685828581845afa9182156103dd57849187918b946103e7575b5086516315ab88c960e31b815292839182905afa9081156103dd576044879289928c916103c0575b508b83895196879586946364e329cb60e11b8652308c870152166024850152165af19081156103b6579086918991610389575b50169060065416176006558385541660604730895288865260c4858a20548860085416928751958694859363f305d71960e01b8552308a86015260248501528d60448501528d606485015260848401524260a48401525af1801561037f579084929161034c575b50604485600654169587541691888551978894859363095ea7b360e01b855284015260001960248401525af1908115610343575061030c575b5050805460ff60a01b1916600160a01b17905580f35b81813d831161033c575b6103208183610b8b565b8101031261033857518015150361011c5738806102f6565b8280fd5b503d610316565b513d86823e3d90fd5b6060809293503d8111610378575b6103648183610b8b565b81010312610374578290386102bd565b8580fd5b503d61035a565b83513d89823e3d90fd5b6103a99150863d88116103af575b6103a18183610b8b565b810190610e33565b38610256565b503d610397565b84513d8a823e3d90fd5b6103d79150843d86116103af576103a18183610b8b565b38610223565b85513d8b823e3d90fd5b6103ff919450823d84116103af576103a18183610b8b565b92386101fb565b845162461bcd60e51b81528085018790526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b6020606492519162461bcd60e51b8352820152601760248201527f74726164696e6720697320616c7265616479206f70656e0000000000000000006044820152fd5b608490602084519162461bcd60e51b8352820152602160248201527f4f6e6c79206f776e65722063616e2063616c6c20746869732066756e6374696f6044820152603760f91b6064820152fd5b9050346103385781600319360112610338576104fe610b5a565b9060243593303303610520575b602084610519878633610bc3565b5160018152f35b600594919454808302908382041483151715610562576127109004820391821161054f5750925080602061050b565b634e487b7160e01b815260118552602490fd5b634e487b7160e01b825260118652602482fd5b9050823461061a578260031936011261061a57610590610b5a565b918360243592338152600160205281812060018060a01b03861682526020522054908282106105c9576020856105198585038733610d31565b608490602086519162461bcd60e51b8352820152602560248201527f45524332303a2064656372656173656420616c6c6f77616e63652062656c6f77604482015264207a65726f60d81b6064820152fd5b80fd5b83833461011c578160031936011261011c57805191809380549160019083821c92828516948515610716575b6020958686108114610703578589529081156106df5750600114610687575b6106838787610679828c0383610b8b565b5191829182610b11565b0390f35b81529295507f8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd19b5b8284106106cc57505050826106839461067992820101948680610668565b80548685018801529286019281016106ae565b60ff19168887015250505050151560051b8301019250610679826106838680610668565b634e487b7160e01b845260228352602484fd5b93607f1693610649565b50503461011c578160031936011261011c5760085490516001600160a01b039091168152602090f35b50503461011c578160031936011261011c576020906005549051908152f35b833461061a578060031936011261061a57600880546001600160a01b031916905580f35b50503461011c57602036600319011261011c5760209181906001600160a01b036107b4610b5a565b16815280845220549051908152f35b82843461061a578160031936011261061a576107dd610b5a565b338252600160209081528383206001600160a01b038316845290528282205460243581019290831061054f57602084610519858533610d31565b50503461011c578160031936011261011c576020905160128152f35b83833461011c57606036600319011261011c5761084e610b5a565b610856610b75565b6044359160018060a01b0381169485815260209560018752858220338352875285822054976000198903610893575b505050906105199291610bc3565b85891061096957811561091a5733156108cc5750948481979861051997845260018a528284203385528a52039120558594938780610885565b865162461bcd60e51b8152908101889052602260248201527f45524332303a20617070726f766520746f20746865207a65726f206164647265604482015261737360f01b6064820152608490fd5b865162461bcd60e51b81529081018890526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b865162461bcd60e51b8152908101889052601d60248201527f45524332303a20696e73756666696369656e7420616c6c6f77616e63650000006044820152606490fd5b50503461011c578160031936011261011c5760209060ff60075460a01c1690519015158152f35b50503461011c578160031936011261011c576020906002549051908152f35b50503461011c578060031936011261011c57602090610519610a12610b5a565b6024359033610d31565b92915034610b0d5783600319360112610b0d57600354600181811c9186908281168015610b03575b6020958686108214610af05750848852908115610ace5750600114610a75575b6106838686610679828b0383610b8b565b929550600383527fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b5b828410610abb575050508261068394610679928201019438610a64565b8054868501880152928601928101610a9e565b60ff191687860152505050151560051b83010192506106798261068338610a64565b634e487b7160e01b845260229052602483fd5b93607f1693610a44565b8380fd5b6020808252825181830181905290939260005b828110610b4657505060409293506000838284010152601f8019910116010190565b818101860151848201604001528501610b24565b600435906001600160a01b0382168203610b7057565b600080fd5b602435906001600160a01b0382168203610b7057565b90601f8019910116810190811067ffffffffffffffff821117610bad57604052565b634e487b7160e01b600052604160045260246000fd5b6001600160a01b03908116918215610cde5716918215610c8d57600082815280602052604081205491808310610c3957604082827fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef958760209652828652038282205586815220818154019055604051908152a3565b60405162461bcd60e51b815260206004820152602660248201527f45524332303a207472616e7366657220616d6f756e7420657863656564732062604482015265616c616e636560d01b6064820152608490fd5b60405162461bcd60e51b815260206004820152602360248201527f45524332303a207472616e7366657220746f20746865207a65726f206164647260448201526265737360e81b6064820152608490fd5b60405162461bcd60e51b815260206004820152602560248201527f45524332303a207472616e736665722066726f6d20746865207a65726f206164604482015264647265737360d81b6064820152608490fd5b6001600160a01b03908116918215610de25716918215610d925760207f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925918360005260018252604060002085600052825280604060002055604051908152a3565b60405162461bcd60e51b815260206004820152602260248201527f45524332303a20617070726f766520746f20746865207a65726f206164647265604482015261737360f01b6064820152608490fd5b60405162461bcd60e51b8152602060048201526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b90816020910312610b7057516001600160a01b0381168103610b70579056fea2646970667358221220285c200b3978b10818ff576bb83f2dc4a2a7c98dfb6a36ea01170de792aa652764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000d3fd4f95820a9aa848ce716d6c200eaefb9a2e4900000000000000000000000000000000000000000000000000000000000000640000000000000000000000000000000000000000000000000000000000000003543131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000035431310000000000000000000000000000000000000000000000000000000000c001a04e551c75810ffdfe6caff57da9f5a8732449f42f0f4c57f935b05250a76db3b6a046cd47e6d01914270c1ec0d9ac7fae7dfb240ec9a8b6ec7898c4d6aa174388f2";
        let data = hex::decode(raw).unwrap();
        let tx = PooledTransactionVariant::decode_2718(&mut data.as_ref()).unwrap();
        let provider = MockEthProvider::default();
        let transaction = EthPooledTransaction::from_pooled(tx.try_into_recovered().unwrap());
        let tx_to_cmp = transaction.clone();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );
        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider).build(blob_store.clone());

        let txpool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store.clone(),
            Default::default(),
        );

        txpool.add_transaction(TransactionOrigin::Local, transaction.clone()).await.unwrap();

        let handle = tokio::runtime::Handle::current();
        let manager = TaskManager::new(handle);
        let config = LocalTransactionBackupConfig::with_local_txs_backup(transactions_path.clone());
        manager.executor().spawn_critical_with_graceful_shutdown_signal("test task", |shutdown| {
            backup_local_transactions_task(shutdown, txpool.clone(), config)
        });

        let mut txns = txpool.get_local_transactions();
        let tx_on_finish = txns.pop().expect("there should be 1 transaction");

        assert_eq!(*tx_to_cmp.hash(), *tx_on_finish.hash());

        // shutdown the executor
        manager.graceful_shutdown();

        let data = fs::read(transactions_path).unwrap();

        let txs: Vec<TxBackup> = serde_json::from_slice::<Vec<TxBackup>>(&data).unwrap();
        assert_eq!(txs.len(), 1);

        temp_dir.close().unwrap();
    }

    #[test]
    fn test_update_with_higher_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(10));
        assert_eq!(tracker.update(Some(15)), Some(15));
        assert_eq!(tracker.last_finalized_block, Some(15));
    }

    #[test]
    fn test_update_with_lower_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(20));
        assert_eq!(tracker.update(Some(15)), None);
        // finalized block should NOT go backwards
        assert_eq!(tracker.last_finalized_block, Some(20));
    }

    #[test]
    fn test_update_with_equal_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(20));
        assert_eq!(tracker.update(Some(20)), None);
        assert_eq!(tracker.last_finalized_block, Some(20));
    }

    #[test]
    fn test_update_with_no_last_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(None);
        assert_eq!(tracker.update(Some(10)), Some(10));
        assert_eq!(tracker.last_finalized_block, Some(10));
    }

    #[test]
    fn test_update_with_no_new_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(10));
        assert_eq!(tracker.update(None), None);
        assert_eq!(tracker.last_finalized_block, Some(10));
    }

    #[test]
    fn test_update_with_no_finalized_blocks() {
        let mut tracker = FinalizedBlockTracker::new(None);
        assert_eq!(tracker.update(None), None);
        assert_eq!(tracker.last_finalized_block, None);
    }
}
