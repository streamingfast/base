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
    flash_delta_with_payload_id, hash, parse_fire_events, run_flashblock_sequence,
    run_flashblock_sequence_at, run_flashblock_sequence_at_with_processor, test_genesis,
};

/// Simplest possible test: send a single flash-base event (block 1, no transactions) and verify
/// that exactly one `FIRE BLOCK` line is emitted for block 1 with the correct block number.
///
/// The base flashblock has raw index 0, so the printed `flash_idx` slot on the `FIRE BLOCK`
/// line is also 0. At the FIRE wire level a `flash_idx == 0` line is ambiguous between a
/// canonical block and a base flashblock; in live operation downstream consumers disambiguate
/// via the per-stream `FIRE INIT` header. The test harness preserves this distinction by
/// tagging each event's tracer output with a `# SOURCE FLASH` / `# SOURCE CANON` marker, so
/// flashblock-tracer lines (any `flash_idx`, including 0) map to [`FireEvent::FlashBlock`] and
/// only canonical-tracer lines map to [`FireEvent::Block`].
#[test]
fn flash_base_emits_fire_block() {
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

    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
}

/// Sends a base flashblock then one delta for the same block number and verifies that two
/// `FIRE BLOCK` lines are emitted with consecutive flash indices.
///
/// Both lines come from the flashblock tracer, so both parse as [`FireEvent::FlashBlock`]:
/// - Block 1, index 0 (base): `flash_idx = 0`.
/// - Block 1, index 1 (delta): `flash_idx = 1`.
#[test]
fn base_plus_delta_emits_two_fire_blocks() {
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
            // Base flashblock (index 0) — flash tracer, flash_idx=0.
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // Delta flashblock (index 1) — flash tracer, flash_idx=1.
            FireEvent::flash_block(1, hash("1a"), 1, false),
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
        // Only the base for block 1 lands. delta(1,1) was dropped by the peek-driven
        // is_final mismatch (geth-equivalent: OnBlockEnd(err) discards the block at
        // the wire layer); block 2 never executes.
        &[FireEvent::flash_block(1, hash("1a"), 0, false)],
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // Peek-driven single-emission is_final: peek saw block 2's base, so
            // delta(1,1) emits ONCE with the recomputed hash + is_final marker
            // (printed as 1001).
            FireEvent::flash_block(1, block1_recomputed, 1, true),
            FireEvent::flash_block(2, hash("2a"), 0, false),
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

    // Only one FIRE BLOCK must be emitted; the duplicate is silently dropped.
    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
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

    // Only the base FIRE BLOCK is emitted; the gap delta is dropped.
    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // delta(1,1) squashed; only the consolidated emission for idx=2 lands.
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

    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // delta(1,1) and delta(1,2) squashed; only the consolidated emission at idx=3.
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // Peek caught block 2's base → delta(1,1) emits once as is_final.
            FireEvent::flash_block(1, block1_recomputed, 1, true),
            FireEvent::flash_block(2, hash("2a"), 0, false),
            FireEvent::flash_block(2, hash("2a"), 1, false),
        ],
    );
}

/// Verifies that the decoded block payload is non-empty and has the correct block number.
///
/// Sends a single base flashblock for block 1. First asserts that the decoded
/// `sf.ethereum.type.v2.Block` protobuf carries `number == 1` and a non-empty `hash`
/// (confirming the tracer encoded the block correctly and [`parse_fire_events`] decoded it).
/// Then uses [`assert_fire_events_eq`] (full comparison including payload) to verify
/// that the event roundtrips through the framework helpers correctly.
#[test]
fn block_payload_has_correct_block_number() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let raw =
        run_flashblock_sequence(client, vec![flash_base(1, hash("1a"), genesis_hash, ts + 2)]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_eq!(events.len(), 1, "expected exactly one FlashBlock event");

    // Extract the decoded EthBlock and verify key fields directly.
    let FireEvent::FlashBlock { block_number, block: ref eth_block, .. } = events[0] else {
        panic!("expected FireEvent::FlashBlock, got {:?}", events[0]);
    };
    assert_eq!(block_number, 1, "block_number metadata must be 1");
    assert_eq!(eth_block.number, 1, "decoded EthBlock.number must be 1");
    assert!(!eth_block.hash.is_empty(), "decoded EthBlock.hash must be non-empty");

    // Use assert_fire_events_eq for a full payload roundtrip check: build an expected event
    // using the actual decoded block so that the comparison verifies the EthBlock survives
    // the parse/clone cycle intact.
    let expected = vec![FireEvent::FlashBlock {
        block_number: 1,
        block_hash: hash("1a"),
        flash_idx: 0,
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
    let block1_recomputed = assembled_block_hash(&[flash_base(1, hash("1a"), genesis_hash, ts + 2)]);

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            canonical_block(1, block1_recomputed),
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
            // Peek for flash_base(1) skips the canonical event and sees flash_base(2);
            // block 2's base is for block N+1, so flash_base(1) is treated as the
            // final partial for block 1. Single FIRE BLOCK sealed with the recomputed
            // hash and is_final marker (printed 1000).
            FireEvent::flash_block(1, block1_recomputed, 0, true),
            FireEvent::canonical_block(1, block1_recomputed), // Canonical1 — canonical tracer
            FireEvent::flash_block(2, hash("2a"), 0, false),  // Flash2 base — flashblock tracer
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
            FireEvent::flash_block(2, hash("2a"), 0, false), // Flash2 — flashblock tracer, flash_idx=0
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
            FireEvent::flash_block(1, hash("1a"), 0, false), // Flash1 — flashblock tracer, block 1, flash_idx=0
            FireEvent::canonical_block(1, hash("1a")), // Canonical1 — canonical tracer, block 1
            FireEvent::canonical_block(2, hash("2a")), // Canonical2 — canonical tracer, block 2
            FireEvent::flash_block(3, hash("3a"), 0, false), // Flash3 — flashblock tracer, block 3, flash_idx=0
            FireEvent::flash_block(3, hash("3b"), 1, false), // Flash3 — flashblock tracer, block 3, flash_idx=1
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
            FireEvent::flash_block(1, hash("1a"), 0, false), // Flash1 — flashblock tracer, block 1, flash_idx=0
            FireEvent::canonical_block(1, hash("1a")), // Canonical1 — canonical tracer, block 1
            FireEvent::canonical_block(2, hash("2a")), // Canonical2 — canonical tracer, block 2
            FireEvent::flash_block(3, hash("3a"), 0, false), // Flash3 — flashblock tracer, block 3, flash_idx=0
            FireEvent::flash_block(3, hash("3b"), 1, false), // Flash3 — flashblock tracer, block 3, flash_idx=1
            FireEvent::canonical_block(3, hash("3a")), // Canonical3 — canonical tracer, block 3
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

    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
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

    let block1_fbs = vec![
        flash_base(1, hash("1a"), genesis_hash, ts + 2),
        flash_delta(1, expected_block1_hash, 1),
    ];

    let mut events = block1_fbs;
    events.push(flash_base(2, hash("2a"), expected_block1_hash, ts + 4));

    let raw = run_flashblock_sequence(client, events);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // Peek-driven single-emission is_final: only ONE FIRE BLOCK lands for
            // delta(1,1), sealed with the recomputed hash and stamped is_final (no
            // separate non-final partial). Matches geth's `SetFinalFlashBlock` +
            // `OnBlockEnd` single-flush pattern.
            FireEvent::flash_block(1, expected_block1_hash, 1, true),
            FireEvent::flash_block(2, hash("2a"), 0, false),
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // 1, 2, 3 all squashed; only the consolidated idx=4 emission lands.
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

    let raw = run_flashblock_sequence(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, block1_recomputed, 1),
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // delta(1,1) executes — peek shows block 2's base, NOT a same-block
            // delta, so squash does not apply. Instead the peek classifies as
            // is_final (next-block base) and delta(1,1) emits ONCE with the
            // recomputed hash + is_final marker (printed 1001).
            FireEvent::flash_block(1, block1_recomputed, 1, true),
            FireEvent::flash_block(2, hash("2a"), 0, false),
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
            FireEvent::flash_block(1, hash("1a"), 0, false),
            // delta(1,1) dropped (peek-driven is_final mismatch → mark_failed).
            // block 2's base and delta both buffered + later discarded → no FIRE
            // BLOCKs for them.
            FireEvent::canonical_block(1, hash("wrong-1")),
        ],
    );
}

/// Asserts that the firehose tracer's flashblock snapshot mechanism is wired up:
/// each emitted FIRE BLOCK at index K must carry the **cumulative** trace from
/// indices `0..=K`, not just the contributions new to iteration K. Mirrors geth's
/// `firehoseTracer.SnapshotFlashBlockForNextIteration()` call before the early
/// return on `!isLastFlashBlock` at `eth/tracers/firehose.go:180`.
///
/// Sequence + reasoning:
/// 1. `flash_base(2, …)` and `flash_delta(2, 1)` arrive before block 1 is
///    canonical → buffered.
/// 2. `canonical_block(1, …)` triggers the replay path, which calls
///    `execute_flashblock` **once per buffered flashblock** in separate EVM
///    invocations. This is the codepath where the snapshot mechanism matters
///    most: each iteration's `on_block_start` clears the in-progress block, so
///    without the snapshot the second emission would lose the first's tracer
///    contributions entirely.
/// 3. The base flashblock executes pre-execution changes (EIP-4788 beacon-roots
///    write) which the firehose tracer records as a `system_call`. The delta
///    has no further pre-execution changes (only the first execution per block
///    fires them), so any `system_calls` count > 0 on the delta's FIRE BLOCK
///    can only have come from the snapshot/restore round-trip.
///
/// Asserts:
/// - The base's FIRE BLOCK carries `system_calls.len() == 1` (the EIP-4788 write).
/// - The delta's FIRE BLOCK also carries `system_calls.len() == 1` (the base's
///   system_call, restored via `snapshot_flash_block_for_next_iteration` →
///   `restore_flash_block_snapshot`). Without the snapshot call, this would be 0.
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
            // canonical(1) triggers the replay path — base and delta execute
            // through `execute_flashblock` in separate iterations (NOT squashed
            // into one EVM call), which is the case where the snapshot mechanism
            // is required to preserve cumulative trace state.
            canonical_block(1, hash("1a")),
        ],
    );

    let flash_events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_eq!(flash_events.len(), 2, "expected 2 FIRE BLOCK lines for block 2 (base + delta)");

    let FireEvent::FlashBlock { flash_idx: base_idx, block: ref base_block, .. } = flash_events[0]
    else {
        panic!("expected FlashBlock");
    };
    assert_eq!(base_idx, 0, "first replayed flashblock is the base (idx 0)");
    assert_eq!(
        base_block.system_calls.len(),
        1,
        "base flashblock must carry the EIP-4788 beacon-roots pre-execution system call"
    );

    let FireEvent::FlashBlock { flash_idx: delta_idx, block: ref delta_block, .. } =
        flash_events[1]
    else {
        panic!("expected FlashBlock");
    };
    assert_eq!(delta_idx, 1, "second replayed flashblock is the delta (idx 1)");
    // This is the regression assertion. Without
    // `snapshot_flash_block_for_next_iteration` between iterations, the delta's
    // FIRE BLOCK would carry `system_calls.len() == 0` because `on_block_start`
    // clears the in-progress block at the start of each iteration. The snapshot
    // is what restores the base's system_call into the delta's emission.
    assert_eq!(
        delta_block.system_calls.len(),
        1,
        "delta FIRE BLOCK must carry the cumulative system_calls from base — \
         without snapshot_flash_block_for_next_iteration this is 0 and the wire \
         stream diverges from the geth flashblock contract"
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

    // Replay emitted the buffered base and delta.
    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1, hash("1a")),
            FireEvent::flash_block(2, hash("2a"), 0, false),
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
