//! Contains RPC handler implementations specific to blocks.

use crate::{
    eth::{
        api::transactions::build_transaction_receipt_with_block_receipts,
        error::{EthApiError, EthResult},
    },
    EthApi,
};
use reth_evm::ConfigureEvm;
use reth_network_api::NetworkInfo;
use reth_primitives::{BlockId, TransactionMeta, B256};
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, EvmEnvProvider, StateProviderFactory};
use reth_rpc_types::{AnyTransactionReceipt, BlockSidecar, Header, Index, RichBlock};
use reth_rpc_types_compat::block::{from_block, uncle_block_from_header};
use reth_transaction_pool::TransactionPool;
use std::sync::Arc;

impl<Provider, Pool, Network, EvmConfig> EthApi<Provider, Pool, Network, EvmConfig>
where
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Pool: TransactionPool + Clone + 'static,
    Network: NetworkInfo + Send + Sync + 'static,
    EvmConfig: ConfigureEvm + 'static,
{
    /// Returns the uncle headers of the given block
    ///
    /// Returns an empty vec if there are none.
    pub(crate) fn ommers(
        &self,
        block_id: impl Into<BlockId>,
    ) -> EthResult<Option<Vec<reth_primitives::Header>>> {
        let block_id = block_id.into();
        Ok(self.provider().ommers_by_id(block_id)?)
    }

    pub(crate) async fn ommer_by_block_and_index(
        &self,
        block_id: impl Into<BlockId>,
        index: Index,
    ) -> EthResult<Option<RichBlock>> {
        let block_id = block_id.into();

        let uncles = if block_id.is_pending() {
            // Pending block can be fetched directly without need for caching
            self.provider().pending_block()?.map(|block| block.ommers)
        } else {
            self.provider().ommers_by_id(block_id)?
        }
        .unwrap_or_default();

        let index = usize::from(index);
        let uncle =
            uncles.into_iter().nth(index).map(|header| uncle_block_from_header(header).into());
        Ok(uncle)
    }

    /// Returns all transaction receipts in the block.
    ///
    /// Returns `None` if the block wasn't found.
    pub(crate) async fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> EthResult<Option<Vec<AnyTransactionReceipt>>> {
        // Fetch block and receipts based on block_id
        let block_and_receipts = if block_id.is_pending() {
            self.provider()
                .pending_block_and_receipts()?
                .map(|(sb, receipts)| (sb, Arc::new(receipts)))
        } else if let Some(block_hash) = self.provider().block_hash_for_id(block_id)? {
            self.cache().get_block_and_receipts(block_hash).await?
        } else {
            None
        };

        // If no block and receipts found, return None
        let Some((block, receipts)) = block_and_receipts else {
            return Ok(None);
        };

        // Extract block details
        let block_number = block.number;
        let base_fee = block.base_fee_per_gas;
        let block_hash = block.hash();
        let excess_blob_gas = block.excess_blob_gas;
        let timestamp = block.timestamp;
        let block = block.unseal();

        #[cfg(feature = "optimism")]
        let (block_timestamp, l1_block_info) = {
            let body = reth_evm_optimism::extract_l1_info(&block);
            (block.timestamp, body.ok())
        };

        // Build transaction receipts
        block
            .body
            .into_iter()
            .zip(receipts.iter())
            .enumerate()
            .map(|(idx, (tx, receipt))| {
                let meta = TransactionMeta {
                    tx_hash: tx.hash,
                    index: idx as u64,
                    block_hash,
                    block_number,
                    base_fee,
                    excess_blob_gas,
                    timestamp,
                };

                #[cfg(feature = "optimism")]
                let op_tx_meta =
                    self.build_op_tx_meta(&tx, l1_block_info.clone(), block_timestamp)?;

                build_transaction_receipt_with_block_receipts(
                    tx,
                    meta,
                    receipt.clone(),
                    &receipts,
                    #[cfg(feature = "optimism")]
                    op_tx_meta,
                )
            })
            .collect::<EthResult<Vec<_>>>()
            .map(Some)
    }

    /// Returns the number transactions in the given block.
    ///
    /// Returns `None` if the block does not exist
    pub(crate) async fn block_transaction_count(
        &self,
        block_id: impl Into<BlockId>,
    ) -> EthResult<Option<usize>> {
        let block_id = block_id.into();

        if block_id.is_pending() {
            // Pending block can be fetched directly without need for caching
            return Ok(self.provider().pending_block()?.map(|block| block.body.len()))
        }

        let block_hash = match self.provider().block_hash_for_id(block_id)? {
            Some(block_hash) => block_hash,
            None => return Ok(None),
        };

        Ok(self.cache().get_block_transactions(block_hash).await?.map(|txs| txs.len()))
    }

    /// Returns the block object for the given block id.
    pub(crate) async fn block(
        &self,
        block_id: impl Into<BlockId>,
    ) -> EthResult<Option<reth_primitives::SealedBlock>> {
        self.block_with_senders(block_id)
            .await
            .map(|maybe_block| maybe_block.map(|block| block.block))
    }

    /// Returns the block object for the given block id.
    pub(crate) async fn block_with_senders(
        &self,
        block_id: impl Into<BlockId>,
    ) -> EthResult<Option<reth_primitives::SealedBlockWithSenders>> {
        let block_id = block_id.into();

        if block_id.is_pending() {
            // Pending block can be fetched directly without need for caching
            let maybe_pending = self.provider().pending_block_with_senders()?;
            return if maybe_pending.is_some() {
                Ok(maybe_pending)
            } else {
                self.local_pending_block().await
            }
        }

        let block_hash = match self.provider().block_hash_for_id(block_id)? {
            Some(block_hash) => block_hash,
            None => return Ok(None),
        };

        Ok(self.cache().get_sealed_block_with_senders(block_hash).await?)
    }

    /// Returns the populated rpc block object for the given block id.
    ///
    /// If `full` is true, the block object will contain all transaction objects, otherwise it will
    /// only contain the transaction hashes.
    pub(crate) async fn rpc_block(
        &self,
        block_id: impl Into<BlockId>,
        full: bool,
    ) -> EthResult<Option<RichBlock>> {
        let block = match self.block_with_senders(block_id).await? {
            Some(block) => block,
            None => return Ok(None),
        };
        let block_hash = block.hash();
        let total_difficulty = self
            .provider()
            .header_td_by_number(block.number)?
            .ok_or(EthApiError::UnknownBlockNumber)?;
        let block = from_block(block.unseal(), total_difficulty, full.into(), Some(block_hash))?;
        Ok(Some(block.into()))
    }

    /// Returns the block header for the given block id.
    pub(crate) async fn rpc_block_header(
        &self,
        block_id: impl Into<BlockId>,
    ) -> EthResult<Option<Header>> {
        let header = self.rpc_block(block_id, false).await?.map(|block| block.inner.header);
        Ok(header)
    }

    /// Returns the sidecars for the given block id.
    pub(crate) async fn rpc_block_sidecars(
        &self,
        block_id: BlockId,
    ) -> EthResult<Option<Vec<BlockSidecar>>> {
        if block_id.is_pending() {
            return Ok(None);
        }

        let sidecars = if let Some(block_hash) = self.provider().block_hash_for_id(block_id)? {
            self.cache().get_sidecars(block_hash).await?
        } else {
            None
        };

        // If no block and sidecars found, return None
        let Some(sidecars) = sidecars else {
            return Ok(None);
        };

        let block_sidecars = sidecars
            .into_iter()
            .map(|o| BlockSidecar {
                blob_sidecar: o.blob_transaction_sidecar,
                block_number: o.block_number.to(),
                block_hash: o.block_hash,
                tx_index: o.tx_index,
                tx_hash: o.tx_hash,
            })
            .collect();

        Ok(Some(block_sidecars))
    }

    /// Returns the sidecar for the given transaction hash.
    pub(crate) async fn rpc_transaction_sidecar(
        &self,
        hash: B256,
    ) -> EthResult<Option<BlockSidecar>> {
        let meta = match self.provider().transaction_by_hash_with_meta(hash)? {
            Some((_, meta)) => meta,
            None => return Ok(None),
        };

        // If no block and receipts found, return None
        let Some(sidecars) = self.cache().get_sidecars(meta.block_hash).await? else {
            return Ok(None);
        };

        return if let Some(index) = sidecars.iter().position(|o| o.tx_hash == hash) {
            let block_sidecar = BlockSidecar {
                blob_sidecar: sidecars[index].blob_transaction_sidecar.clone(),
                block_number: sidecars[index].block_number.to(),
                block_hash: sidecars[index].block_hash,
                tx_index: sidecars[index].tx_index,
                tx_hash: sidecars[index].tx_hash,
            };

            Ok(Some(block_sidecar))
        } else {
            Ok(None)
        }
    }
}
