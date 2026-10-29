use super::*;
use crate::{OverlayStateProvider, StateTrieOverlay};
use alloy_consensus::Header;
use alloy_primitives::{keccak256, Address, U256};
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_provider::{
    test_utils::{create_test_provider_factory, MockNodeTypesWithDB},
    LatestStateProvider, ProviderFactory,
};
use reth_storage_api::{StateProofProvider, StateRootProvider, StateWriter, TrieWriter};
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
            match account {
                Some(account) => {
                    complete.accounts.insert(address, Some(account));
                }
                None => {
                    complete.accounts.remove(&address);
                }
            }
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

        // This oracle builds tries from complete live leaves, without overlay merging or cached
        // intermediate nodes. It therefore cannot inherit an overlay merge ordering mistake.
        let root = state_root_unsorted(complete.accounts.iter().map(|(address, account)| {
            let storage_root = storage_root_unsorted(
                complete.storages[address].storage.iter().map(|(key, value)| (*key, *value)),
            );
            (*address, account.unwrap().into_trie_account(storage_root))
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
            accounts: state.accounts.iter().copied().collect(),
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
