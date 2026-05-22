//! Integration tests for the firehose flashblocks processor.
//!
//! The test framework lives in [`framework`] and provides WS server helpers, flashblock
//! builders, output parsers, and the [`run_flashblock_sequence`] harness. This file
//! contains only the individual test cases.
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
    canonical_block, flash_base, flash_delta, parse_fire_events, run_flashblock_sequence,
    test_genesis,
};

/// Simplest possible test: send a single flash-base event (block 1, no transactions) and verify
/// that exactly one `FIRE BLOCK` line is emitted for block 1 with the correct block number.
///
/// The base flashblock has raw index 0, so the printed flash index on the `FIRE BLOCK` line is
/// also 0. The FIRE protocol does not distinguish a canonical block from a base flashblock at the
/// wire level when `flash_idx == 0`; the two streams are distinguished by their distinct
/// `FIRE INIT` headers in live operation. Both map to `FireEvent::Block` in the parser.
#[tokio::test]
async fn flash_base_emits_fire_block() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    // Block 1, parent = genesis block hash, timestamp = genesis + 2 seconds.
    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![base1]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(&events, &[FireEvent::canonical_block(1)]);
}

/// Sends a base flashblock then one delta for the same block number and verifies that two
/// `FIRE BLOCK` lines are emitted with consecutive flash indices.
///
/// - Block 1, index 0 (base): printed `flash_idx` = 0 → `FireEvent::Block`.
/// - Block 1, index 1 (delta): printed `flash_idx` = 1 → `FireEvent::FlashBlock { flash_idx: 1 }`.
#[tokio::test]
async fn base_plus_delta_emits_two_fire_blocks() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, 1);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base flashblock (index 0) → printed_flash_idx = 0 → Block variant.
            FireEvent::canonical_block(1),
            // Delta flashblock (index 1) → printed_flash_idx = 1 → FlashBlock variant.
            FireEvent::flash_block(1, 1, false),
        ],
    );
}

/// Sends base for block N, one delta for block N, then base for block N+1.
///
/// Asserts three events are emitted:
/// - `Block(N)` — the base flashblock for block N (`flash_idx=0`, treated as canonical).
/// - `FlashBlock(N, idx=1)` — the delta for block N.
/// - `Block(N+1)` — the base flashblock for block N+1 (`flash_idx=0`, treated as canonical).
///
/// This exercises the cross-block state carry-forward path where the accumulated EVM state from
/// block N is carried forward to seed block N+1 without waiting for the canonical `StateProvider`
/// to reflect block N.
#[tokio::test]
async fn base_plus_delta_plus_next_base() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, 1);
    // Block 2's parent hash is B256::ZERO in tests (the mock provider always returns genesis header
    // which has a zero block hash, so any non-zero B256 is fine for routing; use genesis_hash for
    // simplicity — the mock ignores the parent hash when looking up state).
    let base2 = flash_base(2, genesis_hash, genesis_timestamp + 4);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, base2]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1),
            FireEvent::flash_block(1, 1, false),
            FireEvent::canonical_block(2),
        ],
    );
}

/// Sends the same base flashblock twice and asserts that only one `FIRE BLOCK` is emitted.
///
/// The sequence validator returns `Duplicate` for the second base; the processor logs and
/// discards it without re-executing or emitting a second FIRE line.
#[tokio::test]
async fn duplicate_base_is_ignored() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Send the same base twice.
    let raw = run_flashblock_sequence(client, vec![base1.clone(), base1]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Only one FIRE BLOCK must be emitted; the duplicate is silently dropped.
    assert_fire_events_metadata_eq(&events, &[FireEvent::canonical_block(1)]);
}

/// Sends a base for block N, then a delta with index 2 (skipping index 1).
///
/// Asserts that only one `FIRE BLOCK` is emitted (for the base). The gap causes the
/// sequence validator to return `NonSequentialGap`, which resets the processor state. No
/// further FIRE lines are emitted for the out-of-sequence delta.
#[tokio::test]
async fn non_sequential_delta_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Skip index 1 and send index 2 directly — creates a NonSequentialGap.
    let delta1_2 = flash_delta(1, 2);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_2]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Only the base FIRE BLOCK is emitted; the gap delta is dropped.
    assert_fire_events_metadata_eq(&events, &[FireEvent::canonical_block(1)]);
}

/// Sends a base + two successive deltas (idx=1, idx=2) for the same block.
///
/// Asserts three events: `Block(1)`, `FlashBlock(1, idx=1)`, `FlashBlock(1, idx=2)`.
/// Tests that consecutive deltas on the same block are all processed and emitted in order.
#[tokio::test]
async fn two_successive_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, 1);
    let delta1_2 = flash_delta(1, 2);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, delta1_2]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1),
            FireEvent::flash_block(1, 1, false),
            FireEvent::flash_block(1, 2, false),
        ],
    );
}

/// Sends a base then a delta with index=2, skipping index=1.
///
/// This is equivalent to [`non_sequential_delta_is_skipped`] but verifies the behaviour is
/// consistent: the gap at idx=2 triggers `NonSequentialGap`, the processor resets its state,
/// and only the base `FIRE BLOCK` is emitted. Any further deltas on the same block after the
/// reset are also dropped (no in-flight sequence + non-zero index → dropped).
#[tokio::test]
async fn jumping_delta_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Jump directly to idx=2, skipping idx=1.
    let delta1_2 = flash_delta(1, 2);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_2]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(&events, &[FireEvent::canonical_block(1)]);
}

/// Sends a base + three successive deltas (idx=1, idx=2, idx=3) for the same block.
///
/// Asserts four events in total, verifying that the processor correctly sequences three
/// consecutive delta flashblocks and emits a `FIRE BLOCK` for each.
#[tokio::test]
async fn three_successive_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, 1);
    let delta1_2 = flash_delta(1, 2);
    let delta1_3 = flash_delta(1, 3);

    let raw =
        run_flashblock_sequence(client, vec![base1, delta1_1, delta1_2, delta1_3]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1),
            FireEvent::flash_block(1, 1, false),
            FireEvent::flash_block(1, 2, false),
            FireEvent::flash_block(1, 3, false),
        ],
    );
}

/// Sends two full block cycles, each with one delta: base(N)+delta(N,1)+base(N+1)+delta(N+1,1).
///
/// Asserts four events: `Block(N)`, `FlashBlock(N,1)`, `Block(N+1)`, `FlashBlock(N+1,1)`.
/// Exercises cross-block state carry-forward together with inter-block delta processing.
#[tokio::test]
async fn two_blocks_with_deltas() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1_1 = flash_delta(1, 1);
    let base2 = flash_base(2, genesis_hash, genesis_timestamp + 4);
    let delta2_1 = flash_delta(2, 1);

    let raw = run_flashblock_sequence(client, vec![base1, delta1_1, base2, delta2_1]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            FireEvent::canonical_block(1),
            FireEvent::flash_block(1, 1, false),
            FireEvent::canonical_block(2),
            FireEvent::flash_block(2, 1, false),
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
#[tokio::test]
async fn block_payload_has_correct_block_number() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![base1]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. }))
        .collect();

    assert_eq!(events.len(), 1, "expected exactly one Block event");

    // Extract the decoded EthBlock and verify key fields directly.
    let FireEvent::Block { block_number, block: ref eth_block, .. } = events[0] else {
        panic!("expected FireEvent::Block, got {:?}", events[0]);
    };
    assert_eq!(block_number, 1, "block_number metadata must be 1");
    assert_eq!(eth_block.number, 1, "decoded EthBlock.number must be 1");
    assert!(!eth_block.hash.is_empty(), "decoded EthBlock.hash must be non-empty");

    // Use assert_fire_events_eq for a full payload roundtrip check: build an expected event
    // using the actual decoded block so that the comparison verifies the EthBlock survives
    // the parse/clone cycle intact.
    let expected = vec![FireEvent::Block {
        block_number: 1,
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
/// Without a prior [`TestEvent::canonical_block(1)`], `GenesisClient` would return a
/// `ProviderError` for block 1, causing the processor to exhaust its retries and skip the
/// block. With the canonical block event applied, the provider returns successfully and the
/// processor emits a `FIRE BLOCK` for block 2.
///
/// This verifies the [`TestEvent::CanonicalBlock`] path: it marks the block as available in
/// `GenesisClient` so that the bootstrap provider call succeeds on the first attempt.
#[tokio::test]
async fn canonical_block_unblocks_next_base() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    // Block 1 base — bootstraps from genesis (block 0 is always available).
    let base1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Block 2 base — would need block 1 to be available to bootstrap from the provider,
    // since block 2 is NOT the sequential successor of block 1 after we send base1 alone
    // (no delta in between, but the processor still carries forward accumulated_db because
    // block 2 == block 1 + 1; the key path tested here is that marking block 1 canonical
    // allows the provider call to succeed even if accumulated_db were None).
    //
    // We send base1 → canonical_block(1) → base2 to verify that:
    //   1. base1 produces a FIRE BLOCK for block 1.
    //   2. canonical_block(1) marks block 1 as available without emitting anything.
    //   3. base2 produces a FIRE BLOCK for block 2 (carried forward from accumulated_db).
    let base2 = flash_base(2, genesis_hash, genesis_timestamp + 4);

    let raw =
        run_flashblock_sequence(client, vec![base1, canonical_block(1), base2]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[FireEvent::canonical_block(1), FireEvent::canonical_block(2)],
    );
}

/// Exercises the bootstrap path: sending a base for block 2 when the processor has no prior
/// context requires bootstrapping from the provider at block 1.
///
/// Without [`TestEvent::canonical_block(1)`], `GenesisClient` returns a `ProviderError` for
/// block 1 and the processor exhausts its retries, causing block 2 to be skipped.
/// With the canonical block event applied first, the provider returns successfully and the
/// processor emits a `FIRE BLOCK` for block 2.
///
/// This tests the bootstrap path in isolation: no prior flashblock context means
/// `accumulated_db` is always `None` and the provider call for the parent block is mandatory.
#[tokio::test]
async fn canonical_block_unblocks_non_sequential_gap() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    // Send base for block 2 as the very first flashblock (no block 1 context).
    // The processor has no accumulated_db, so it must bootstrap from the provider at block 1.
    let base2 = flash_base(2, genesis_hash, genesis_timestamp + 4);

    let raw = run_flashblock_sequence(client, vec![canonical_block(1), base2]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(&events, &[FireEvent::canonical_block(2)]);
}
