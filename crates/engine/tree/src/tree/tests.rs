// -----------------------------------------------------------------------------
// Characterization tests for `query_header_with_td`.
//
// These lock down the observable behavior of the function across every
// distinct control-flow path, so a refactor can be verified against the
// current implementation. With the default `MockEthProvider`, the trait
// method `header_td_by_number` returns `Ok(None)` and test blocks carry
// zero difficulty, so the returned `Option<U256>` is always `None` and TD
// summation is not exercised — what these tests do verify is:
//
//   - which header is returned (canonical DB vs in-memory fork)
//   - whether the call returns `Ok` or `Err(HeaderNotFound(_))`
//   - which branch resolves each scenario
// -----------------------------------------------------------------------------

/// Builds a `TestHarness` with nothing pre-populated so each test can set
/// up canonical DB and `tree_state` directly.
fn td_query_harness() -> (TestHarness, TestBlockBuilder) {
    let harness = TestHarness::new(MAINNET.clone());
    let builder = TestBlockBuilder::eth();
    (harness, builder)
}

/// Inserts a block only into the canonical (`MockEthProvider`) side, leaving
/// `tree_state` untouched.
fn persist_canonical(harness: &TestHarness, block: &ExecutedBlock) {
    let recovered = block.recovered_block().clone();
    harness.persist_blocks(vec![recovered]);
}

/// Inserts a block only into the in-memory `tree_state`, leaving the
/// canonical provider untouched.
fn insert_tree_only(harness: &mut TestHarness, block: &ExecutedBlock) {
    harness.tree.state.tree_state.insert_executed(block.clone());
}

#[test]
fn query_header_with_td_canonical_hit_returns_db_header() {
    let (mut harness, mut builder) = td_query_harness();
    let blocks: Vec<_> = builder.get_executed_blocks(0..3).collect();
    for block in &blocks {
        persist_canonical(&harness, block);
    }
    // Leave tree_state empty so resolution can only come from canonical.
    let _ = &mut harness;

    let target = blocks[1].recovered_block();
    let (header, td) = harness
        .tree
        .query_header_with_td(target.number(), target.hash())
        .expect("canonical hit should succeed");

    assert_eq!(header.number, target.number());
    assert_eq!(SealedHeader::seal_slow(header).hash(), target.hash());
    assert_eq!(td, None);
}

#[test]
fn query_header_with_td_unknown_hash_is_header_not_found() {
    let (mut harness, mut builder) = td_query_harness();
    // Populate a couple of canonical blocks just so last_block_number resolves.
    let blocks: Vec<_> = builder.get_executed_blocks(0..2).collect();
    for block in &blocks {
        persist_canonical(&harness, block);
    }
    let _ = &mut harness;

    let bogus = B256::random();
    let err = harness.tree.query_header_with_td(10, bogus).unwrap_err();

    assert_matches!(err, ProviderError::HeaderNotFound(_));
}

#[test]
fn query_header_with_td_fork_direct_canonical_parent_above_tip() {
    // Canonical tip at 0; fork at height 1 whose parent is canonical 0.
    // Loop condition `0 >= 0` is TRUE, so the main loop runs once and
    // resolves via its canonical-hit arm.
    let (mut harness, mut builder) = td_query_harness();
    let canonical_0 = builder.get_executed_block_with_number(0, B256::ZERO);
    persist_canonical(&harness, &canonical_0);

    let fork_1 = builder.get_executed_block_with_number(1, canonical_0.recovered_block().hash());
    insert_tree_only(&mut harness, &fork_1);

    let (header, td) = harness
        .tree
        .query_header_with_td(1, fork_1.recovered_block().hash())
        .expect("fork lookup should succeed");

    assert_eq!(header.number, 1);
    assert_eq!(
        SealedHeader::seal_slow(header).hash(),
        fork_1.recovered_block().hash(),
        "must return the in-memory fork header, not any canonical header",
    );
    assert_eq!(td, None);
}

#[test]
fn query_header_with_td_fork_multi_hop_walk_to_canonical() {
    // Canonical tip at 0; three fork blocks above it (1, 2, 3) in
    // tree_state. The main loop walks through 3 -> 2 -> 1 in tree_state
    // and finally resolves at canonical 0.
    let (mut harness, mut builder) = td_query_harness();
    let canonical_0 = builder.get_executed_block_with_number(0, B256::ZERO);
    persist_canonical(&harness, &canonical_0);

    let fork_1 = builder.get_executed_block_with_number(1, canonical_0.recovered_block().hash());
    let fork_2 = builder.get_executed_block_with_number(2, fork_1.recovered_block().hash());
    let fork_3 = builder.get_executed_block_with_number(3, fork_2.recovered_block().hash());
    insert_tree_only(&mut harness, &fork_1);
    insert_tree_only(&mut harness, &fork_2);
    insert_tree_only(&mut harness, &fork_3);

    let (header, td) = harness
        .tree
        .query_header_with_td(3, fork_3.recovered_block().hash())
        .expect("fork walk should succeed");

    assert_eq!(header.number, 3);
    assert_eq!(SealedHeader::seal_slow(header).hash(), fork_3.recovered_block().hash(),);
    assert_eq!(td, None);
}

#[test]
fn query_header_with_td_fork_parent_unknown_above_tip_errors() {
    // Canonical tip at 0, fork block at height 5 whose parent is neither
    // canonical nor in tree_state. Main loop runs (5 - 1 = 4 >= 0), hits
    // the None arm for both provider and tree_state, returns
    // HeaderNotFound.
    let (mut harness, mut builder) = td_query_harness();
    let canonical_0 = builder.get_executed_block_with_number(0, B256::ZERO);
    persist_canonical(&harness, &canonical_0);

    let orphan = builder.get_executed_block_with_number(5, B256::random());
    insert_tree_only(&mut harness, &orphan);

    let err = harness.tree.query_header_with_td(5, orphan.recovered_block().hash()).unwrap_err();
    assert_matches!(err, ProviderError::HeaderNotFound(_));
}

#[test]
fn query_header_with_td_equal_height_fork_attaches_at_tip() {
    // Canonical chain has blocks 0 and 1 (tip = 1). A fork block ALSO at
    // height 1 sits in tree_state with parent = canonical 0.
    // `current_number = number - 1 = 0 < last_block_number = 1`, so the
    // main while-loop body never runs. This is the fallback scenario that
    // commit 3e06cdbef added: the final canonical lookup on `current_hash`
    // must resolve, returning the fork header (not the canonical one).
    let (mut harness, mut builder) = td_query_harness();
    let canonical_0 = builder.get_executed_block_with_number(0, B256::ZERO);
    let canonical_1 =
        builder.get_executed_block_with_number(1, canonical_0.recovered_block().hash());
    persist_canonical(&harness, &canonical_0);
    persist_canonical(&harness, &canonical_1);

    let fork_1 = builder.get_executed_block_with_number(1, canonical_0.recovered_block().hash());
    insert_tree_only(&mut harness, &fork_1);

    let (header, td) = harness
        .tree
        .query_header_with_td(1, fork_1.recovered_block().hash())
        .expect("fork at tip height should resolve via fallback");

    assert_eq!(
        SealedHeader::seal_slow(header).hash(),
        fork_1.recovered_block().hash(),
        "must return the fork header, not the canonical sibling",
    );
    assert_eq!(td, None);
}

#[test]
fn query_header_with_td_fork_below_tip_direct_canonical_parent() {
    // Canonical chain 0..4 (tip = 3). A fork block at height 1 with parent
    // = canonical 0 sits in tree_state. `current_number = 0 < 3`, loop
    // skipped, fallback resolves via canonical ancestor.
    let (mut harness, mut builder) = td_query_harness();
    let canonicals: Vec<_> = builder.get_executed_blocks(0..4).collect();
    for block in &canonicals {
        persist_canonical(&harness, block);
    }

    let fork_1 = builder.get_executed_block_with_number(1, canonicals[0].recovered_block().hash());
    insert_tree_only(&mut harness, &fork_1);

    let (header, td) = harness
        .tree
        .query_header_with_td(1, fork_1.recovered_block().hash())
        .expect("below-tip fork with canonical parent should resolve");

    assert_eq!(SealedHeader::seal_slow(header).hash(), fork_1.recovered_block().hash(),);
    assert_eq!(td, None);
}

#[test]
fn query_header_with_td_fork_below_tip_unknown_parent_returns_ok_none() {
    // Canonical tip = 3. Fork at height 1 whose parent is a random hash
    // that lives in neither canonical nor tree_state. Main loop skipped
    // (0 < 3). Fallback `block_number(unknown)` is None. This case
    // returns `Ok((fork_header, None))` — the legacy behavior the caller
    // depends on (Err here would turn `QueryTd` into a failure).
    let (mut harness, mut builder) = td_query_harness();
    let canonicals: Vec<_> = builder.get_executed_blocks(0..4).collect();
    for block in &canonicals {
        persist_canonical(&harness, block);
    }

    let orphan_fork = builder.get_executed_block_with_number(1, B256::random());
    insert_tree_only(&mut harness, &orphan_fork);

    let (header, td) = harness
        .tree
        .query_header_with_td(1, orphan_fork.recovered_block().hash())
        .expect("below-tip fork with unknown parent must not return Err");

    assert_eq!(SealedHeader::seal_slow(header).hash(), orphan_fork.recovered_block().hash(),);
    assert_eq!(td, None);
}
