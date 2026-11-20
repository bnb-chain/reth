//! Controlled checks of the account-nonce read path used by latest-state RPCs.

use super::*;
use crate::OverlayStateProviderFactory;
use alloy_primitives::Address;
use reth_chain_state::test_utils::TestBlockBuilder;
use reth_provider::{
    test_utils::{create_test_provider_factory, MockNodeTypesWithDB},
    BlockWriter, ProviderFactory,
};
use reth_stages_types::{FinishCheckpoint, StageCheckpoint, StageId};
use reth_storage_api::{DatabaseProviderROFactory, StageCheckpointWriter, StateProvider};
use reth_tasks::WorkerPool;
use revm::{database::BundleState, state::AccountInfo};
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn fixture() -> (ProviderFactory<MockNodeTypesWithDB>, Vec<ExecutedBlock<EthPrimitives>>) {
    let factory = create_test_provider_factory();
    let blocks = TestBlockBuilder::eth()
        .get_executed_blocks(0..3)
        .enumerate()
        .map(|(index, mut block)| {
            let mut output = (*block.execution_output).clone();
            output.state = BundleState::builder(index as u64..=index as u64)
                .state_present_account_info(
                    Address::with_last_byte(index as u8),
                    AccountInfo { nonce: index as u64 + 7, ..Default::default() },
                )
                .build();
            block.execution_output = Arc::new(output);
            block
        })
        .collect::<Vec<_>>();
    let provider = factory.provider_rw().unwrap();
    provider.insert_block(blocks[0].recovered_block()).unwrap();
    provider
        .save_stage_checkpoint(
            StageId::Finish,
            StageCheckpoint::new(0)
                .with_finish_stage_checkpoint(FinishCheckpoint { partial_state_trie: Some(0) }),
        )
        .unwrap();
    provider.commit().unwrap();
    (factory, blocks)
}

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

#[test]
fn nonce_diagnostic_two_readers_wait_for_same_computing_entry() {
    let (factory, blocks) = fixture();
    let manager = OverlayManager::default();
    manager.insert_block(blocks[1].clone());
    let key = OverlayCacheKey {
        anchor_hash: blocks[0].recovered_block().hash(),
        tip_hash: blocks[1].recovered_block().hash(),
    };
    let expected = manager
        .execution_overlay_for_block_state(
            &manager.block_state(key.tip_hash).unwrap(),
            key.anchor_hash,
            OverlayCacheConfig::default(),
        )
        .unwrap();
    let waiter = Arc::new(OverlayWaiter::new());
    manager
        .execution_overlays
        .entries
        .insert(key, OverlayCacheEntry::Computing(Arc::clone(&waiter)));
    let (done_tx, done_rx) = mpsc::channel();
    let readers = (0..2)
        .map(|id| {
            let source = OverlayStateProviderFactory::new(
                factory.clone(),
                manager.overlay_builder(key.tip_hash),
            );
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                let provider = source.database_provider_ro().unwrap();
                let start = Instant::now();
                let nonce = provider.account_nonce(&Address::with_last_byte(1)).unwrap();
                done_tx.send((id, nonce, start.elapsed())).unwrap();
            })
        })
        .collect::<Vec<_>>();

    // The owner and cache hold two references; each blocked reader holds one more.
    let both_waiting = wait_until(|| Arc::strong_count(&waiter) >= 4);
    let before_release = done_rx.recv_timeout(Duration::from_millis(200));
    let released = Instant::now();
    waiter.finish(expected);
    for reader in readers {
        reader.join().unwrap();
    }
    assert!(both_waiting, "both readers must reach the same Computing entry");
    assert_eq!(before_release, Err(mpsc::RecvTimeoutError::Timeout));
    for _ in 0..2 {
        let (id, nonce, elapsed) = done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(nonce, Some(8));
        assert!(elapsed >= Duration::from_millis(200));
        eprintln!(
            "controlled Computing hold: reader={id} nonce={nonce:?} elapsed_ms={:.3}",
            elapsed.as_secs_f64() * 1000.0
        );
    }
    eprintln!(
        "both readers joined {:.3}ms after release",
        released.elapsed().as_secs_f64() * 1000.0
    );
}

#[test]
fn nonce_diagnostic_import_returns_before_precompute_is_ready() {
    let (factory, blocks) = fixture();
    let workers = Arc::new(WorkerPool::new(1, "nonce-diagnostic"));
    let manager = OverlayManager::new(Arc::clone(&workers));
    manager.insert_block(blocks[1].clone());
    let anchor = blocks[0].recovered_block().hash();
    manager
        .execution_overlay_for_block_state(
            &manager.block_state(blocks[1].recovered_block().hash()).unwrap(),
            anchor,
            OverlayCacheConfig::default(),
        )
        .unwrap();

    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    workers.spawn(move || {
        started_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let insert_started = Instant::now();
    manager.insert_block(blocks[2].clone());
    let insert_elapsed = insert_started.elapsed();
    let key = OverlayCacheKey { anchor_hash: anchor, tip_hash: blocks[2].recovered_block().hash() };
    let ready_after_insert = manager.execution_overlays.ready(&key).is_some();
    let source = OverlayStateProviderFactory::new(factory, manager.overlay_builder(key.tip_hash));
    let (done_tx, done_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let provider = source.database_provider_ro().unwrap();
        let nonce = provider.account_nonce(&Address::with_last_byte(2)).unwrap();
        done_tx.send(nonce).unwrap();
    });
    let computing = wait_until(|| {
        manager
            .execution_overlays
            .entries
            .get(&key)
            .is_some_and(|entry| matches!(entry.value(), OverlayCacheEntry::Computing(_)))
    });
    let before_release = done_rx.recv_timeout(Duration::from_millis(100));
    release_tx.send(()).unwrap();
    reader.join().unwrap();
    assert!(!ready_after_insert);
    assert!(computing);
    assert_eq!(before_release, Err(mpsc::RecvTimeoutError::Timeout));
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap(), Some(9));
    assert!(manager.execution_overlays.ready(&key).is_some());
    eprintln!(
        "insert_block returned in {:.3}ms while worker was held; nonce completed only after release",
        insert_elapsed.as_secs_f64() * 1000.0
    );
}

#[test]
fn retained_parent_serves_old_head_without_rebuilding() {
    let (factory, blocks) = fixture();
    let manager = OverlayManager::default();
    for block in &blocks[1..] {
        manager.insert_block(block.clone());
    }
    let key = OverlayCacheKey {
        anchor_hash: blocks[0].recovered_block().hash(),
        tip_hash: blocks[1].recovered_block().hash(),
    };
    let parent = manager
        .execution_overlay_for_block_state(
            &manager.block_state(key.tip_hash).unwrap(),
            key.anchor_hash,
            OverlayCacheConfig::default(),
        )
        .unwrap();
    // Capture the old head's read view before the child extends a copy of its cache entry.
    let source = OverlayStateProviderFactory::new(factory, manager.overlay_builder(key.tip_hash));
    let provider = source.database_provider_ro().unwrap();
    manager
        .execution_overlay_for_block_state(
            &manager.block_state(blocks[2].recovered_block().hash()).unwrap(),
            key.anchor_hash,
            OverlayCacheConfig::default(),
        )
        .unwrap();
    assert!(manager.execution_overlays.entries.contains_key(&key));

    assert_eq!(provider.account_nonce(&Address::with_last_byte(1)).unwrap(), Some(8));
    let rebuilt = manager.execution_overlays.ready(&key).unwrap();
    assert!(Arc::ptr_eq(&parent, &rebuilt));
    assert_eq!(parent.accounts(), rebuilt.accounts());
    assert!(!rebuilt.accounts().contains_key(&Address::with_last_byte(2)));
    eprintln!("old-head nonce stayed 8; its retained parent cache was reused without a rebuild");
}

#[test]
fn ready_probe_never_starts_or_waits_for_computation_and_pins_results() {
    let (factory, blocks) = fixture();
    let manager = OverlayManager::default();
    manager.insert_block(blocks[1].clone());
    let key = OverlayCacheKey {
        anchor_hash: blocks[0].recovered_block().hash(),
        tip_hash: blocks[1].recovered_block().hash(),
    };
    let source = OverlayStateProviderFactory::new(factory, manager.overlay_builder(key.tip_hash));
    let provider = source.database_provider_ro().unwrap();
    assert!(!provider.try_ready_execution_overlay().unwrap());
    assert!(manager.execution_overlays.entries.is_empty());
    let waiter = Arc::new(OverlayWaiter::new());
    manager.execution_overlays.entries.insert(key, OverlayCacheEntry::Computing(waiter.clone()));
    assert!(!provider.try_ready_execution_overlay().unwrap());
    assert_eq!(Arc::strong_count(&waiter), 2);
    manager.execution_overlays.entries.remove(&key);
    manager
        .execution_overlay_for_block_state(
            &manager.block_state(key.tip_hash).unwrap(),
            key.anchor_hash,
            OverlayCacheConfig::default(),
        )
        .unwrap();
    assert!(provider.try_ready_execution_overlay().unwrap());
    manager.execution_overlays.retain(|_, _| false);
    assert_eq!(provider.account_nonce(&Address::with_last_byte(1)).unwrap(), Some(8));
    assert!(manager.execution_overlays.entries.is_empty());
}
