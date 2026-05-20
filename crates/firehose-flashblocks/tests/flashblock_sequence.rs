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
//! 4. Use [`framework::assert_fire_events_eq`] with expected events built from the constructors
//!    on [`framework::FireEvent`] for clean equality-based assertions.

mod framework;

use base_execution_chainspec::BaseChainSpec;

use framework::{
    FireEvent, assert_fire_events_eq, flash_base, flash_delta, parse_fire_events,
    run_flashblock_sequence, test_genesis, GenesisClient,
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

    assert_fire_events_eq(
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

    assert_fire_events_eq(
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

    assert_fire_events_eq(
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
    assert_fire_events_eq(
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
    assert_fire_events_eq(
        &events,
        &[FireEvent::canonical_block(1)],
    );
}
