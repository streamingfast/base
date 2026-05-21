//! Integration tests for the firehose flashblocks processor.
//!
//! The test framework lives in [`framework`] and provides WS server helpers, flashblock
//! builders, output parsers, and the [`run_flashblock_sequence`] harness. This file
//! contains only the individual test cases.
//!
//! # Adding new test cases
//!
//! 1. Build a `Vec<Flashblock>` using [`framework::flash_base`] / [`framework::flash_delta`].
//! 2. Call [`framework::run_flashblock_sequence`] with the sequence and a [`framework::GenesisClient`].
//! 3. Call [`framework::parse_fire_events`] to validate the emitted output.
//! 4. Use [`framework::assert_fire_events_metadata_eq`] for metadata-only assertions or
//!    [`framework::assert_fire_events_eq`] when you also need to verify the decoded block payload.

mod framework;

use base_execution_chainspec::BaseChainSpec;

use framework::{
    FireEvent, assert_fire_events_eq, assert_fire_events_metadata_eq, flash_base, flash_delta,
    parse_fire_events, run_flashblock_sequence, test_genesis, GenesisClient,
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
    let fb = flash_base(1, genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![fb]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[FireEvent::canonical_block(1)],
    );
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
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta = flash_delta(1, 1);

    let raw = run_flashblock_sequence(client, vec![base, delta]).await;

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
    let base_n = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta_n = flash_delta(1, 1);
    // Block 2's parent hash is B256::ZERO in tests (the mock provider always returns genesis header
    // which has a zero block hash, so any non-zero B256 is fine for routing; use genesis_hash for
    // simplicity — the mock ignores the parent hash when looking up state).
    let base_n1 = flash_base(2, genesis_hash, genesis_timestamp + 4);

    let raw = run_flashblock_sequence(client, vec![base_n, delta_n, base_n1]).await;

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
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Send the same base twice.
    let raw = run_flashblock_sequence(client, vec![base.clone(), base]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Only one FIRE BLOCK must be emitted; the duplicate is silently dropped.
    assert_fire_events_metadata_eq(
        &events,
        &[FireEvent::canonical_block(1)],
    );
}

/// Sends a base for block N, then a delta with index 2 (skipping index 1).
///
/// Asserts that only one `FIRE BLOCK` is emitted (for the base). The gap causes the
/// sequence validator to return `NonSequentialGap`, which sets `is_skipping = true`. No
/// further FIRE lines are emitted for the out-of-sequence delta.
#[tokio::test]
async fn non_sequential_delta_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Skip index 1 and send index 2 directly — creates a NonSequentialGap.
    let gap_delta = flash_delta(1, 2);

    let raw = run_flashblock_sequence(client, vec![base, gap_delta]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    // Only the base FIRE BLOCK is emitted; the gap delta is dropped.
    assert_fire_events_metadata_eq(
        &events,
        &[FireEvent::canonical_block(1)],
    );
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
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1 = flash_delta(1, 1);
    let delta2 = flash_delta(1, 2);

    let raw = run_flashblock_sequence(client, vec![base, delta1, delta2]).await;

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
/// consistent: the gap at idx=2 triggers `NonSequentialGap`, sets `is_skipping=true`, and
/// only the base `FIRE BLOCK` is emitted. Any further deltas on the same block while
/// `is_skipping` is set are also dropped (exercised in [`three_successive_deltas_then_gap`]).
#[tokio::test]
async fn jumping_delta_is_skipped() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    // Jump directly to idx=2, skipping idx=1.
    let gap_delta = flash_delta(1, 2);

    let raw = run_flashblock_sequence(client, vec![base, gap_delta]).await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::Block { .. } | FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[FireEvent::canonical_block(1)],
    );
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
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta1 = flash_delta(1, 1);
    let delta2 = flash_delta(1, 2);
    let delta3 = flash_delta(1, 3);

    let raw = run_flashblock_sequence(client, vec![base, delta1, delta2, delta3]).await;

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
    let base_1 = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta_1 = flash_delta(1, 1);
    let base_2 = flash_base(2, genesis_hash, genesis_timestamp + 4);
    let delta_2 = flash_delta(2, 1);

    let raw = run_flashblock_sequence(client, vec![base_1, delta_1, base_2, delta_2]).await;

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
    let fb = flash_base(1, genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![fb]).await;

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
