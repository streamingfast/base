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
    FireEvent, GenesisClient, assert_fire_events_eq, assert_fire_events_metadata_eq,
    canonical_block, flash_base, flash_delta, hash, parse_fire_events, run_flashblock_sequence,
    test_genesis,
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

    // Block 1, parent = genesis block hash, timestamp = genesis + 2 seconds.
    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![base1]);

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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, hash("1a"), 1);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1]);

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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, hash("1a"), 1);
    // Block 2's parent hash is B256::ZERO in tests (the mock provider always returns genesis header
    // which has a zero block hash, so any non-zero B256 is fine for routing; use genesis_hash for
    // simplicity — the mock ignores the parent hash when looking up state).
    let base2 = flash_base(2, hash("2a"), genesis_hash, genesis_timestamp + 4);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, base2]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 0, false),
            FireEvent::flash_block(1, hash("1a"), 1, false),
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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    // Send the same base twice.
    let raw = run_flashblock_sequence(client, vec![base1.clone(), base1]);

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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    // Skip index 1 and send index 2 directly — creates a NonSequentialGap.
    let delta1_2 = flash_delta(1, hash("1a"), 2);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_2]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Only the base FIRE BLOCK is emitted; the gap delta is dropped.
    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
}

/// Sends a base + two successive deltas (idx=1, idx=2) for the same block.
///
/// Asserts three [`FireEvent::FlashBlock`] events: `idx=0` (base), `idx=1`, `idx=2`.
/// Tests that consecutive deltas on the same block are all processed and emitted in order.
#[test]
fn two_successive_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis);

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, hash("1a"), 1);
    let delta1_2 = flash_delta(1, hash("1a"), 2);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, delta1_2]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 0, false),
            FireEvent::flash_block(1, hash("1a"), 1, false),
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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    // Jump directly to idx=2, skipping idx=1.
    let delta1_2 = flash_delta(1, hash("1a"), 2);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_2]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(&events, &[FireEvent::flash_block(1, hash("1a"), 0, false)]);
}

/// Sends a base + three successive deltas (idx=1, idx=2, idx=3) for the same block.
///
/// Asserts four events in total, verifying that the processor correctly sequences three
/// consecutive delta flashblocks and emits a `FIRE BLOCK` for each.
#[test]
fn three_successive_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis);

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, hash("1a"), 1);
    let delta1_2 = flash_delta(1, hash("1a"), 2);
    let delta1_3 = flash_delta(1, hash("1a"), 3);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, delta1_2, delta1_3]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 0, false),
            FireEvent::flash_block(1, hash("1a"), 1, false),
            FireEvent::flash_block(1, hash("1a"), 2, false),
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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, hash("1a"), 1);
    let base2 = flash_base(2, hash("2a"), genesis_hash, genesis_timestamp + 4);
    let delta2_1 = flash_delta(2, hash("2a"), 1);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, base2, delta2_1]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 0, false),
            FireEvent::flash_block(1, hash("1a"), 1, false),
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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![base1]);

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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    let base2 = flash_base(2, hash("2a"), genesis_hash, genesis_timestamp + 4);

    let raw = run_flashblock_sequence(client, vec![base1, canonical_block(1, hash("1a")), base2]);

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::flash_block(1, hash("1a"), 0, false), // Flash1 — flashblock tracer, flash_idx=0
            FireEvent::canonical_block(1, hash("1a")),       // Canonical1 — canonical tracer
            FireEvent::flash_block(2, hash("2a"), 0, false), // Flash2 — flashblock tracer, flash_idx=0
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
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis);

    let genesis_timestamp = 0x67d00000u64;
    // Send base for block 2 as the very first flashblock (no block 1 context).
    // The processor has no accumulated_db, so it must bootstrap from the provider at block 1.
    let base2 = flash_base(2, hash("2a"), genesis_hash, genesis_timestamp + 4);

    let raw = run_flashblock_sequence(client, vec![canonical_block(1, hash("1a")), base2]);

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

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, hash("1a"), genesis_hash, genesis_timestamp + 2);
    // No flashblock for block 2 — only canonical events for blocks 1 and 2.
    let base3 = flash_base(3, hash("3a"), hash("2b"), genesis_timestamp + 6);
    let flash3 = flash_delta(3, hash("3b"), 1);

    let raw = run_flashblock_sequence(
        client,
        vec![base1, canonical_block(1, hash("1a")), canonical_block(2, hash("2a")), base3, flash3],
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
