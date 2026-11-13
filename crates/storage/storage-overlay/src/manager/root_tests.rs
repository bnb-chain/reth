use super::*;
use crate::{
    OverlayReadError, OverlayStateProvider, OverlayStateProviderFactory, StateTrieOverlay,
};
use alloy_consensus::Header;
use alloy_primitives::{keccak256, Address, U256};
use reth_db::{
    models::{AccountBeforeTx, BlockNumberAddress},
    tables,
    transaction::DbTxMut,
};
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_primitives_traits::{Account, RecoveredBlock, StorageEntry};
use reth_provider::{
    test_utils::{create_test_provider_factory, MockNodeTypesWithDB},
    BlockWriter, LatestStateProvider, ProviderFactory,
};
use reth_stages_types::{FinishCheckpoint, StageCheckpoint, StageId};
use reth_storage_api::{
    DatabaseProviderROFactory, StageCheckpointWriter, StateProofProvider, StateRootProvider,
    StateWriter, TrieWriter,
};
use reth_trie::{
    root::{state_root_unsorted, storage_root_unsorted},
    DecodedMultiProofV2, HashedPostState, HashedStorage, MultiProofTargetsV2, SortedTrieData,
};

struct RootFixture {
    blocks: Vec<ExecutedBlock<EthPrimitives>>,
    roots: Vec<B256>,
    proofs: Vec<DecodedMultiProofV2>,
}

fn targets() -> MultiProofTargetsV2 {
    // Include an account and a slot that never exist, as well as deleted and recreated entries.
    MultiProofTargetsV2 {
        account_targets: (0..5).map(|id| keccak256(Address::with_last_byte(id)).into()).collect(),
        storage_targets: (0..5)
            .map(|id| {
                (
                    keccak256(Address::with_last_byte(id)),
                    (0..9).map(|slot| keccak256(B256::with_last_byte(slot)).into()).collect(),
                )
            })
            .collect(),
    }
}

fn persist(factory: &ProviderFactory<MockNodeTypesWithDB>, block: &ExecutedBlock<EthPrimitives>) {
    let writer = factory.provider_rw().unwrap();
    writer.write_hashed_state(&block.trie_data().hashed_state).unwrap();
    writer.write_trie_updates_sorted(&block.trie_data().trie_updates).unwrap();
    writer.commit().unwrap();
}

fn fixture(branch: u64) -> RootFixture {
    let factory = create_test_provider_factory();
    let mut fixture = RootFixture { blocks: Vec::new(), roots: Vec::new(), proofs: Vec::new() };
    let mut complete = HashedPostState::default();
    let mut parent_hash = B256::ZERO;
    for number in 0..12 {
        let mut delta = HashedPostState::default();
        for id in 0..4 {
            if number > 0 && id >= 2 && number % 3 != 0 {
                continue
            }
            let address = keccak256(Address::with_last_byte(id));
            // The two histories share blocks 0..=4 and diverge after the recreation.
            let nonce = number + 1 + if number > 4 { branch * 100 } else { 0 };
            let deleted = (id == 0 && number == 3) || (id == 1 && number == 7);
            let account = (!deleted).then_some(Account { nonce, ..Default::default() });
            delta.accounts.insert(address, account);
            let mut storage = HashedStorage::default();
            for slot in 0..8 {
                let recreated = (id == 0 && number == 4) || (id == 1 && number == 8);
                let clearing = deleted || number == 5;
                let changed = number == 0 ||
                    clearing ||
                    if recreated {
                        slot < 2
                    } else {
                        (number == 2 && slot % 2 == 0) || (u64::from(slot) + number) % 3 == 0
                    };
                if !changed {
                    continue
                }
                let key = keccak256(B256::with_last_byte(slot));
                // Clear a full storage trie, delete alternate slots, and write them again.
                let value = if clearing || (number == 2 && slot % 2 == 0) {
                    U256::ZERO
                } else {
                    U256::from(nonce * 100 + u64::from(slot) + 1)
                };
                storage.storage.insert(key, value);
                let live = complete.storages.entry(address).or_default();
                if value.is_zero() {
                    live.storage.remove(&key);
                } else {
                    live.storage.insert(key, value);
                }
            }
            delta.storages.insert(address, storage);
        }
        for (address, account) in delta.accounts.clone() {
            match account {
                Some(account) => {
                    complete.accounts.insert(address, Some(account));
                }
                None => {
                    complete.accounts.remove(&address);
                }
            }
        }

        // This oracle builds tries from complete live leaves, without overlay merging or cached
        // intermediate nodes. It therefore cannot inherit an overlay merge ordering mistake.
        let root =
            state_root_unsorted(complete.accounts.clone().into_iter().map(|(address, account)| {
                let storage_root = storage_root_unsorted(
                    complete.storages[&address].storage.iter().map(|(key, value)| (*key, *value)),
                );
                (address, account.unwrap().into_trie_account(storage_root))
            }));
        let (incremental_root, updates) = LatestStateProvider::new(factory.provider().unwrap())
            .state_root_with_updates(delta.clone())
            .unwrap();
        assert_eq!(incremental_root, root, "fixture block {number}");
        let block = ExecutedBlock::new(
            Arc::new(RecoveredBlock::new_unhashed(
                Block {
                    header: Header { number, parent_hash, state_root: root, ..Default::default() },
                    body: Default::default(),
                },
                Vec::new(),
            )),
            Arc::new(Default::default()),
            SortedTrieData::new(Arc::new(delta.into_sorted()), Arc::new(updates.into_sorted())),
        );
        persist(&factory, &block);
        let proof = LatestStateProvider::new(factory.provider().unwrap())
            .multiproof_v2(targets(), true)
            .unwrap();
        verify_proofs(&proof, root);
        parent_hash = block.recovered_block().hash();
        fixture.blocks.push(block);
        fixture.roots.push(root);
        fixture.proofs.push(proof);
    }
    fixture
}

fn verify_proofs(proof: &DecodedMultiProofV2, root: B256) {
    let slots = (0..9).map(B256::with_last_byte).collect::<Vec<_>>();
    for id in 0..5 {
        proof.account_proof(Address::with_last_byte(id), &slots).unwrap().verify(root).unwrap();
    }
}

fn assert_overlay(
    factory: &ProviderFactory<MockNodeTypesWithDB>,
    manager: &OverlayManager,
    fixture: &RootFixture,
    tip: usize,
    input: TrieInputSorted,
) {
    let provider = OverlayStateProvider::new(
        factory.provider().unwrap(),
        manager.overlay_builder(fixture.blocks[tip].recovered_block().hash()),
    )
    .with_state_trie_overlay(StateTrieOverlay::new(input));
    assert_eq!(provider.state_root(HashedPostState::default()).unwrap(), fixture.roots[tip]);
    // Reuse the same provider's calculator and switch mask modes in both directions.
    for masks in [true, true, false, true] {
        let proof = provider.multiproof_v2(targets(), masks).unwrap();
        verify_proofs(&proof, fixture.roots[tip]);
        assert_eq!(proof, fixture.proofs[tip]);
    }
    if let Some(child) = fixture.blocks.get(tip + 1) {
        let state = &child.trie_data().hashed_state;
        let delta = HashedPostState {
            accounts: state.accounts.iter().map(Clone::clone).collect(),
            storages: state
                .storages
                .iter()
                .map(|(address, storage)| {
                    (*address, HashedStorage::from_iter(storage.storage_slots.iter().copied()))
                })
                .collect(),
        };
        assert_eq!(provider.state_root(delta.clone()).unwrap(), fixture.roots[tip + 1]);
        assert_eq!(provider.state_root_with_updates(delta).unwrap().0, fixture.roots[tip + 1]);
    }
}

#[test]
fn cached_ancestors_preserve_roots_and_proofs_across_deletions_and_forks() {
    let canonical = fixture(0);
    let fork = fixture(1);
    assert_eq!(canonical.roots[4], fork.roots[4]);
    assert_ne!(canonical.roots[5], fork.roots[5]);
    for write_to_cache in [false, true] {
        for anchor in [0, 2, 4, 6] {
            let factory = create_test_provider_factory();
            for block in &canonical.blocks[..=anchor] {
                persist(&factory, block);
            }
            for cached in anchor + 1..10 {
                for tip in cached + 2..12 {
                    let manager = OverlayManager::default();
                    for block in canonical.blocks.iter().chain(&fork.blocks) {
                        manager.insert_block(block.clone());
                    }
                    let anchor_hash = canonical.blocks[anchor].recovered_block().hash();
                    let cached_hash = canonical.blocks[cached].recovered_block().hash();
                    let key = OverlayCacheKey { anchor_hash, tip_hash: cached_hash };
                    let (nodes, state) =
                        super::tests::overlay_for_parent(&manager, cached_hash, anchor_hash)
                            .unwrap();
                    let held = TrieInputSorted::new(nodes, state, Default::default());
                    // A ready overlay at the same height on another fork must never be selected.
                    if cached > 4 && anchor <= 4 {
                        manager.state_trie_overlays.entries.insert(
                            OverlayCacheKey {
                                anchor_hash,
                                tip_hash: fork.blocks[cached].recovered_block().hash(),
                            },
                            OverlayCacheEntry::Ready(Arc::new(merge_blocks(
                                fork.blocks[anchor + 1..=cached].iter().rev().cloned().collect(),
                            ))),
                        );
                    }
                    let parent_state = manager
                        .block_state(canonical.blocks[tip].recovered_block().hash())
                        .unwrap();
                    let (nodes, state) = manager
                        .overlay_for_parent(
                            &parent_state,
                            anchor_hash,
                            OverlayCacheConfig { precompute: false, write_to_cache },
                        )
                        .unwrap();
                    assert_eq!(
                        manager.state_trie_overlays.entries.contains_key(&key),
                        !write_to_cache
                    );
                    let cold = merge_blocks(
                        canonical.blocks[anchor + 1..=tip].iter().rev().cloned().collect(),
                    );
                    assert_eq!(nodes, cold.nodes);
                    assert_eq!(state, cold.state);
                    assert_overlay(&factory, &manager, &canonical, tip, cold);
                    assert_overlay(
                        &factory,
                        &manager,
                        &canonical,
                        tip,
                        TrieInputSorted::new(nodes, state, Default::default()),
                    );
                    assert_overlay(&factory, &manager, &canonical, cached, held);
                }
            }
        }
    }
}

#[test]
fn persisted_anchor_change_does_not_reuse_old_overlay() {
    let fixture = fixture(0);
    let factory = create_test_provider_factory();
    let manager = OverlayManager::default();
    for block in &fixture.blocks {
        manager.insert_block(block.clone());
    }
    persist(&factory, &fixture.blocks[0]);
    let old_anchor = fixture.blocks[0].recovered_block().hash();
    let cached_hash = fixture.blocks[8].recovered_block().hash();
    super::tests::overlay_for_parent(&manager, cached_hash, old_anchor).unwrap();
    for block in &fixture.blocks[1..=5] {
        persist(&factory, block);
    }
    let anchor = fixture.blocks[5].recovered_block().hash();
    let tip = fixture.blocks[11].recovered_block().hash();
    let (nodes, state) = super::tests::overlay_for_parent(&manager, tip, anchor).unwrap();
    assert!(manager
        .state_trie_overlays
        .entries
        .contains_key(&OverlayCacheKey { anchor_hash: old_anchor, tip_hash: cached_hash }));
    assert_overlay(
        &factory,
        &manager,
        &fixture,
        11,
        TrieInputSorted::new(nodes, state, Default::default()),
    );
}

fn masked_finish_fixture() -> (ProviderFactory<MockNodeTypesWithDB>, RootFixture) {
    let durable = fixture(0);
    let factory = create_test_provider_factory();
    let partial = durable.blocks[4].recovered_block().num_hash();
    let finish = durable.blocks[7].recovered_block().num_hash();
    for block in &durable.blocks[..=4] {
        persist(&factory, block);
    }

    // Non-state data is at Finish, but state/trie rows still describe block 4.
    let writer = factory.provider_rw().unwrap();
    let mut complete = HashedPostState::default();
    for (number, block) in durable.blocks[..=7].iter().enumerate() {
        writer.insert_block(block.recovered_block()).unwrap();
        let delta = block.trie_data();
        if number > 4 {
            for (address_hash, _) in &delta.hashed_state.accounts {
                let address = (0..4)
                    .map(Address::with_last_byte)
                    .find(|address| keccak256(address) == *address_hash)
                    .unwrap();
                writer
                    .tx_ref()
                    .put::<tables::AccountChangeSets>(
                        block.block_number(),
                        AccountBeforeTx {
                            address,
                            info: complete.accounts.get(address_hash).cloned().flatten(),
                        },
                    )
                    .unwrap();
            }
            for (address_hash, storage) in &delta.hashed_state.storages {
                let address = (0..4)
                    .map(Address::with_last_byte)
                    .find(|address| keccak256(address) == *address_hash)
                    .unwrap();
                for (slot_hash, _) in &storage.storage_slots {
                    let slot = (0..8)
                        .map(B256::with_last_byte)
                        .find(|slot| keccak256(slot) == *slot_hash)
                        .unwrap();
                    let value = complete
                        .storages
                        .get(address_hash)
                        .and_then(|storage| storage.storage.get(slot_hash))
                        .copied()
                        .unwrap_or_default();
                    writer
                        .tx_ref()
                        .put::<tables::StorageChangeSets>(
                            BlockNumberAddress((block.block_number(), address)),
                            StorageEntry { key: slot, value },
                        )
                        .unwrap();
                }
            }
        }
        complete.extend_from_sorted(&delta.hashed_state);
    }
    writer
        .save_stage_checkpoint(
            StageId::Finish,
            StageCheckpoint::new(finish.number).with_finish_stage_checkpoint(FinishCheckpoint {
                partial_state_trie: Some(partial.number),
            }),
        )
        .unwrap();
    writer.commit().unwrap();

    (factory, durable)
}

#[test]
fn block_source_completes_masked_finish_for_fork_and_older_parent() {
    let (factory, durable) = masked_finish_fixture();
    let fork = fixture(1);
    let partial = durable.blocks[4].recovered_block().num_hash();
    let finish = durable.blocks[7].recovered_block().num_hash();

    // The second case needs Finish even without a fork: persistence advanced beyond the parent.
    for (chain, tip) in [(&fork, 9), (&durable, 6)] {
        let source = OverlayManager::default();
        for block in &durable.blocks[5..=7] {
            source.insert_block(block.clone());
        }
        let local = OverlayManager::default();
        for block in &chain.blocks[5..=tip] {
            local.insert_block(block.clone());
        }
        let parent = chain.blocks[tip].recovered_block().hash();
        let provider = factory.provider().unwrap();
        let error = local
            .overlay_builder(parent)
            .build_state_trie_overlay_at_frontiers(&provider, partial, finish, true)
            .unwrap_err();
        assert_eq!(
            error.downcast_other_ref::<OverlayReadError>(),
            Some(&OverlayReadError::MissingBlock { hash: finish.hash, partial, finish })
        );

        let manager = local.with_block_source(&source);
        let overlay = manager
            .overlay_builder(parent)
            .build_state_trie_overlay_at_frontiers(&provider, partial, finish, true)
            .unwrap();
        assert!(source.state_trie_overlays.entries.is_empty());
        assert!(source.execution_overlays.entries.is_empty());
        source.remove_blocks(
            durable.blocks[5..=7].iter().map(|block| block.recovered_block().hash()),
        );
        assert_overlay(&factory, &manager, chain, tip, overlay.input().clone());
    }
}

#[test]
fn old_snapshot_reports_missing_finish_if_source_is_pruned_before_capture() {
    let (factory, durable) = masked_finish_fixture();
    let fork = fixture(1);
    let source = OverlayManager::default();
    for block in &durable.blocks[5..=7] {
        source.insert_block(block.clone());
    }
    let local = OverlayManager::default();
    for block in &fork.blocks[5..=9] {
        local.insert_block(block.clone());
    }
    let manager = local.with_block_source(&source);
    let provider = factory.provider().unwrap();
    let (partial, finish) = database_state_frontiers(&provider).unwrap();
    assert_eq!(partial, durable.blocks[4].recovered_block().num_hash());
    assert_eq!(finish, durable.blocks[7].recovered_block().num_hash());

    // Advance the durable state/trie frontier while the reader keeps its older DB snapshot.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                for block in &durable.blocks[5..=7] {
                    persist(&factory, block);
                }
                let writer = factory.provider_rw().unwrap();
                writer
                    .save_stage_checkpoint(
                        StageId::Finish,
                        StageCheckpoint::new(finish.number).with_finish_stage_checkpoint(
                            FinishCheckpoint { partial_state_trie: Some(finish.number) },
                        ),
                    )
                    .unwrap();
                writer.commit().unwrap();
                source.remove_blocks(
                    durable.blocks[5..=7].iter().map(|block| block.recovered_block().hash()),
                );
            })
            .join()
            .unwrap();
    });

    assert_eq!(database_state_frontiers(&provider).unwrap(), (partial, finish));
    assert_eq!(database_state_frontiers(&factory.provider().unwrap()).unwrap(), (finish, finish));
    let error = manager
        .overlay_builder(fork.blocks[9].recovered_block().hash())
        .build_state_trie_overlay_at_frontiers(&provider, partial, finish, true)
        .unwrap_err();
    assert_eq!(
        error.downcast_other_ref::<OverlayReadError>(),
        Some(&OverlayReadError::MissingBlock { hash: finish.hash, partial, finish })
    );
}

#[test]
fn source_prune_between_parent_lookups_truncates_capture_but_keeps_captured_chain_alive() {
    let (factory, durable) = masked_finish_fixture();
    let source = OverlayManager::default();
    for block in &durable.blocks[5..=7] {
        source.insert_block(block.clone());
    }
    let manager = OverlayManager::default().with_block_source(&source);
    let finish = durable.blocks[7].recovered_block().num_hash();
    let partial = durable.blocks[4].recovered_block().num_hash();
    let complete = Arc::new(manager.block_state(finish.hash).unwrap());
    let mut capture = manager.parent_chain(finish.hash);
    let first = capture.next().unwrap();
    assert_eq!(first.recovered_block().num_hash(), finish);

    // Force pruning strictly between two lookups; no sleep or scheduler timing is required.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                source.remove_blocks(
                    durable.blocks[5..=7].iter().map(|block| block.recovered_block().hash()),
                );
            })
            .join()
            .unwrap();
    });
    assert!(capture.next().is_none());
    assert_eq!(complete.chain().count(), 3);

    let provider = factory.provider().unwrap();
    let truncated = Arc::new(BlockState::new(first));
    let error = manager
        .overlay_builder_for_state(truncated)
        .build_state_trie_overlay_at_frontiers(&provider, partial, finish, true)
        .unwrap_err();
    let missing_anchor = durable.blocks[6].recovered_block().num_hash();
    assert_eq!(
        error.downcast_other_ref::<OverlayReadError>(),
        Some(&OverlayReadError::MissingBlock { hash: missing_anchor.hash, partial, finish })
    );

    let builder = manager.overlay_builder_for_state(complete);
    let overlay =
        builder.build_state_trie_overlay_at_frontiers(&provider, partial, finish, true).unwrap();
    let state_provider =
        OverlayStateProvider::new(provider, builder).with_state_trie_overlay(overlay);
    assert_eq!(state_provider.state_root(HashedPostState::default()).unwrap(), durable.roots[7]);
    let proof = state_provider.multiproof_v2(targets(), true).unwrap();
    verify_proofs(&proof, durable.roots[7]);
    assert_eq!(proof, durable.proofs[7]);
}

#[test]
fn factory_captures_finish_bridge_before_pruning_and_keeps_old_snapshot_readable() {
    for (branch, tip) in [(1, 9), (0, 6)] {
        let (factory, durable) = masked_finish_fixture();
        let target = fixture(branch);
        let source = OverlayManager::default();
        for block in &durable.blocks[5..=7] {
            source.insert_block(block.clone());
        }
        let local = OverlayManager::default();
        for block in &target.blocks[5..=tip] {
            local.insert_block(block.clone());
        }
        let manager = local.with_block_source(&source);
        let provider_factory = OverlayStateProviderFactory::new(
            factory.clone(),
            manager.overlay_builder(target.blocks[tip].recovered_block().hash()),
        );
        // No root/proof or lazy overlay has been computed yet.
        let provider = provider_factory.database_provider_ro().unwrap();
        let borrowed_reader = factory.provider().unwrap();
        let bound = manager
            .overlay_builder(target.blocks[tip].recovered_block().hash())
            .bind_read_view(&borrowed_reader)
            .unwrap();
        let borrowed_provider = OverlayStateProvider::new_ref(&borrowed_reader, bound);
        let finish = durable.blocks[7].recovered_block().num_hash();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    for block in &durable.blocks[5..=7] {
                        persist(&factory, block);
                    }
                    let writer = factory.provider_rw().unwrap();
                    writer
                        .save_stage_checkpoint(
                            StageId::Finish,
                            StageCheckpoint::new(finish.number).with_finish_stage_checkpoint(
                                FinishCheckpoint { partial_state_trie: Some(finish.number) },
                            ),
                        )
                        .unwrap();
                    writer.commit().unwrap();
                    source.remove_blocks(
                        durable.blocks[5..=7].iter().map(|block| block.recovered_block().hash()),
                    );
                    manager.remove_blocks(
                        target.blocks[5..=tip].iter().map(|block| block.recovered_block().hash()),
                    );
                })
                .join()
                .unwrap();
        });
        assert!(source.blocks.is_empty());
        assert!(manager.blocks.is_empty());
        assert_eq!(
            database_state_frontiers(&factory.provider().unwrap()).unwrap(),
            (finish, finish)
        );
        assert_eq!(provider.state_root(HashedPostState::default()).unwrap(), target.roots[tip]);
        assert_eq!(
            borrowed_provider.state_root(HashedPostState::default()).unwrap(),
            target.roots[tip]
        );
        for masks in [true, false, true] {
            let proof = provider.multiproof_v2(targets(), masks).unwrap();
            verify_proofs(&proof, target.roots[tip]);
            assert_eq!(proof, target.proofs[tip]);
        }
    }
}

#[test]
fn missing_finish_uses_pinned_ready_reverts_after_cache_eviction() {
    for partial_index in [4, 6] {
        let (factory, durable) = masked_finish_fixture();
        let target = fixture(1);
        let finish = durable.blocks[7].recovered_block().num_hash();
        for block in durable.blocks.iter().take(partial_index + 1).skip(5) {
            persist(&factory, block);
        }
        let writer = factory.provider_rw().unwrap();
        writer
            .save_stage_checkpoint(
                StageId::Finish,
                StageCheckpoint::new(finish.number).with_finish_stage_checkpoint(
                    FinishCheckpoint { partial_state_trie: Some(partial_index as u64) },
                ),
            )
            .unwrap();
        writer.commit().unwrap();
        let source = OverlayManager::default();
        for block in &durable.blocks[5..=7] {
            source.insert_block(block.clone());
        }
        let manager = OverlayManager::default().with_block_source(&source);
        for block in &target.blocks[5..=9] {
            manager.insert_block(block.clone());
        }
        let builder = manager.overlay_builder(target.blocks[9].recovered_block().hash());
        let reader = factory.provider().unwrap();
        let ready = manager.get_or_compute_cached_changesets_range(&reader, 5..=7).unwrap();
        let references = Arc::strong_count(&ready);
        let complete = builder.open_read_view(&factory).unwrap();
        assert!(complete
            .builder
            .overlay_manager
            .read_context
            .as_ref()
            .unwrap()
            .ready_reverts
            .is_none());
        assert_eq!(
            Arc::strong_count(&ready),
            references,
            "complete capture must not eagerly pin or merge ready reverts"
        );
        drop(complete);
        source.remove_blocks(
            durable.blocks[5..=7].iter().map(|block| block.recovered_block().hash()),
        );

        let view = builder.open_read_view(&factory).unwrap();
        let context = view.builder.overlay_manager.read_context.as_ref().unwrap();
        assert!(matches!(context.finish_error, Some(OverlayReadError::MissingBlock { .. })));
        assert!(Arc::ptr_eq(&context.ready_reverts.as_ref().unwrap().1, &ready));
        manager.evict_cached_changesets(u64::MAX);
        manager
            .remove_blocks(target.blocks[5..=9].iter().map(|block| block.recovered_block().hash()));
        let provider = OverlayStateProvider::new(view.provider, view.builder);
        assert_eq!(provider.state_root(HashedPostState::default()).unwrap(), target.roots[9]);
        for masks in [false, true, false] {
            let proof = provider.multiproof_v2(targets(), masks).unwrap();
            verify_proofs(&proof, target.roots[9]);
            assert_eq!(proof, target.proofs[9]);
        }
    }
}

#[test]
fn bound_builder_rejects_other_snapshot_even_with_old_arguments_and_ready_overlay() {
    let (factory, durable) = masked_finish_fixture();
    let manager = OverlayManager::default();
    for block in &durable.blocks[5..=9] {
        manager.insert_block(block.clone());
    }
    let view = manager
        .overlay_builder(durable.blocks[9].recovered_block().hash())
        .open_read_view(&factory)
        .unwrap();
    let (partial, finish) = view.builder.read_frontiers().unwrap();
    let ready = view
        .builder
        .build_state_trie_overlay_at_frontiers(&view.provider, partial, finish, true)
        .unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                for block in &durable.blocks[5..=7] {
                    persist(&factory, block);
                }
                let writer = factory.provider_rw().unwrap();
                writer
                    .save_stage_checkpoint(StageId::Finish, StageCheckpoint::new(finish.number))
                    .unwrap();
                writer.commit().unwrap();
            })
            .join()
            .unwrap();
    });
    let fresh = factory.provider().unwrap();
    let error = view
        .builder
        .build_state_trie_overlay_at_frontiers(&fresh, partial, finish, true)
        .unwrap_err();
    assert_eq!(
        error.downcast_other_ref::<OverlayReadError>(),
        Some(&OverlayReadError::SnapshotMismatch)
    );
    let mismatched =
        OverlayStateProvider::new(fresh, view.builder.clone()).with_state_trie_overlay(ready);
    let error = mismatched.state_root(HashedPostState::default()).unwrap_err();
    assert_eq!(
        error.downcast_other_ref::<OverlayReadError>(),
        Some(&OverlayReadError::SnapshotMismatch)
    );

    // Fresh factory acquisition from an already bound builder deliberately captures a new view.
    let fresh = view.builder.open_read_view(&factory).unwrap();
    assert_eq!(fresh.builder.read_frontiers(), Some((finish, finish)));
    let provider = OverlayStateProvider::new(fresh.provider, fresh.builder);
    assert_eq!(provider.state_root(HashedPostState::default()).unwrap(), durable.roots[9]);
}
