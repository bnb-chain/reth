//! Binding immutable overlay dependencies to a database read snapshot.

use crate::{
    builder::AnchorForParent, changeset_cache::ChangesetRangeKey, database_state_frontiers,
    OverlayBuilder, OverlayManager,
};
use alloy_eips::BlockNumHash;
use alloy_primitives::{map::B256Map, B256};
use parking_lot::RwLock;
use reth_chain_state::{BlockState, ExecutedBlock};
use reth_errors::{ProviderError, ProviderResult};
use reth_ethereum_primitives::EthPrimitives;
use reth_primitives_traits::{dashmap::DashMap, AlloyBlockHeader, NodePrimitives};
use reth_storage_api::{
    BlockNumReader, DatabaseProviderFactory, PruneCheckpointReader, StageCheckpointReader,
};
use reth_trie::updates::TrieUpdatesSorted;
use std::{fmt, ops::Deref, sync::Arc};

/// Maximum block references captured for one reader, including both branches.
///
/// Capture is synchronous and must not walk an unbounded retained graph. This bounds references,
/// not the size of the already executed blocks shared by those references.
pub(crate) const MAX_CAPTURE_BLOCKS: usize = 16_384;

/// A database reader and the overlay builder bound to that reader's snapshot.
///
/// The builder owns immutable block dependencies, but no transaction or capture lock. Keep this
/// provider and builder together; a different database snapshot requires a new capture.
#[derive(Debug)]
pub struct OverlayReadView<P, N: NodePrimitives = EthPrimitives> {
    /// The database snapshot opened during capture.
    pub provider: P,
    /// The builder whose dependencies were captured for `provider`.
    pub builder: OverlayBuilder<N>,
}

/// Failure to bind a complete overlay view to a database snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayReadError {
    /// A source is being pruned. Capture never waits while holding a database reader.
    SourceBusy,
    /// A required block was no longer available when this reader captured its dependencies.
    MissingBlock {
        /// Hash whose execution/trie data is missing.
        hash: B256,
        /// Durable state/trie frontier of the reader.
        partial: BlockNumHash,
        /// Durable non-state frontier of the reader.
        finish: BlockNumHash,
    },
    /// Retained data is not a consecutive hash-linked chain.
    DisconnectedChain {
        /// Hash at the broken link.
        hash: B256,
    },
    /// The dependency closure exceeds the synchronous capture budget.
    CaptureLimit,
    /// A bound builder was paired with different durable frontiers.
    SnapshotMismatch,
}

impl fmt::Display for OverlayReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceBusy => f.write_str("overlay block source is being pruned; retry with a fresh reader"),
            Self::MissingBlock { hash, partial, finish } => write!(
                f,
                "overlay read context is missing block {hash} for partial #{} ({}) and Finish #{} ({})",
                partial.number, partial.hash, finish.number, finish.hash,
            ),
            Self::DisconnectedChain { hash } => write!(f, "overlay read context has a disconnected chain at {hash}"),
            Self::CaptureLimit => write!(f, "overlay read context exceeds {MAX_CAPTURE_BLOCKS} block references"),
            Self::SnapshotMismatch => f.write_str("overlay read context does not match the database frontiers"),
        }
    }
}

impl std::error::Error for OverlayReadError {}

/// The live index and the gate shared by every manager that can read or prune this source.
#[derive(Debug)]
pub(crate) struct OverlayBlockSource<N: NodePrimitives> {
    blocks: DashMap<B256, ExecutedBlock<N>>,
    pub(crate) capture: RwLock<()>,
}

impl<N: NodePrimitives> Default for OverlayBlockSource<N> {
    fn default() -> Self {
        Self { blocks: Default::default(), capture: Default::default() }
    }
}

impl<N: NodePrimitives> Deref for OverlayBlockSource<N> {
    type Target = DashMap<B256, ExecutedBlock<N>>;

    fn deref(&self) -> &Self::Target {
        &self.blocks
    }
}

/// No cursors, readers, computing entries or live-index guards are retained here.
#[derive(Debug, Clone)]
pub(crate) struct OverlayReadContext<N: NodePrimitives> {
    pub(crate) partial: BlockNumHash,
    pub(crate) finish: BlockNumHash,
    pub(crate) blocks: B256Map<ExecutedBlock<N>>,
    /// A missing Finish bridge does not prevent historical execution-only reads.
    pub(crate) finish_error: Option<OverlayReadError>,
    /// An exact completed range may replace the unavailable bridge; retain its Arc, not a hit bit.
    pub(crate) ready_reverts: Option<(ChangesetRangeKey, Arc<TrieUpdatesSorted>)>,
}

impl<N: NodePrimitives> OverlayManager<N> {
    /// Lock source gates in stable address order, without waiting or retaining a partial lock set.
    pub(crate) fn capture<T>(
        &self,
        capture: impl FnOnce() -> ProviderResult<T>,
    ) -> ProviderResult<T> {
        self.capture_with_wait(false, capture)
    }

    fn capture_with_wait<T>(
        &self,
        wait_before_open: bool,
        capture: impl FnOnce() -> ProviderResult<T>,
    ) -> ProviderResult<T> {
        let (first, second) = match self.block_source.as_ref() {
            Some(source) if !Arc::ptr_eq(&self.blocks, source) => {
                if Arc::as_ptr(&self.blocks) < Arc::as_ptr(source) {
                    (&self.blocks, Some(source))
                } else {
                    (source, Some(&self.blocks))
                }
            }
            _ => (&self.blocks, None),
        };
        let _first =
            if wait_before_open { Some(first.capture.read()) } else { first.capture.try_read() }
                .ok_or_else(|| ProviderError::other(OverlayReadError::SourceBusy))?;
        let _second = second
            .map(|source| {
                if wait_before_open {
                    Some(source.capture.read())
                } else {
                    source.capture.try_read()
                }
                .ok_or_else(|| ProviderError::other(OverlayReadError::SourceBusy))
            })
            .transpose()?;
        capture()
    }

    fn capture_block(&self, hash: B256) -> Option<ExecutedBlock<N>> {
        self.read_context
            .as_ref()
            .and_then(|context| context.blocks.get(&hash).cloned())
            .or_else(|| self.blocks.get(&hash).map(|block| block.value().clone()))
            .or_else(|| self.block_source.as_ref()?.get(&hash).map(|block| block.value().clone()))
    }

    pub(crate) fn require_finish_bridge(&self) -> ProviderResult<()> {
        if let Some(error) =
            self.read_context.as_ref().and_then(|context| context.finish_error.as_ref())
        {
            return Err(ProviderError::other(error.clone()))
        }
        Ok(())
    }
}

impl<N: NodePrimitives> OverlayBuilder<N> {
    /// Opens a fresh database reader and captures both its target chain and Finish bridge.
    ///
    /// Source pruning is excluded only during reader creation, frontier reads and bounded block
    /// capture. Fresh acquisition waits for pruning before opening the reader; the gate wait has
    /// no fixed timeout. Writers must release database writers before pruning this source. No trie
    /// computation or cache waiting runs under the source gates. Missing Finish data is recorded:
    /// historical execution reads remain available, but cold trie reads fail without an exact
    /// pinned ready range. No reader is silently reopened.
    pub fn open_read_view<F>(&self, factory: &F) -> ProviderResult<OverlayReadView<F::Provider, N>>
    where
        F: DatabaseProviderFactory,
        F::Provider: StageCheckpointReader + BlockNumReader + PruneCheckpointReader,
    {
        let mut view = self.overlay_manager.capture_with_wait(true, || {
            let provider = factory.database_provider_ro()?;
            let (partial, finish) = database_state_frontiers(&provider)?;
            let builder = self.capture_at_frontiers(&provider, partial, finish)?;
            Ok(OverlayReadView { provider, builder })
        })?;
        view.builder.pin_ready_reverts(&view.provider)?;
        Ok(view)
    }

    /// Binds an already-open reader without changing its database snapshot.
    ///
    /// This never waits for a source gate or reopens the reader. Missing target data fails binding;
    /// a missing Finish bridge is recorded for trie reads, allowing historical execution reads.
    /// This cannot protect the interval before the call; factories should use
    /// [`Self::open_read_view`].
    pub fn bind_read_view<P>(&self, provider: &P) -> ProviderResult<Self>
    where
        P: StageCheckpointReader + BlockNumReader + PruneCheckpointReader,
    {
        let mut bound = self.overlay_manager.capture(|| {
            let (partial, finish) = database_state_frontiers(provider)?;
            self.capture_at_frontiers(provider, partial, finish)
        })?;
        bound.pin_ready_reverts(provider)?;
        Ok(bound)
    }

    /// Returns the durable frontiers of this bound builder, or `None` before capture.
    pub fn read_frontiers(&self) -> Option<(BlockNumHash, BlockNumHash)> {
        self.overlay_manager.read_context.as_ref().map(|context| (context.partial, context.finish))
    }

    pub(crate) fn capture_at_frontiers<P>(
        &self,
        provider: &P,
        partial: BlockNumHash,
        finish: BlockNumHash,
    ) -> ProviderResult<Self>
    where
        P: BlockNumReader + PruneCheckpointReader,
    {
        if partial.number > finish.number ||
            (partial.number == finish.number && partial.hash != finish.hash)
        {
            return Err(ProviderError::other(OverlayReadError::SnapshotMismatch))
        }
        let mut context = OverlayReadContext {
            partial,
            finish,
            blocks: B256Map::default(),
            finish_error: None,
            ready_reverts: None,
        };
        let parent_state = self.capture_chain(
            provider,
            self.parent_hash,
            self.parent_state.as_deref(),
            &mut context,
        )?;
        if partial != finish {
            match self.capture_chain(provider, finish.hash, None, &mut context) {
                Ok(finish_state) => {
                    if finish_state.as_ref().is_none_or(|state| state.anchor() != partial) {
                        return Err(ProviderError::other(OverlayReadError::DisconnectedChain {
                            hash: finish.hash,
                        }))
                    }
                }
                Err(error) => match error.downcast_other_ref::<OverlayReadError>() {
                    Some(missing @ OverlayReadError::MissingBlock { .. }) => {
                        context.finish_error = Some(missing.clone())
                    }
                    _ => return Err(error),
                },
            }
        }
        let mut bound = self.clone();
        bound.parent_state = parent_state;
        bound.overlay_manager.read_context = Some(Arc::new(context));
        // Validate the exact anchor and history availability even if the later cache lookup hits.
        if let AnchorForParent::RevertsRequired { anchor, .. } =
            bound.anchor_at_parent_with_frontiers(provider, partial, finish)?
        {
            bound.capture_forward_updates(anchor.number + 1)?;
        }
        Ok(bound)
    }

    /// Called under the source gates; optional forward updates can include persisted ancestors.
    pub(crate) fn capture_forward_updates(&mut self, start: u64) -> ProviderResult<()> {
        let Some(context) = &self.overlay_manager.read_context else { return Ok(()) };
        let mut hash = context.finish.hash;
        let mut number = context.finish.number;
        let mut extra = Vec::new();
        while number >= start {
            let Some(block) = self.overlay_manager.capture_block(hash) else { break };
            if block.recovered_block().number() != number {
                return Err(ProviderError::other(OverlayReadError::DisconnectedChain { hash }))
            }
            if !context.blocks.contains_key(&hash) {
                if context.blocks.len() + extra.len() == MAX_CAPTURE_BLOCKS {
                    return Err(ProviderError::other(OverlayReadError::CaptureLimit))
                }
                extra.push((hash, block.clone()));
            }
            hash = block.recovered_block().parent_hash();
            let Some(parent_number) = number.checked_sub(1) else { break };
            number = parent_number;
        }
        if let Some(context) = &mut self.overlay_manager.read_context {
            Arc::make_mut(context).blocks.extend(extra);
        }
        Ok(())
    }

    /// Cache locks and any ready-range merge are outside the source capture gates.
    pub(crate) fn pin_ready_reverts<P>(&mut self, provider: &P) -> ProviderResult<()>
    where
        P: BlockNumReader + PruneCheckpointReader,
    {
        if self
            .overlay_manager
            .read_context
            .as_ref()
            .is_none_or(|context| context.finish_error.is_none())
        {
            return Ok(())
        }
        let Some((partial, finish)) = self.read_frontiers() else { return Ok(()) };
        let AnchorForParent::RevertsRequired { anchor, .. } =
            self.anchor_at_parent_with_frontiers(provider, partial, finish)?
        else {
            return Ok(())
        };
        let key = ChangesetRangeKey::new(anchor.number + 1, finish.number, finish.hash);
        let ready = self.overlay_manager.changeset_cache().ready_range(provider, key)?;
        if let Some(context) = &mut self.overlay_manager.read_context {
            Arc::make_mut(context).ready_reverts = ready.map(|updates| (key, updates));
        }
        Ok(())
    }

    fn capture_chain<P>(
        &self,
        provider: &P,
        tip: B256,
        existing: Option<&BlockState<N>>,
        context: &mut OverlayReadContext<N>,
    ) -> ProviderResult<Option<Arc<BlockState<N>>>>
    where
        P: BlockNumReader,
    {
        let mut hash = tip;
        let mut expected_number = None;
        let mut chain = Vec::new();
        let mut existing = existing.into_iter().flat_map(BlockState::chain).peekable();
        loop {
            if hash == context.partial.hash {
                if expected_number.is_some_and(|number| number != context.partial.number) {
                    return Err(ProviderError::other(OverlayReadError::DisconnectedChain { hash }))
                }
                break
            }
            let block = if existing.peek().is_some_and(|state| state.hash() == hash) {
                existing.next().map(BlockState::block)
            } else {
                context
                    .blocks
                    .get(&hash)
                    .cloned()
                    .or_else(|| self.overlay_manager.capture_block(hash))
            };
            let number = match &block {
                Some(block) => Some(block.recovered_block().number()),
                None => provider.block_number(hash)?,
            };
            if let Some(number) = number {
                if expected_number.is_some_and(|expected| number != expected) {
                    return Err(ProviderError::other(OverlayReadError::DisconnectedChain { hash }))
                }
                if number <= context.partial.number && provider.block_hash(number)? == Some(hash) {
                    break
                }
            }
            if block.is_none() &&
                (expected_number.is_some_and(|number| number <= context.partial.number) ||
                    (hash == tip && number.is_none()))
            {
                return Err(ProviderError::BlockHashNotFound(hash))
            }
            let block = block.ok_or_else(|| {
                ProviderError::other(OverlayReadError::MissingBlock {
                    hash,
                    partial: context.partial,
                    finish: context.finish,
                })
            })?;
            expected_number = block.recovered_block().number().checked_sub(1);
            if expected_number.is_none() {
                return Err(ProviderError::other(OverlayReadError::DisconnectedChain { hash }))
            }
            if !context.blocks.contains_key(&hash) {
                if context.blocks.len() == MAX_CAPTURE_BLOCKS {
                    return Err(ProviderError::other(OverlayReadError::CaptureLimit))
                }
                context.blocks.insert(hash, block.clone());
            }
            hash = block.recovered_block().parent_hash();
            chain.push(block);
        }
        Ok(chain
            .into_iter()
            .rev()
            .fold(None, |parent, block| Some(Arc::new(BlockState::with_parent(block, parent)))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use reth_ethereum_primitives::Block;
    use reth_primitives_traits::RecoveredBlock;
    use reth_provider::test_utils::create_test_provider_factory;
    use std::{sync::mpsc, time::Duration};

    fn empty_block(number: u64, parent_hash: B256, branch: u64) -> ExecutedBlock<EthPrimitives> {
        ExecutedBlock::new(
            Arc::new(RecoveredBlock::new_unhashed(
                Block {
                    header: Header { number, parent_hash, timestamp: branch, ..Default::default() },
                    body: Default::default(),
                },
                Vec::new(),
            )),
            Arc::default(),
            Default::default(),
        )
    }

    #[test]
    fn source_gates_wait_before_open_and_release_on_existing_reader_error() {
        let factory = create_test_provider_factory();
        let source = OverlayManager::<EthPrimitives>::default();
        let manager = OverlayManager::default().with_block_source(&source);
        let (first, second) = if Arc::as_ptr(&manager.blocks) < Arc::as_ptr(&source.blocks) {
            (&manager.blocks, &source.blocks)
        } else {
            (&source.blocks, &manager.blocks)
        };
        let write = second.capture.write();
        let existing = factory.provider().unwrap();
        let builder = manager.overlay_builder(B256::ZERO);
        let error = builder.bind_read_view(&existing).unwrap_err();
        assert_eq!(
            error.downcast_other_ref::<OverlayReadError>(),
            Some(&OverlayReadError::SourceBusy)
        );
        assert!(first.capture.try_write().is_some(), "failed second gate must release the first");
        drop(existing);

        let (started, starting) = mpsc::channel();
        let (opened, opening) = mpsc::channel();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                started.send(()).unwrap();
                manager.capture_with_wait(true, || {
                    let _reader = factory.provider()?;
                    assert!(first.capture.try_write().is_none());
                    assert!(second.capture.try_write().is_none());
                    opened.send(()).unwrap();
                    Err::<(), _>(ProviderError::BestBlockNotFound)
                })
            });
            let began = starting.recv_timeout(Duration::from_secs(1));
            let blocked = opening.recv_timeout(Duration::from_millis(50));
            drop(write);
            began.unwrap();
            assert_eq!(blocked, Err(mpsc::RecvTimeoutError::Timeout));
            opening.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(matches!(task.join().unwrap(), Err(ProviderError::BestBlockNotFound)));
        });
        assert!(first.capture.try_write().is_some());
        assert!(second.capture.try_write().is_some());
        // Duplicate and reversed sources use the same gate set without recursive locking.
        manager.clone().with_block_source(&manager).capture(|| Ok(())).unwrap();
        source.clone().with_block_source(&manager).capture(|| Ok(())).unwrap();
    }

    #[test]
    fn capture_budget_counts_both_branches_and_shared_blocks_once() {
        let factory = create_test_provider_factory();
        let provider = factory.provider().unwrap();
        let manager = OverlayManager::default();
        let partial = BlockNumHash::new(0, B256::with_last_byte(1));
        let mut tips = Vec::new();
        for branch in 1..=2 {
            let mut parent = partial.hash;
            for number in 1..=(MAX_CAPTURE_BLOCKS / 2) as u64 {
                let block = empty_block(number, parent, branch);
                parent = block.recovered_block().hash();
                manager.insert_block(block);
            }
            tips.push(parent);
        }
        let finish = BlockNumHash::new((MAX_CAPTURE_BLOCKS / 2) as u64, tips[1]);
        let mut context = OverlayReadContext {
            partial,
            finish,
            blocks: B256Map::default(),
            finish_error: None,
            ready_reverts: None,
        };
        let builder = manager.overlay_builder(tips[0]);
        builder.capture_chain(&provider, tips[0], None, &mut context).unwrap();
        builder.capture_chain(&provider, tips[0], None, &mut context).unwrap();
        assert_eq!(context.blocks.len(), MAX_CAPTURE_BLOCKS / 2);
        builder.capture_chain(&provider, tips[1], None, &mut context).unwrap();
        assert_eq!(context.blocks.len(), MAX_CAPTURE_BLOCKS);
        let extra = empty_block(finish.number + 1, tips[1], 2);
        let hash = extra.recovered_block().hash();
        manager.insert_block(extra);
        let error = builder.capture_chain(&provider, hash, None, &mut context).unwrap_err();
        assert_eq!(
            error.downcast_other_ref::<OverlayReadError>(),
            Some(&OverlayReadError::CaptureLimit)
        );
    }

    #[test]
    fn capture_rejects_disconnected_and_underflowing_heights() {
        let factory = create_test_provider_factory();
        let provider = factory.provider().unwrap();
        let manager = OverlayManager::default();
        let partial = BlockNumHash::new(1, B256::with_last_byte(1));
        for number in [0, 3] {
            let block = empty_block(number, partial.hash, 1);
            let hash = block.recovered_block().hash();
            manager.insert_block(block);
            let mut context = OverlayReadContext {
                partial,
                finish: partial,
                blocks: B256Map::default(),
                finish_error: None,
                ready_reverts: None,
            };
            let error = manager
                .overlay_builder(hash)
                .capture_chain(&provider, hash, None, &mut context)
                .unwrap_err();
            assert_eq!(
                error.downcast_other_ref::<OverlayReadError>(),
                Some(&OverlayReadError::DisconnectedChain {
                    hash: if number == 0 { hash } else { partial.hash },
                })
            );
        }
    }
}
