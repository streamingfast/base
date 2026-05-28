//! Integration tests for the firehose flashblocks processor.
//!
//! The test framework lives in [`framework`] and provides flashblock builders, output parsers,
//! and the [`run_flashblock_sequence`] harness. This file contains only the individual test
//! cases.
//!
//! # Adding new test cases
//!
//! 1. Build a `Vec<TestEvent>` using the free helpers [`framework::flash_base`],
//!    [`framework::flash_delta`], and [`framework::canonical_block`] — each returns a
//!    [`framework::TestEvent`] directly, so no wrapping is needed.
//! 2. Call [`framework::run_flashblock_sequence`] with the events and a [`framework::GenesisClient`].
//! 3. Call [`framework::parse_fire_events`] to validate the emitted output.
//! 4. Use [`framework::assert_fire_events_metadata_eq`] for metadata-only assertions or
//!    [`framework::assert_fire_events_eq`] when you also need to verify the decoded block payload.

mod framework;

use base_execution_chainspec::BaseChainSpec;

use framework::{
    FireEvent, GenesisClient, assembled_block_hash, assert_fire_events_eq,
    assert_fire_events_metadata_eq, canonical_block, flash_base, flash_delta,
    flash_delta_with_payload_id, flash_delta_with_txs, hash, parse_fire_events,
    run_flashblock_sequence, run_flashblock_sequence_at, run_flashblock_sequence_at_with_processor,
    run_flashblock_sequence_without_peek, signed_legacy_transfer, test_genesis,
};

/// A lone base flashblock (wire idx 0) never emits a standalone FIRE BLOCK: its
/// printed flash_idx would be 0, which collides with canonical FIRE BLOCKs on a
/// single merged firehose stream. The base is squashed and held for a future delta
/// (or for the is_final emission via the +1000 marker). With nothing following it,
/// the block stays silent on the flashblock tracer; downstream consumers will see
/// the canonical FIRE BLOCK when block 1 is committed.
#[test]
fn flash_base_emits_no_standalone_fire_block() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        // Block 1, parent = genesis block hash, timestamp = genesis + 2 seconds.
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2)],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(&events, &[]);
}

/// Sends a base flashblock then one delta for the same block. The base is squashed
/// (no standalone FIRE BLOCK — flash_idx=0 collides with canonical) and folded into
/// the delta's emission. A single FIRE BLOCK is emitted at `flash_idx=1` carrying
/// the transactions from both base and delta.
#[test]
fn base_plus_delta_emits_single_merged_fire_block() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("1a"), 1)],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Only the delta emits (flash_idx=1); the base's data is gathered into
            // this execution via the last_executed_index=None codepath.
            FireEvent::flash_block(1, hash("1a"), 1, false),
        ],
    );
}

/// Sends a base flashblock then two deltas for the same block. The base is squashed
/// (no standalone FIRE BLOCK — flash_idx=0 collides with canonical) and folded into
/// the delta's emission. A single FIRE BLOCK is emitted at `flash_idx=1` carrying
/// the transactions from both base and delta, then another one for `flash_idx=2`
#[test]
fn base_plus_two_deltas_emits_two_fire_blocks() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence_without_peek(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("1a"), 1),
            flash_delta(1, hash("1b"), 2),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 1, false),
            FireEvent::flash_block(1, hash("1b"), 2, false),
        ],
    );
}

/// Sends base for block N, one delta for block N, then base for block N+1.
///
/// All three lines come from the flashblock tracer, so all three parse as
/// [`FireEvent::FlashBlock`]:
/// - `FlashBlock(N, idx=0)` — the base flashblock for block N.
/// - `FlashBlock(N, idx=1)` — the delta for block N.
/// - `FlashBlock(N+1, idx=0)` — the base flashblock for block N+1.
///
/// This exercises the cross-block state carry-forward path where the accumulated EVM state from
/// block N is carried forward to seed block N+1 without waiting for the canonical `StateProvider`
/// to reflect block N.
#[test]
fn base_plus_delta_plus_next_base() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("1a"), 1),
            // Block 2's parent_hash is "1b" — doesn't match the recomputed block-1 hash.
            // Peek-driven is_final fires on delta(1,1): recompute hash, compare with
            // "1b", mismatch → `mark_failed` drops the FIRE BLOCK for delta(1,1) and
            // resets state. Block 2's base then sees an empty in-flight sequence and
            // cannot bootstrap (block 1 not marked canonical) → pending_state, no emit.
            flash_base(2, hash("2a"), hash("1b"), ts + 4),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        // Base squashed, delta(1,1) attempted with is_final → mismatch → mark_failed
        // drops its FIRE BLOCK and resets state; block 2's base is then squashed too
        // (no peek follows it). Nothing reaches the flashblock tracer.
        &[],
    );
}

#[test]
fn base_plus_delta_plus_next_base_but_no_state_yet() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    // Block 2's `parent_hash` must equal the locally-recomputed block-1 hash so the
    // FirstOfNextBlock is_final attempt validates. To also satisfy the in-flight-tip
    // parent-hash sanity check (which compares the new base's `parent_hash` against
    // block 1's last flashblock's `diff.block_hash`), seal the block-1 delta with the
    // same value.
    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("1b"), 1)];
    let block1_recomputed = assembled_block_hash(&placeholder);

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, block1_recomputed, 1),
            // Sequential fast path: block 2's base lands on block 1's accumulated state.
            flash_base(2, hash("2a"), block1_recomputed, ts + 4),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (peek=delta(1,1) is same-block; no is_final).
            // Peek-driven single-emission is_final on delta(1,1): peek saw block 2's
            // base, so delta(1,1) emits ONCE with the recomputed hash + is_final
            // marker (printed as 1001) carrying base+delta txs.
            FireEvent::flash_block(1, block1_recomputed, 1, true),
            // Base(2) squashed (no peek follows).
        ],
    );
}

/// Sends the same base flashblock twice and asserts that only one `FIRE BLOCK` is emitted.
///
/// The sequence validator returns `Duplicate` for the second base; the processor logs and
/// discards it without re-executing or emitting a second FIRE line.
#[test]
fn duplicate_base_is_ignored() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            // Send the same base twice — the duplicate is silently dropped.
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Base is squashed (flash_idx=0 collides with canonical); duplicate is dropped.
    // No standalone FIRE BLOCK reaches the flashblock tracer.
    assert_fire_events_metadata_eq(&events, &[]);
}

/// Sends a base for block N, then a delta with index 2 (skipping index 1).
///
/// Asserts that only one `FIRE BLOCK` is emitted (for the base). The gap causes the
/// sequence validator to return `NonSequentialGap`, which resets the processor state. No
/// further FIRE lines are emitted for the out-of-sequence delta.
#[test]
fn non_sequential_delta_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            // Skip index 1 and send index 2 directly — creates a NonSequentialGap.
            flash_delta(1, hash("1a"), 2),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Base squashed (flash_idx=0 collides with canonical), gap delta dropped.
    // No FIRE BLOCK reaches the flashblock tracer.
    assert_fire_events_metadata_eq(&events, &[]);
}

/// Sends a base + two successive deltas (idx=1, idx=2) for the same block.
///
/// With the peek-driven squash optimisation, `delta(1,1)` is dropped from the
/// emission stream because the runner's peek window sees `delta(1,2)` already
/// queued. Only `delta(1,2)` runs through the EVM (with `delta(1,1)`'s transactions
/// folded into the same execution) and emits a FIRE BLOCK.
///
/// Asserts two [`FireEvent::FlashBlock`] events: `idx=0` (base) and `idx=2` (the
/// last accumulated delta). `idx=1` is silently squashed.
#[test]
fn two_successive_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("1a"), 1),
            flash_delta(1, hash("1a"), 2),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base and delta(1,1) both squashed; only the consolidated emission for
            // idx=2 lands, carrying base + delta(1,1) + delta(1,2) txs.
            FireEvent::flash_block(1, hash("1a"), 2, false),
        ],
    );
}

/// Sends a base then a delta with index=2, skipping index=1.
///
/// This is equivalent to [`non_sequential_delta_is_skipped`] but verifies the behaviour is
/// consistent: the gap at idx=2 triggers `NonSequentialGap`, the processor resets its state,
/// and only the base `FIRE BLOCK` is emitted. Any further deltas on the same block after the
/// reset are also dropped (no in-flight sequence + non-zero index → dropped).
#[test]
fn jumping_delta_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            // Jump directly to idx=2, skipping idx=1.
            flash_delta(1, hash("1a"), 2),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Base squashed, jumping delta(1,2) gap → reset. Nothing emitted.
    assert_fire_events_metadata_eq(&events, &[]);
}

/// Sends a base + three successive deltas (idx=1, idx=2, idx=3) for the same block.
///
/// With the peek-driven squash optimisation, the runner pre-feeds all events, so
/// when each of `delta(1,1)` and `delta(1,2)` is processed the peek window already
/// shows the next same-block delta. Both are squashed; only `delta(1,3)` executes
/// (with all squashed deltas' transactions gathered in one EVM pass) and emits.
#[test]
fn three_successive_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("1a"), 1),
            flash_delta(1, hash("1a"), 2),
            flash_delta(1, hash("1a"), 3),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base, delta(1,1), and delta(1,2) all squashed; only the consolidated
            // emission at idx=3 lands.
            FireEvent::flash_block(1, hash("1a"), 3, false),
        ],
    );
}

/// Sends two full block cycles, each with one delta: base(N)+delta(N,1)+base(N+1)+delta(N+1,1).
///
/// Asserts four [`FireEvent::FlashBlock`] events:
/// `FlashBlock(N, idx=0)`, `FlashBlock(N, idx=1)`, `FlashBlock(N+1, idx=0)`, `FlashBlock(N+1, idx=1)`.
/// Exercises cross-block state carry-forward together with inter-block delta processing.
#[test]
fn two_blocks_with_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    // Block 2's `parent_hash` must equal block 1's locally-recomputed hash so the
    // FirstOfNextBlock is_final attempt validates and the transition proceeds.
    // Mirror the recomputed hash onto block 1's last flashblock's `diff.block_hash`
    // so the in-flight-tip parent-hash sanity check also passes.
    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("1a"), 1)];
    let block1_recomputed = assembled_block_hash(&placeholder);

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, block1_recomputed, 1),
            flash_base(2, hash("2a"), block1_recomputed, ts + 4),
            flash_delta(2, hash("2a"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed.
            // Peek caught block 2's base → delta(1,1) emits once as is_final
            // (printed 1001) carrying base+delta(1,1) txs.
            FireEvent::flash_block(1, block1_recomputed, 1, true),
            // Base(2) squashed; delta(2,1) emits carrying base(2)+delta(2,1) txs.
            FireEvent::flash_block(2, hash("2a"), 1, false),
        ],
    );
}

/// Verifies that the decoded block payload is non-empty and has the correct block
/// number when a delta emits — folds in the base squash behavior. Sends base + one
/// delta for block 1; the base is squashed and a single FIRE BLOCK at flash_idx=1
/// is emitted carrying both. Asserts the decoded `sf.ethereum.type.v2.Block`
/// protobuf carries `number == 1`, non-empty `hash`, and roundtrips through the
/// framework helpers.
#[test]
fn block_payload_has_correct_block_number() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("1a"), 1)],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_eq!(events.len(), 1, "expected exactly one FlashBlock event (base squashed)");

    // Extract the decoded EthBlock and verify key fields directly.
    let FireEvent::FlashBlock { block_number, flash_idx, block: ref eth_block, .. } = events[0]
    else {
        panic!("expected FireEvent::FlashBlock, got {:?}", events[0]);
    };
    assert_eq!(block_number, 1, "block_number metadata must be 1");
    assert_eq!(flash_idx, 1, "delta emits at flash_idx=1 (base squashed)");
    assert_eq!(eth_block.number, 1, "decoded EthBlock.number must be 1");
    assert!(!eth_block.hash.is_empty(), "decoded EthBlock.hash must be non-empty");

    // Use assert_fire_events_eq for a full payload roundtrip check: build an expected event
    // using the actual decoded block so that the comparison verifies the EthBlock survives
    // the parse/clone cycle intact.
    let expected = vec![FireEvent::FlashBlock {
        block_number: 1,
        block_hash: hash("1a"),
        flash_idx: 1,
        is_final: false,
        prev_block_number: 0,
        lib_num: 0,
        timestamp_ns: 0,
        block: eth_block.clone(),
    }];
    assert_fire_events_eq(&events, &expected);
}

/// Exercises the bootstrap path: send base for block 2 after marking block 1 as canonical.
///
/// When the processor receives the base for block 2, `accumulated_db` is `None` because
/// block 2 is not the sequential successor of any previously processed block. It therefore
/// calls `state_by_block_number_or_tag(BlockNumberOrTag::Number(1))` on the client to
/// bootstrap its EVM state.
///
/// Without a prior [`TestEvent::canonical_block(1, hash("1a"))`], `GenesisClient` would return a
/// `ProviderError` for block 1, causing the processor to exhaust its retries and skip the
/// block. With the canonical block event applied, the provider returns successfully and the
/// processor emits a `FIRE BLOCK` for block 2.
///
/// This verifies the [`TestEvent::CanonicalBlock`] path: it both marks block 1 available in
/// [`GenesisClient`] so that the bootstrap provider call succeeds, and emits a canonical FIRE
/// BLOCK (simulating the global `ExEx` tracer), for a total of three emitted events.
///
/// - `base1` → Flash1 (flashblock tracer, block 1).
/// - `canonical_block(1, hash("1a"))` → Canonical1 (canonical tracer, block 1) + makes block 1 available.
/// - `base2` → Flash2 (flashblock tracer, block 2, state carried forward via `accumulated_db`).
#[test]
fn canonical_block_unblocks_next_base() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    // Block 2's `parent_hash` must equal the locally-recomputed block-1 hash so the
    // FirstOfNextBlock is_final attempt validates. Use that same value as the
    // canonical hash so the parent-hash sanity check against `latest_canonical` also
    // passes.
    let block1_recomputed =
        assembled_block_hash(&[flash_base(1, hash("1a"), genesis_hash, ts + 2)]);

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            canonical_block(1, block1_recomputed),
            flash_base(2, hash("2a"), block1_recomputed, ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Peek for flash_base(1) skips the canonical event and sees flash_base(2);
            // block 2's base is for block N+1, so flash_base(1) is treated as the
            // final partial for block 1. Single FIRE BLOCK sealed with the recomputed
            // hash and is_final marker (printed 1000) — no collision because is_final
            // adds +1000 to the printed flash_idx.
            FireEvent::flash_block(1, block1_recomputed, 0, true),
            FireEvent::canonical_block(1, block1_recomputed), // Canonical1 — canonical tracer
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// Exercises the bootstrap path: sending a base for block 2 when the processor has no prior
/// context requires bootstrapping from the provider at block 1.
///
/// Without [`TestEvent::canonical_block(1, hash("1a"))`], `GenesisClient` returns a `ProviderError` for
/// block 1 and the processor exhausts its retries, causing block 2 to be skipped.
/// With the canonical block event applied first, the provider returns successfully and the
/// processor emits a `FIRE BLOCK` for block 2.
///
/// This tests the bootstrap path in isolation: no prior flashblock context means
/// `accumulated_db` is always `None` and the provider call for the parent block is mandatory.
///
/// The `canonical_block(1, hash("1a"))` event also emits a canonical FIRE BLOCK (simulating the global
/// `ExEx` tracer), so two events appear in total: Canonical1 then Flash2.
#[test]
fn canonical_block_unblocks_non_sequential_gap() {
    let genesis = test_genesis();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            canonical_block(1, hash("1a")),
            // Send base for block 2 as the very first flashblock (no block 1 context).
            // The processor has no accumulated_db, so it must bootstrap from the provider
            // at block 1. Block 2's parent must match canonical block 1's hash for the
            // parent-hash sanity check.
            flash_base(2, hash("2a"), hash("1a"), ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1, hash("1a")), // Canonical1 — canonical tracer
            FireEvent::flash_block(2, hash("2b"), 1, false), // Flash2 — flashblock tracer, flash_idx=1
        ],
    );
}

/// Sends `base1 → canonical_block(1, hash("1a")) → canonical_block(2, hash("2a")) → base3`
/// and verifies that four FIRE BLOCK lines are emitted in order: Flash1, Canonical1,
/// Canonical2, Flash3.
///
/// Sequence breakdown:
/// - `base1` → flashblock for block 1 (`flash_idx` 0) → Flash1 emitted by flashblock tracer.
/// - `canonical_block(1, hash("1a"))` → makes block 1 available **and** emits Canonical1 via the canonical
///   tracer, simulating the global `ExEx` tracer finalising block 1.
/// - `canonical_block(2, hash("2a"))` → makes block 2 available **and** emits Canonical2 via the canonical
///   tracer, simulating the global `ExEx` tracer finalising block 2 (even though no flashblock was
///   sent for block 2).
/// - `base3` → flashblock for block 3 (`flash_idx` 0). The processor detects a block gap
///   (current = 1, incoming = 3) which triggers the bootstrap path. Because block 2 was already
///   marked available by the preceding `canonical_block(2, …)`, `state_by_block_number_or_tag(2)`
///   succeeds on the first attempt → Flash3 emitted.
///
/// All four FIRE BLOCK lines use `flash_idx == 0` on the wire, but the harness disambiguates
/// them by source: Flash1 and Flash3 come from the flashblock tracer and parse as
/// [`FireEvent::FlashBlock`] (with `flash_idx == 0`); Canonical1 and Canonical2 come from the
/// canonical tracer and parse as [`FireEvent::Block`]. Each tracer writes to its own buffer;
/// the runner interleaves the two streams in event-processing order using `# SOURCE FLASH` /
/// `# SOURCE CANON` markers, which the parser uses to assign each FIRE BLOCK to the right
/// variant.
#[test]
fn base_canonical_gap_then_base_emits_four_fire_blocks() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            // No flashblock for block 2 — only canonical events for blocks 1 and 2.
            canonical_block(1, hash("1a")),
            canonical_block(2, hash("2a")),
            // Block 3's parent must match canonical block 2's hash for the parent-hash
            // sanity check.
            flash_base(3, hash("3a"), hash("2a"), ts + 6),
            flash_delta(3, hash("3b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (no peek emit at flash_idx=0).
            FireEvent::canonical_block(1, hash("1a")),
            FireEvent::canonical_block(2, hash("2a")),
            // Base(3) squashed; delta(3,1) emits at flash_idx=1 carrying base+delta txs.
            FireEvent::flash_block(3, hash("3b"), 1, false),
        ],
    );
}

#[test]
fn base_canonical_after_flashblock_flushes_buffer() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis);

    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            canonical_block(1, hash("1a")),
            // these will get accumulated in buffer
            flash_base(3, hash("3a"), hash("2a"), ts + 6),
            flash_delta(3, hash("3b"), 1),
            // this triggers the flashblocks
            canonical_block(2, hash("2a")),
            // these will get accumulated in buffer
            flash_base(4, hash("4a"), hash("3b"), ts + 8),
            flash_delta(4, hash("4b"), 1),
            // this should NOT trigger the flashblocks 4 because their parent is wrong
            canonical_block(3, hash("3a")),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed; on_canonical_block(1) tries the in-flight recompute
            // (state.current_block_number=Some(1), final_part_sent=false) but the
            // mock provider's state_root=ZERO yields a hash that mismatches
            // canonical hash("1a") → reset state, no is_final emitted.
            FireEvent::canonical_block(1, hash("1a")),
            FireEvent::canonical_block(2, hash("2a")),
            // Base(3) squashed, delta(3,1) emits carrying base+delta(3,1) txs.
            FireEvent::flash_block(3, hash("3b"), 1, false),
            FireEvent::canonical_block(3, hash("3a")),
        ],
    );
}

#[test]
fn base_ordered_wrong_hash() {
    let genesis = test_genesis();

    let client = GenesisClient::new(genesis);

    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            // this overrides genesis
            canonical_block(1, hash("1a")),
            flash_base(2, hash("2a"), hash("1b"), ts + 4), // this has wrong parent hash, should not send
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(&events, &[FireEvent::canonical_block(1, hash("1a"))]);
}

/// Verifies that a delta whose `payload_id` differs from the in-flight base's is
/// discarded, and the processor refuses any subsequent deltas until a fresh base
/// starts a new sequence.
///
/// Sequence:
/// 1. `base1` (payload_id = 0) → emit Flash1.0.
/// 2. `delta1_1` with payload_id = 42 → mismatch with base's payload_id 0; discard and
///    reset. No FIRE BLOCK emitted.
/// 3. `delta1_2` with payload_id = 0 → no in-flight sequence after the reset, and a
///    delta (index != 0) cannot start a sequence; dropped, waiting for next base.
#[test]
fn delta_with_wrong_payload_id_is_discarded() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            // Base uses the default payload_id (all zeros).
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            // Delta with a different payload_id — must be discarded, and the processor
            // must reset its in-flight sequence so any further deltas are also ignored
            // until a fresh base arrives.
            flash_delta_with_payload_id(1, hash("1b"), 1, 42),
            // Even a delta with the correct payload_id won't be accepted now, because
            // the prior mismatch reset the in-flight sequence.
            flash_delta(1, hash("1c"), 2),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Base squashed (flash_idx=0 collides with canonical); the delta with bad
    // payload_id resets state; the subsequent delta is dropped (no in-flight).
    // Nothing reaches the flashblock tracer.
    assert_fire_events_metadata_eq(&events, &[]);
}

/// A flashblock whose timestamp is more than the staleness threshold (5 s) in the past
/// is discarded without producing any FIRE BLOCK emission.
///
/// The processor's clock is pinned to `ts + 2 + 10` — 10 s ahead of the base flashblock's
/// timestamp (`ts + 2`). With the threshold at 5 s the flashblock is ~10 s old → stale →
/// skipped.
#[test]
fn stale_flashblock_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence_at(
        client,
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2)],
        ts + 2 + 10,
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert!(
        events.is_empty(),
        "expected no FIRE BLOCK events for a stale flashblock, got {:?}",
        events
    );
}

/// Trigger A for is_final emission: the first flashblock of block N+1 arrives, and its
/// `parent_hash` matches the hash recomputed from block N's accumulated flashblocks.
///
/// Sequence:
/// 1. Block 1 has a base + a delta. The processor recomputes the block hash by
///    deriving the post-execution `state_root` from the EVM bundle (the wire's
///    state_root is unused). In tests the mock provider returns `B256::ZERO`, so the
///    sealed header carries `state_root = ZERO` — same as the assembled header used
///    by [`assembled_block_hash`], which lets us predict the recomputed hash.
/// 2. We pre-compute that hash via [`assembled_block_hash`] and feed it as the
///    `parent_hash` of block 2's base flashblock.
/// 3. The processor sees the match on `FirstOfNextBlock` and emits one extra FIRE BLOCK
///    line for block 1's final partial with `is_final = true` (printed flash index =
///    final_index + 1000), sealed with the recomputed hash.
///
/// Expected order:
/// - Block 1 base (idx 0) — non-final
/// - Block 1 delta (idx 1) — non-final
/// - Block 1 is_final partial (idx 1, is_final = true) — emitted just before block 2
///   starts executing
/// - Block 2 base (idx 0) — non-final
#[test]
fn is_final_emitted_on_next_base_match() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    // First, build a placeholder block-1 sequence to extract the recomputed hash via
    // `assembled_block_hash`. The placeholder diff.block_hash doesn't affect
    // `Header::hash_slow()` since the header excludes block_hash. We then rebuild the
    // sequence with the LAST delta's diff.block_hash set to the recomputed hash, so
    // that the processor's parent-hash sanity check on block 2's base (which compares
    // against the in-flight tip's diff.block_hash) passes.
    let placeholder = vec![
        flash_base(1, hash("1a"), genesis_hash, ts + 2),
        flash_delta(1, hash("placeholder"), 1),
    ];
    let expected_block1_hash = assembled_block_hash(&placeholder);

    // Block-2 carries a delta(2,1) so the test can also assert that base(2) was
    // correctly processed: with base squashing, a lone base(2) would never emit
    // a FIRE BLOCK on its own. delta(2,1) emits at flash_idx=1 carrying
    // base(2)+delta(2,1) txs — its presence in the output proves base(2)
    // successfully bootstrapped its EVM state from block 1's carried-forward
    // accumulated_db.
    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, expected_block1_hash, 1),
            flash_base(2, hash("2a"), expected_block1_hash, ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (flash_idx=0 collides with canonical).
            // Peek-driven single-emission is_final: only ONE FIRE BLOCK lands for
            // delta(1,1), sealed with the recomputed hash and stamped is_final
            // (printed 1001) carrying base+delta(1,1) txs.
            FireEvent::flash_block(1, expected_block1_hash, 1, true),
            // Base(2) squashed; delta(2,1) emits at flash_idx=1 carrying
            // base(2)+delta(2,1) txs — verifies base(2) was processed.
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// Peek-driven squash: when a same-block delta is already waiting in the dispatch
/// queue at the moment we're about to execute the current delta, the current delta's
/// EVM execution and FIRE BLOCK emission are deferred. The queued delta executes
/// later with all the deferred transactions folded in.
///
/// This explicitly verifies the peek mechanism on a long burst:
/// - `flash_base(1,…)` is the base; bases are never squashed.
/// - `flash_delta(1,1)`, `flash_delta(1,2)`, `flash_delta(1,3)`, `flash_delta(1,4)`
///   are queued in the runner. Each one (except the last) is squashed because the
///   next item in the runner's peek window is another same-block flashblock.
/// - Only `flash_delta(1,4)` runs through the EVM (with txs from idx 1..4 gathered)
///   and emits.
///
/// Behaviour matches the user-requested "broader skip intermediate deltas": data is
/// preserved (prepended to the executed delta's tx list), only emission count drops.
#[test]
fn squash_chain_collapses_intermediate_deltas_into_last() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("1a"), 1),
            flash_delta(1, hash("1a"), 2),
            flash_delta(1, hash("1a"), 3),
            flash_delta(1, hash("1a"), 4),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base + deltas 1,2,3 all squashed; only the consolidated idx=4 emission
            // lands, carrying every accumulated tx.
            FireEvent::flash_block(1, hash("1a"), 4, false),
        ],
    );
}

/// Peek-driven squash does NOT apply when the next queued message is for a different
/// block — the current delta executes normally.
///
/// Sequence: `flash_base(1,…)`, `flash_delta(1,1)`, then `flash_base(2,…)` whose
/// parent_hash equals block 1's recomputed hash so the transition succeeds. The
/// peek at `flash_delta(1,1)` sees `flash_base(2,…)` — different block number, no
/// squash. `flash_delta(1,1)` executes and emits, then the FirstOfNextBlock
/// transition re-emits is_final for block 1.
#[test]
fn squash_does_not_apply_across_block_boundaries() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("1a"), 1)];
    let block1_recomputed = assembled_block_hash(&placeholder);

    // Add delta(2,1) so the test asserts that base(2) was processed correctly:
    // with base squashing, a lone base(2) wouldn't emit. delta(2,1) emits at
    // flash_idx=1 carrying base(2)+delta(2,1) txs — its presence verifies the
    // transition to block 2 worked.
    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, block1_recomputed, 1),
            flash_base(2, hash("2a"), block1_recomputed, ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (flash_idx=0 collides with canonical).
            // delta(1,1) executes — peek shows block 2's base, NOT a same-block
            // delta. peek classifies as is_final (next-block base) and delta(1,1)
            // emits ONCE with the recomputed hash + is_final marker (printed 1001)
            // carrying base+delta(1,1) txs.
            FireEvent::flash_block(1, block1_recomputed, 1, true),
            // Base(2) squashed; delta(2,1) emits carrying base(2)+delta(2,1) txs
            // — verifies base(2) was processed.
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// is_final hash mismatch on a peek-driven path: the processor matches geth's
/// `controller.go:300-306` behavior — when the recomputed block hash disagrees with
/// the next-block base's `parent_hash`, the FIRE BLOCK for the current delta is
/// dropped at the wire layer via `mark_failed` (geth's `OnBlockEnd(err)`),
/// `Skipping=true` is set (in our model: `state.reset()`), and a later canonical
/// notification is **not** treated as a fallback trigger.
///
/// Sequence:
/// 1. Block 1 base + delta with the in-flight tip `hash("1b")`.
/// 2. Block 2 base whose `parent_hash` is also `hash("1b")` — passes the in-flight
///    parent-hash sanity check. Peek for `flash_delta(1, "1b")` sees `flash_base(2,
///    "1b")`, classifies as is_final with `expected_parent_hash = "1b"`. The
///    processor executes the delta, recomputes the block hash (with `state_root =
///    ZERO` via the mock provider), compares against `"1b"`, **mismatch** → the
///    delta's FIRE BLOCK is `mark_failed`'d and dropped at the wire layer, the
///    processor resets state.
/// 3. A delta for block 2 then arrives and is buffered (parent state for block 1
///    not available) — no FIRE BLOCK.
/// 4. `canonical_block(1, hash("wrong-1"))` discards the buffered block-2
///    flashblocks because their base's `parent_hash` (`"1b"`) disagrees with the
///    canonical hash for block 1 (`"wrong-1"`).
#[test]
fn is_final_mismatch_resets_state_and_drops_new_block() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("1b"), 1),
            // Block 2's parent_hash matches block 1's in-flight tip (`hash("1b")`)
            // so the in-flight parent-hash sanity check passes — but it differs
            // from the locally-recomputed block-1 hash, so the peek-driven
            // is_final on delta(1,1) fails and the processor resets.
            flash_base(2, hash("2a"), hash("1b"), ts + 4),
            // Buffered after reset → bootstrap of block-1 state fails (block 1
            // never marked canonical) → no FIRE BLOCK.
            flash_delta(2, hash("2b"), 1),
            // Canonical for block 1 with the wrong hash: buffered block-2
            // flashblocks are discarded because their base's parent_hash differs.
            canonical_block(1, hash("wrong-1")),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed; delta(1,1) attempted with peek-driven is_final
            // → mismatch → mark_failed drops its FIRE BLOCK and resets state.
            // Block 2's base/delta buffered then discarded by canonical(1) with
            // wrong hash. Only the canonical block reaches the wire.
            FireEvent::canonical_block(1, hash("wrong-1")),
        ],
    );
}

/// Regression for the cumulative-across-block tracer offsets.
///
/// Each `execute_flashblock` call instantiates a fresh `BaseBlockExecutor`, so
/// revm hands back per-iteration receipt fields — `cumulative_gas_used` starts
/// at 0 again, `transaction_index` starts at 0 again, log `block_index` starts
/// at 0 again. Geth gets the canonical cumulative-across-block values for free
/// because its `StateProcessor` shares `usedGas`, `gp`, and `lastTxIndex`
/// across the `Process()` calls of a single block. The firehose-tracer fix
/// applies three offsets (`flashblock_tx_index_offset`,
/// `flashblock_cumulative_gas_offset`, `flashblock_log_block_index_offset`),
/// derived from the restored snapshot in `restore_flash_block_snapshot`, so
/// the FIRE BLOCK protobuf at flash idx K carries values cumulative across
/// the whole block through K.
///
/// Fixture: block 1 with three flashblocks, each emitted separately via the
/// no-peek runner so the snapshot+restore happens between iterations:
/// - base (idx 0): no transactions; pre-execution generates the EIP-4788
///   beacon-roots system call.
/// - delta (idx 1): one signed transfer (anvil account #0 → 0x010101…),
///   `gas_limit=21_000`, expected `gas_used=21_000`.
/// - delta (idx 2): a second signed transfer at nonce=1, same gas profile.
///
/// Asserts that the third FIRE BLOCK carries:
/// - `transaction_traces[0]` from the restored snapshot:
///   `index=0`, `gas_used=21000`, `cumulative_gas_used=21000`.
/// - `transaction_traces[1]` from THIS iteration's EVM run:
///   `index=1` (NOT 0, the EVM's per-iteration value),
///   `gas_used=21000`,
///   `cumulative_gas_used=42000` (NOT 21000, the EVM's per-iteration value).
///
/// Without the tracer offsets the second-delta values would be 0 and 21000.
#[test]
fn tracer_offsets_carry_cumulative_tx_fields_across_flashblocks() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence_without_peek(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta_with_txs(1, hash("1b"), 1, vec![signed_legacy_transfer(0)]),
            flash_delta_with_txs(1, hash("1c"), 2, vec![signed_legacy_transfer(1)]),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();
    assert_eq!(events.len(), 2, "expected 2 FIRE BLOCKs (base squashed, delta(1) + delta(2))");

    // First delta: carries delta(1)'s one tx (base was empty + squashed,
    // so nothing extra to gather). Offsets are 0 because no prior tx has
    // been emitted; the EVM's per-iteration values pass through.
    let FireEvent::FlashBlock { flash_idx, ref block, .. } = events[0] else {
        panic!("expected FlashBlock");
    };
    assert_eq!(flash_idx, 1, "first emission is delta(1) (base squashed)");
    assert_eq!(
        block.transaction_traces.len(),
        1,
        "delta(1) should carry the tx executed in this iteration"
    );
    let trx = &block.transaction_traces[0];
    assert_eq!(trx.index, 0, "delta(1) tx index = 0 (no prior txs in this block)");
    assert_eq!(trx.gas_used, 21_000, "21k for the transfer");
    let receipt = trx.receipt.as_ref().expect("receipt populated");
    assert_eq!(
        receipt.cumulative_gas_used, 21_000,
        "delta(1) tx cumulative_gas_used = its own gas (no prior)"
    );

    // Second delta: two txs in the FIRE BLOCK.
    // - traces[0] is the FIRST iteration's tx, restored from the snapshot —
    //   its index/cumulative_gas_used are unchanged (they were already
    //   correct when first emitted).
    // - traces[1] is the second iteration's tx; revm gave it
    //   `transaction_index = 0` and `cumulative_gas_used = 21_000` (its own
    //   gas, the iteration's first tx). The tracer's offsets MUST adjust
    //   these to canonical-across-block values.
    let FireEvent::FlashBlock { flash_idx, ref block, .. } = events[1] else {
        panic!("expected FlashBlock");
    };
    assert_eq!(flash_idx, 2);
    assert_eq!(
        block.transaction_traces.len(),
        2,
        "delta(2) FIRE BLOCK should carry both txs (one restored from the snapshot, one new)"
    );

    // Restored tx unchanged.
    let restored = &block.transaction_traces[0];
    assert_eq!(restored.index, 0);
    assert_eq!(restored.receipt.as_ref().unwrap().cumulative_gas_used, 21_000);

    // The regression assertions — without `flashblock_tx_index_offset` and
    // `flashblock_cumulative_gas_offset` these would be 0 and 21_000.
    let new_tx = &block.transaction_traces[1];
    assert_eq!(
        new_tx.index, 1,
        "without flashblock_tx_index_offset this would be 0 — the EVM's per-iteration tx index"
    );
    assert_eq!(new_tx.gas_used, 21_000);
    let new_receipt = new_tx.receipt.as_ref().expect("receipt populated");
    assert_eq!(
        new_receipt.cumulative_gas_used, 42_000,
        "without flashblock_cumulative_gas_offset this would be 21_000 — the EVM's per-iteration cumulative"
    );
}

/// Verifies the merged-replay path collapses a multi-flashblock buffered sequence
/// into a single FIRE BLOCK that still carries every required trace component
/// (transactions AND pre-execution system calls).
///
/// Sequence + reasoning:
/// 1. `flash_base(2, …)` and `flash_delta(2, 1)` arrive before block 1 is
///    canonical → buffered (`pending_state = true`), no FIRE BLOCK lines.
/// 2. `canonical_block(1, …)` triggers the merged-replay path. Instead of one
///    `execute_flashblock` call per buffered flashblock (which would emit a
///    FIRE BLOCK per entry), the processor assembles the buffered slice once,
///    gathers all transactions across base+delta into one list, and runs a
///    single EVM execution that emits ONE FIRE BLOCK stamped with the highest
///    buffered index.
/// 3. Pre-execution changes (EIP-4788 beacon-roots write) still run because the
///    merged call sets `is_first_execution_for_block = true` — the resulting
///    `system_call` lands on the merged FIRE BLOCK directly (no snapshot/restore
///    round-trip needed since there is only one emission).
///
/// Asserts:
/// - Exactly ONE FIRE BLOCK is emitted for block 2 (the merged one).
/// - It is stamped at the highest buffered index (1).
/// - It carries `system_calls.len() == 1` (the EIP-4788 write).
#[test]
fn snapshot_carries_cumulative_traces_across_replay() {
    let genesis = test_genesis();
    let _genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            // Buffered: block 1 isn't canonical yet, so base + delta wait in the
            // pending buffer with no FIRE BLOCK emission.
            flash_base(2, hash("2a"), hash("1a"), ts + 4),
            flash_delta(2, hash("2b"), 1),
            // canonical(1) triggers the merged-replay path: one FIRE BLOCK with
            // all buffered transactions and the highest buffered index (1).
            canonical_block(1, hash("1a")),
        ],
    );

    let flash_events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_eq!(
        flash_events.len(),
        1,
        "expected exactly 1 FIRE BLOCK line for block 2 (merged base+delta replay)"
    );

    let FireEvent::FlashBlock { flash_idx, block: ref merged_block, .. } = flash_events[0] else {
        panic!("expected FlashBlock");
    };
    assert_eq!(flash_idx, 1, "merged replay stamps the highest buffered index");
    assert_eq!(
        merged_block.system_calls.len(),
        1,
        "merged FIRE BLOCK must carry the EIP-4788 beacon-roots pre-execution system call"
    );
}

/// Regression for a prod-observed `nonce too low` failure: after the canonical-driven
/// replay path executes a buffered base + delta, [`ProcessorState::last_executed_index`]
/// MUST be set to the last replayed index — otherwise the next delta arriving through
/// the normal `process_inner` path sees `last_executed_index = None`, the multi-delta
/// tx-gather filter ("include every stored flashblock whose index > last_executed")
/// pulls in every previously-replayed delta's transactions, and the EVM trips on
/// already-applied state (`nonce X too low, expected X+1`).
///
/// Sequence:
/// 1. `flash_base(2, …)` and `flash_delta(2, 1)` arrive while block 1 is not yet
///    canonical → both buffered (`pending_state = true`), no FIRE BLOCK lines.
/// 2. `canonical_block(1, …)` triggers the replay path: both buffered flashblocks
///    are executed and their FIRE BLOCK lines are emitted. After the loop, the
///    processor stamps `last_executed_index = Some(1)` (the index of the last
///    replayed flashblock).
///
/// Asserts:
/// - Replay emits exactly two FIRE BLOCK lines (base + delta) for block 2.
/// - `processor.last_executed_index_for_test() == Some(1)` after the canonical
///   notification, proving the multi-delta gather won't re-execute the replayed
///   transactions on the next delta.
#[test]
fn canonical_replay_stamps_last_executed_index() {
    let genesis = test_genesis();
    let _genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    // Match the prod scenario from the WARN log:
    // - flashblocks for block N arrive before canonical(N-1) is committed
    // - on_canonical_block(N-1) replays both buffered flashblocks
    // - the next delta MUST execute only its own transactions, not the already-
    //   applied ones from the replay.
    let result = run_flashblock_sequence_at_with_processor(
        client,
        vec![
            // Block 2 base + delta(1) buffered because block 1 is not yet canonical.
            flash_base(2, hash("2a"), hash("1a"), ts + 4),
            flash_delta(2, hash("2b"), 1),
            // canonical(1) flushes the buffer.
            canonical_block(1, hash("1a")),
        ],
        ts + 4,
    );

    let events: Vec<FireEvent> = parse_fire_events(&result.raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Merged replay: ONE FIRE BLOCK stamped at the highest buffered index (1)
    // carrying all transactions from base + delta in a single EVM pass.
    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1, hash("1a")),
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );

    // The exact assertion that catches the bug. Without the fix, this was None
    // after replay and the next delta arriving through process_inner would have
    // re-run base + delta(1)'s transactions, tripping "nonce too low" in prod.
    assert_eq!(
        result.processor.last_executed_index_for_test(),
        Some(1),
        "canonical replay must stamp last_executed_index = Some(highest replayed index) \
         so the multi-delta tx-gather filter in process_inner doesn't re-execute already-applied transactions"
    );
}

/// Regression for the prod bug where block N+1's base was rejected because the
/// in-flight tip's `diff.block_hash` (sequencer-computed with `state_root = ZERO`)
/// disagreed with the new base's `parent_hash` (the real canonical hash, computed
/// with the post-execution state root). The fix removed the broken check; the
/// rigorous version still runs inside the `FirstOfNextBlock` branch via
/// [`build_is_final_emission`], which recomputes the previous block's hash with
/// the locally-computed state root and compares against the new base's
/// `parent_hash`.
///
/// Fixture mimics the op-rbuilder wire format: the block-1 delta's
/// `diff.block_hash` is set to an arbitrary value DIFFERENT from the recomputed
/// hash (simulating "sequencer reports a block_hash computed with null
/// state_root"). Block 2's base.parent_hash is the CORRECT recomputed hash.
///
/// Without the fix, the in-flight-tip check would reject block 2's base because
/// `delta.diff.block_hash != base.parent_hash`. With the fix, the test runner's
/// peek path catches block 2's base ahead-of-time and emits block 1's delta as
/// is_final with the recomputed hash; then block 2's base proceeds normally.
///
/// Asserts: block 1 base + is_final emission for block 1 delta + block 2 base
/// all reach the wire — block 2 is NOT dropped by a broken hash comparison.
#[test]
fn next_base_accepted_when_delta_diff_block_hash_diverges_from_recompute() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let placeholder = vec![
        flash_base(1, hash("1a"), genesis_hash, ts + 2),
        flash_delta(1, hash("placeholder"), 1),
    ];
    let recomputed_block1_hash = assembled_block_hash(&placeholder);

    // Simulates the op-rbuilder wire: the sequencer-reported `diff.block_hash`
    // for the last delta differs from the canonical (locally-recomputed) hash.
    let wire_block_hash = hash("wire-hash-with-null-state-root");
    assert_ne!(
        wire_block_hash, recomputed_block1_hash,
        "fixture must use a wire hash that differs from the recompute, to exercise the bug"
    );

    // Add delta(2,1) to verify base(2) was accepted and processed: a lone
    // base(2) would be squashed and invisible, so without an extra delta we
    // couldn't tell the recovery actually proceeded into block 2.
    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            // The delta carries the wire-reported hash (≠ recompute). The previous
            // in-flight-tip check would have compared this against block 2's
            // base.parent_hash and rejected block 2 outright.
            flash_delta(1, wire_block_hash, 1),
            // Block 2's base.parent_hash is the CANONICAL/recomputed hash. The
            // peek-driven is_final path catches this transition and recomputes
            // block 1's hash via state_root=ZERO from the mock provider — that
            // matches the recompute helper's prediction, so is_final emits with
            // the recomputed hash.
            flash_base(2, hash("2a"), recomputed_block1_hash, ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (flash_idx=0 collides with canonical).
            // Single is_final emission for block 1's delta sealed with the
            // recomputed hash (not the wire-reported one), carrying base+delta txs.
            FireEvent::flash_block(1, recomputed_block1_hash, 1, true),
            // Base(2) squashed; delta(2,1) emits — verifies base(2) accepted.
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// Regression for the prod scenario where canonical(N) arrives BEFORE the
/// peek-driven path catches block N+1's base. With the new `on_canonical_block`
/// Flow 1 logic, the canonical notification itself recomputes block N's hash
/// from the EVM bundle and emits is_final on match — so finality reaches the
/// flashblock-tracer stream regardless of whether next-base or canonical
/// arrives first.
///
/// Fixture: block 1 base + delta, then `canonical_block(1, recomputed_block1_hash)`.
/// No subsequent flashblock is queued, so the test runner's peek slot for the
/// block-1 delta is empty — the peek-driven is_final path does NOT fire. The
/// is_final emission must come from on_canonical_block.
#[test]
fn canonical_emits_is_final_when_no_subsequent_flashblock() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("any"), 1)];
    let recomputed_block1_hash = assembled_block_hash(&placeholder);

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            // Wire-reported block_hash that differs from the recompute (the
            // value the in-flight-tip check used to compare against).
            flash_delta(1, hash("wire-hash"), 1),
            // Canonical confirms block 1 with the locally-recomputable hash.
            // No subsequent Flashblock event, so the runner's peek slot for
            // the delta above is empty and the peek-driven is_final path is
            // never triggered — finality must reach the wire via
            // on_canonical_block's Flow 1 recompute.
            canonical_block(1, recomputed_block1_hash),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (flash_idx=0 collides with canonical).
            // Delta emits as non-final (no next-block-base in peek), carrying
            // base+delta txs.
            FireEvent::flash_block(1, hash("wire-hash"), 1, false),
            // Canonical FIRE BLOCK on the canonical tracer.
            FireEvent::canonical_block(1, recomputed_block1_hash),
            // is_final emitted by on_canonical_block at idx = latest_idx + 1 = 2
            // (printed as 1002): the previous execute_flashblock for the delta
            // already stamped the tracer's snapshot at `flash_index = 1`, so the
            // fallback emission has to step to 2 to clear the tracer's
            // strictly-greater check (matches geth's CurrentIndex++ behavior).
            FireEvent::flash_block(1, recomputed_block1_hash, 2, true),
        ],
    );
}

/// Mirror of [`canonical_emits_is_final_when_no_subsequent_flashblock`] but with
/// a deliberately-wrong canonical hash. The new `on_canonical_block` Flow 1
/// recomputes block 1's hash, sees a mismatch, and resets state — matching
/// geth's `Skipping = true` behavior on hash divergence.
#[test]
fn canonical_with_wrong_hash_resets_state() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("any"), 1)];
    let recomputed_block1_hash = assembled_block_hash(&placeholder);
    let wrong_canonical_hash = hash("definitely-not-the-recompute");
    assert_ne!(wrong_canonical_hash, recomputed_block1_hash);

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("wire-hash"), 1),
            // Canonical with WRONG hash → Flow 1 mismatch → reset.
            canonical_block(1, wrong_canonical_hash),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (flash_idx=0 collides with canonical).
            FireEvent::flash_block(1, hash("wire-hash"), 1, false),
            FireEvent::canonical_block(1, wrong_canonical_hash),
            // NO is_final emission. State reset.
        ],
    );
}

/// Verifies the no-peek production scenario (slow-drip WS arrivals: peek slot
/// empty when each flashblock is delivered). Block 2's base.parent_hash is the
/// recomputed block-1 hash; without peek catching the transition, the
/// `FirstOfNextBlock` fallback inside `process_inner` runs the recompute and
/// emits is_final for block 1. The previously-broken in-flight-tip check would
/// have rejected block 2's base before this fallback could run.
#[test]
fn next_base_accepted_without_peek_when_recompute_matches() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("wire-hash"), 1)];
    let recomputed_block1_hash = assembled_block_hash(&placeholder);

    // The no-peek runner feeds flashblocks through `on_flashblock_received`
    // (no peek hint), so the peek-driven is_final path never fires. The
    // FirstOfNextBlock fallback inside `process_inner` is what validates the
    // transition. delta(2,1) emits at flash_idx=1 carrying base(2)+delta(2,1)
    // — its presence verifies that base(2) was processed (without it, the
    // squashed base would leave no trace and we couldn't tell the
    // FirstOfNextBlock recovery actually carried into block 2).
    let raw = run_flashblock_sequence_without_peek(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("wire-hash"), 1),
            flash_base(2, hash("2a"), recomputed_block1_hash, ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed (flash_idx=0 collides with canonical).
            FireEvent::flash_block(1, hash("wire-hash"), 1, false),
            // is_final emitted by the FirstOfNextBlock fallback at idx =
            // latest_idx + 1 = 2 (printed 1002). The increment is needed
            // because the previous execute_flashblock for the delta stamped
            // the tracer's snapshot at `flash_index = 1`; matches geth's
            // CurrentIndex++ before the fallback executeAndValidateBlock call.
            FireEvent::flash_block(1, recomputed_block1_hash, 2, true),
            // Base(2) squashed; delta(2,1) emits — verifies base(2) accepted by
            // the FirstOfNextBlock fallback (broken in-flight-tip check used to
            // drop the new base here).
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// Prod-observed scenario: the FirstOfNextBlock recompute mismatched canonical
/// (block 46530249 → 46530250). Prior behavior reset state AND returned early,
/// dropping the new base; subsequent deltas for block N+1 hit the "no in-flight
/// sequence" guard and were also dropped — the wire stream broke for an entire
/// block until a much later restart point.
///
/// New behavior: on mismatch we reset the in-flight bundle (the previous block's
/// state proved divergent from canonical) but **fall through to `start_block`**
/// for the new base. `accumulated_db` is now `None`, so the execute path
/// re-bootstraps from canonical — or buffers the new block in `pending_state`
/// until the canonical notification for the parent arrives. Either way the
/// chain proceeds.
///
/// Fixture:
/// 1. Block 1 base + delta. Delta's `diff.block_hash = hash("wire-hash")` (the
///    sequencer's null-state-root reading, ≠ recompute).
/// 2. Block 2 base with `parent_hash = hash("not-the-recompute")` — a value
///    that differs from BOTH the recompute and the canonical we'll send below.
///    With the no-peek runner, the FirstOfNextBlock fallback fires for the
///    block-1→2 transition, recompute ≠ "not-the-recompute" → mismatch path.
/// 3. Block 2 delta accumulates while we wait for canonical(1) bootstrap.
/// 4. `canonical_block(1, hash("not-the-recompute"))` — canonical agrees with
///    block 2's `parent_hash`, so the pending-buffer replay (Flow 2 of
///    on_canonical_block) bootstraps and replays block 2's flashblocks.
///
/// Asserts the full recovery: block 2's flashblocks DO eventually reach the
/// wire, via the canonical-replay path. With the old "reset + return" the
/// output would have ended at block 1's delta with no block 2 emissions at all.
#[test]
fn next_base_mismatch_recovers_via_pending_and_canonical_replay() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let canonical_block1_hash = hash("not-the-recompute");
    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("wire-hash"), 1)];
    let recomputed_block1_hash = assembled_block_hash(&placeholder);
    // Sanity check: the canonical hash we use as parent_hash for block 2 must
    // differ from the recompute, otherwise the fallback would actually MATCH
    // and the test wouldn't exercise the recovery path.
    assert_ne!(canonical_block1_hash, recomputed_block1_hash);

    let raw = run_flashblock_sequence_without_peek(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("wire-hash"), 1),
            // Block 2's parent_hash ≠ recompute → FirstOfNextBlock fallback
            // mismatches. With the fix, this resets in-flight state and falls
            // through to start_block(2). Bootstrap from block 1 fails (block 1
            // not yet canonical) → block 2 enters pending_state.
            flash_base(2, hash("2a"), canonical_block1_hash, ts + 4),
            // Buffered alongside the base while pending.
            flash_delta(2, hash("2b"), 1),
            // Canonical for block 1 (with the same hash block 2's base
            // referenced as `parent_hash`) → on_canonical_block Flow 2 replays
            // the buffered block-2 flashblocks.
            canonical_block(1, canonical_block1_hash),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed; delta(1,1) emits at flash_idx=1.
            FireEvent::flash_block(1, hash("wire-hash"), 1, false),
            // No is_final for block 1 — the recompute mismatched canonical.
            // Block 2's base + delta were buffered while pending.
            // Canonical(1) fires the live-block tracer first.
            FireEvent::canonical_block(1, canonical_block1_hash),
            // Then Flow 2 replays block 2's buffered flashblocks via the merged
            // replay: ONE FIRE BLOCK stamped at the highest buffered index (1)
            // carrying all transactions from base + delta. Without the recovery
            // fix, neither this nor the per-flashblock emissions would have
            // reached the wire — the broken "reset + return" would have dropped
            // the new base at step 3.
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// Speculative state-root precompute (the latency optimisation for is_final).
///
/// Under a tokio runtime, the processor must spawn a background state_root
/// computation at the end of `execute_flashblock` once the index crosses the
/// `SPECULATIVE_STATE_ROOT_MIN_INDEX = 10` threshold. The test:
///
/// 1. Feeds 11 flashblocks for block 1 (base + deltas 1..=10).
/// 2. Asserts that `speculative_state_root_status_for_test()` returns a tracked
///    spec keyed by `(block=1, flashblock=10)`. Without the implementation, this
///    returns `None`.
/// 3. The runtime is held active long enough for `spawn_blocking` to complete,
///    so the spec's `completed` flag flips to `true` (the mock provider's
///    `state_root` is a no-op returning ZERO, but it still goes through the
///    spawn → return path).
#[test]
fn speculative_state_root_launched_at_threshold() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test tokio runtime");
    let _guard = runtime.enter();

    let mut events = vec![flash_base(1, hash("1a"), genesis_hash, ts + 2)];
    for idx in 1..=10u64 {
        events.push(flash_delta(1, hash("1a"), idx));
    }

    let result = run_flashblock_sequence_at_with_processor(client, events, ts + 2);

    let status = result
        .processor
        .speculative_state_root_status_for_test()
        .expect("speculative state-root must be tracked after idx >= threshold");
    assert_eq!(status.0, 1, "spec must be keyed to block 1");
    assert_eq!(status.1, 10, "spec must be keyed to the latest executed idx (10)");

    // Drive the runtime forward until the spec_blocking task has finished. The
    // mock provider returns ZERO immediately, so this should complete in a few
    // poll iterations.
    runtime.block_on(async {
        for _ in 0..100 {
            if result.processor.speculative_state_root_status_for_test().is_some_and(|s| s.3) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("speculative state-root task did not complete within deadline");
    });

    let status =
        result.processor.speculative_state_root_status_for_test().expect("spec still tracked");
    assert!(status.3, "spec result must be populated by the background task");
}

/// Regression for the merged-replay optimisation: a longer buffered chain
/// (base + 3 deltas, all signed transfers) must collapse into a single FIRE
/// BLOCK at the highest buffered index, carrying every transaction across
/// the buffered slice.
///
/// Sequence:
/// 1. base(2) + delta(2,1) + delta(2,2) + delta(2,3) — each delta carries one
///    signed transfer at nonces 0, 1, 2. Block 1 is not yet canonical, so all
///    four buffer in `pending_state = true`.
/// 2. `canonical_block(1, …)` triggers the merged replay: ONE FIRE BLOCK
///    stamped at idx 3 with `transaction_traces.len() == 3` (one per delta).
///
/// Without the merge, the canonical-replay path would emit 4 FIRE BLOCKs
/// (one per buffered flashblock).
#[test]
fn merged_replay_emits_single_fire_block_with_all_transactions() {
    let genesis = test_genesis();
    let _genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(2, hash("2a"), hash("1a"), ts + 4),
            flash_delta_with_txs(2, hash("2b"), 1, vec![signed_legacy_transfer(0)]),
            flash_delta_with_txs(2, hash("2c"), 2, vec![signed_legacy_transfer(1)]),
            flash_delta_with_txs(2, hash("2d"), 3, vec![signed_legacy_transfer(2)]),
            canonical_block(1, hash("1a")),
        ],
    );

    let flash_events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_eq!(
        flash_events.len(),
        1,
        "merged replay must emit ONE FIRE BLOCK regardless of buffered count; \
         got {} (probably reverted to per-flashblock replay)",
        flash_events.len()
    );

    let FireEvent::FlashBlock { flash_idx, ref block, .. } = flash_events[0] else {
        panic!("expected FlashBlock");
    };
    assert_eq!(flash_idx, 3, "merged replay stamps the highest buffered index");
    assert_eq!(
        block.transaction_traces.len(),
        3,
        "merged FIRE BLOCK must carry all transactions across the buffered slice"
    );
}

/// Regression for the prod-observed `flash_idx=0` emission on merged-replay
/// when only the base flashblock was buffered.
///
/// Sequence:
/// 1. base(2) arrives before canonical(1) → pending_state = true, stored = [base].
/// 2. canonical(1) arrives → Flow 2 (pending replay) fires. With the fix, the
///    "only base buffered" edge case is detected and skipped: no FIRE BLOCK is
///    emitted at flash_idx=0 (which would collide with canonical FIRE BLOCKs).
///    pending_state is cleared so subsequent events aren't deferred.
/// 3. delta(2,1) arrives. accumulated_db is None, so process_inner bootstraps
///    fresh from the now-canonical parent, gathers base+delta txs, and emits at
///    flash_idx=1 — proving the base's txs were preserved through the skip.
#[test]
fn merged_replay_skips_emission_when_only_base_buffered() {
    let genesis = test_genesis();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(2, hash("2a"), hash("1a"), ts + 4),
            // canonical(1) triggers merged-replay flow 2. Only the base is buffered
            // — the fix must skip emission here to avoid flash_idx=0.
            canonical_block(1, hash("1a")),
            // The delta arrives after the canonical replay; process_inner now
            // bootstraps fresh and emits at flash_idx=1 carrying base+delta txs.
            flash_delta(2, hash("2b"), 1),
        ],
    );

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Canonical FIRE BLOCK for block 1 (from the canonical tracer).
            FireEvent::canonical_block(1, hash("1a")),
            // No flash_idx=0 emission (the regression check).
            // delta(2,1) emits at flash_idx=1 carrying base+delta txs.
            FireEvent::flash_block(2, hash("2b"), 1, false),
        ],
    );
}

/// Regression for the prod-observed `parent header missing` reset bug.
///
/// State availability and header availability can diverge briefly: a payload
/// insert makes the parent's state queryable via `state_by_block_number_or_tag`,
/// but the canonical-chain commit (which writes the canonical header) lands
/// 100-150 ms later. A flashblock for block N+1 arriving in that window finds
/// the parent's state but not the parent's header, and the EVM env
/// construction errors out with `parent header missing`.
///
/// Old behavior: the outer `process` caught the error and called `state.reset()`,
/// dropping the buffered base. Subsequent indices for the same block then hit
/// the "no in-flight sequence" guard and were lost — the whole block disappeared
/// from the flashblock stream until a much later restart.
///
/// New behavior: `process_inner` does an explicit `header_by_number` pre-check
/// after a successful state bootstrap. On miss, mark the sequence pending so
/// the canonical-block notification replays it once the header lands.
///
/// Fixture:
/// 1. Mark canonical block 1 *state* available (via canonical event) but
///    deliberately suppress its *header* via `mark_header_unavailable`.
/// 2. Send base(2) + delta(2,1). The base is squashed; delta(2,1) triggers
///    `execute_flashblock` and would normally crash on the header lookup.
/// 3. With the fix, the new pre-check converts the miss into pending state
///    instead. Assert: no FIRE BLOCK emitted, in-flight block still tracked
///    (`current_block_number = Some(2)`), `pending_state = true`, and both
///    flashblocks are buffered (`stored_flashblocks.len() == 2`).
#[test]
fn parent_header_missing_buffers_instead_of_resetting() {
    let genesis = test_genesis();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    // Block 1's state is queryable (canonical signal arrived), but its header
    // hasn't yet landed (canonical-chain commit pending).
    client.mark_header_unavailable(1);

    let result = run_flashblock_sequence_at_with_processor(
        client,
        vec![
            canonical_block(1, hash("1a")),
            flash_base(2, hash("2a"), hash("1a"), ts + 4),
            flash_delta(2, hash("2b"), 1),
        ],
        ts + 4,
    );

    // No flashblock-tracer FIRE BLOCK is emitted — block 2 is pending.
    let flash_events: Vec<FireEvent> = parse_fire_events(&result.raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();
    assert!(
        flash_events.is_empty(),
        "no flashblock FIRE BLOCK should be emitted while the parent header is missing; got {} events",
        flash_events.len()
    );

    // The bug manifested as a state reset → `current_block_number = None` and
    // an empty `stored_flashblocks`. The fix keeps the in-flight block tracked
    // and both incoming flashblocks buffered.
    let (current_block, pending, stored_count) =
        result.processor.pending_state_for_test();
    assert_eq!(
        current_block,
        Some(2),
        "in-flight block must still be tracked (bug would reset to None)"
    );
    assert!(
        pending,
        "sequence must be marked pending so the canonical-block signal replays it"
    );
    assert_eq!(
        stored_count, 2,
        "both base(2) and delta(2,1) must remain buffered (bug would clear to 0)"
    );
}
