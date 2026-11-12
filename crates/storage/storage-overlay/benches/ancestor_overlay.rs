//! Compare overlay assembly with and without a ready ancestor, excluding cache seeding.

use alloy_eips::BlockNumHash;
use alloy_primitives::{keccak256, B256, U256};
use reth_chain_state::{test_utils::TestBlockBuilder, ExecutedBlock};
use reth_ethereum_primitives::EthPrimitives;
use reth_primitives_traits::{Account, AlloyBlockHeader};
use reth_provider::test_utils::create_test_provider_factory;
use reth_storage_overlay::OverlayManager;
use reth_trie::{
    updates::TrieUpdatesSorted, BranchNodeCompact, HashedPostState, HashedPostStateSorted,
    HashedStorage, Nibbles, SortedTrieData,
};
use std::{hint::black_box, sync::Arc, time::Instant};

const BLOCKS: u64 = 100;
const KEYS_PER_BLOCK: u64 = 2_000;
const REPETITIONS: usize = 5;

fn main() {
    let blocks = TestBlockBuilder::eth()
        .get_executed_blocks(1..BLOCKS + 1)
        .map(|block| {
            let number = block.block_number();
            let keys = (0..KEYS_PER_BLOCK)
                .map(|index| keccak256((number * KEYS_PER_BLOCK / 2 + index).to_be_bytes()))
                .collect::<Vec<_>>();
            let state = HashedPostState::default()
                .with_accounts(
                    keys.iter()
                        .map(|key| (*key, Some(Account { nonce: number, ..Default::default() }))),
                )
                .with_storages([(
                    B256::ZERO,
                    HashedStorage::from_iter(keys.iter().map(|key| (*key, U256::from(number)))),
                )])
                .into_sorted();
            let mut nodes = keys
                .iter()
                .map(|key| {
                    (Nibbles::unpack(key), Some(BranchNodeCompact::new(1, 0, 0, vec![], None)))
                })
                .collect::<Vec<_>>();
            nodes.sort_unstable_by_key(|(key, _)| *key);
            ExecutedBlock::<EthPrimitives>::new(
                Arc::clone(&block.recovered_block),
                Arc::clone(&block.execution_output),
                SortedTrieData::new(
                    Arc::new(state),
                    Arc::new(TrieUpdatesSorted::new(nodes, Default::default())),
                ),
            )
        })
        .collect::<Vec<_>>();
    let factory = create_test_provider_factory();
    let provider = factory.provider().unwrap();
    let anchor = BlockNumHash::new(0, blocks[0].recovered_block().parent_hash());
    let tip = blocks.last().unwrap().recovered_block().hash();
    let mut expected: Option<(Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)> = None;

    for cached_blocks in [0, 99, 98, 90, 50] {
        for hold_ancestor in [false, true] {
            if cached_blocks == 0 && hold_ancestor {
                continue
            }
            let mut samples = Vec::new();
            for _ in 0..REPETITIONS {
                let manager = OverlayManager::default();
                for block in &blocks {
                    manager.insert_block(block.clone());
                }
                let held = (cached_blocks > 0).then(|| {
                    manager
                        .overlay_builder(blocks[cached_blocks - 1].recovered_block().hash())
                        .build_state_trie_overlay_at_frontiers(&provider, anchor, anchor, true)
                        .unwrap()
                });
                let held = if hold_ancestor { held } else { None };
                let builder = manager.overlay_builder(tip);
                let start = Instant::now();
                let overlay = black_box(
                    builder
                        .build_state_trie_overlay_at_frontiers(&provider, anchor, anchor, true)
                        .unwrap(),
                );
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
                if let Some(expected) = &expected {
                    assert_eq!(&overlay.input().nodes, &expected.0);
                    assert_eq!(&overlay.input().state, &expected.1);
                } else {
                    expected = Some((
                        Arc::clone(&overlay.input().nodes),
                        Arc::clone(&overlay.input().state),
                    ));
                }
                black_box(&held);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "cached_blocks={cached_blocks} held={hold_ancestor} median_ms={:.3}",
                samples[REPETITIONS / 2],
            );
        }
    }
}
