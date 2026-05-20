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
//! 3. Call [`framework::parse_fire_blocks`] to validate the emitted output.

mod framework;

use base_execution_chainspec::BaseChainSpec;

use framework::{
    flash_base, flash_delta, parse_fire_blocks, run_flashblock_sequence, test_genesis,
    GenesisClient,
};

/// Simplest possible test: send a single flash-base event (block 1, no transactions) and verify
/// that exactly one `FIRE BLOCK` line is emitted for block 1 with the correct block number and
/// flash index.
///
/// The base flashblock has raw index 0, so the printed `flash_idx` on the `FIRE BLOCK` line is
/// also 0 (non-final, no `+1000` offset). In the live system the flashblock and canonical tracers
/// are distinguished by their distinct `FIRE INIT` headers; in the test all output comes from the
/// dedicated buffer-backed flashblock tracer so any `FIRE BLOCK` line is guaranteed to be a
/// flashblock emission.
#[tokio::test]
async fn flash_base_emits_fire_block() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    // Block 1, parent = genesis block hash, timestamp = genesis + 2 seconds.
    let genesis_timestamp = 0x67d00000u64;
    let fb = flash_base(1, genesis_hash, genesis_timestamp + 2);

    let raw = run_flashblock_sequence(client, vec![fb]).await;

    let blocks = parse_fire_blocks(&raw);
    assert!(
        !blocks.is_empty(),
        "expected at least one FIRE BLOCK line; raw output:\n{}",
        String::from_utf8_lossy(&raw)
    );

    let first = &blocks[0];
    assert_eq!(first.block_number, 1, "expected FIRE BLOCK for block 1, got {}", first.block_number);

    // Base flashblock has raw index 0 passed to the tracer; is_final=false → printed_flash_idx=0.
    assert_eq!(
        first.printed_flash_idx,
        0,
        "expected printed_flash_idx 0 (base flashblock, non-final), got {}",
        first.printed_flash_idx,
    );

    // The base flashblock is not marked is_final (no +1000 sentinel).
    assert!(
        !first.is_final(),
        "base flashblock should not be marked is_final (no +1000 sentinel)"
    );
}

/// Sends a base flashblock followed by a delta for block 1 and verifies that two `FIRE BLOCK`
/// lines are emitted.
///
/// - Block 1, index 0 (base): printed `flash_idx` = 0.
/// - Block 1, index 1 (delta): printed `flash_idx` = 1 (`is_final=false` → no `+1000`).
///
/// The `flash_idx` field on the `ParsedFireBlock` helps callers distinguish canonical blocks
/// (value 0) from flashblock partials (value > 0 or value >= 1000 for final).
#[tokio::test]
async fn flash_base_plus_delta_emits_two_fire_blocks() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();

    let client = GenesisClient::new(genesis.clone());

    let genesis_timestamp = 0x67d00000u64;
    let base = flash_base(1, genesis_hash, genesis_timestamp + 2);
    let delta = flash_delta(1, 1);

    let raw = run_flashblock_sequence(client, vec![base, delta]).await;

    let blocks = parse_fire_blocks(&raw);
    assert!(
        blocks.len() >= 2,
        "expected at least two FIRE BLOCK lines; raw output:\n{}",
        String::from_utf8_lossy(&raw)
    );

    // First emission: base flashblock (index 0).
    let base_block = &blocks[0];
    assert_eq!(base_block.block_number, 1, "base block number mismatch");
    assert_eq!(base_block.printed_flash_idx, 0, "base flash_idx should be 0");
    assert_eq!(base_block.flash_idx(), None, "base is treated as non-flash (flash_idx 0 → None)");
    assert!(!base_block.is_final(), "base should not be marked is_final");

    // Second emission: delta flashblock (index 1).
    let delta_block = &blocks[1];
    assert_eq!(delta_block.block_number, 1, "delta block number mismatch");
    assert_eq!(delta_block.printed_flash_idx, 1, "delta flash_idx should be 1");
    assert_eq!(delta_block.flash_idx(), Some(1), "delta flash_idx should be Some(1)");
    assert!(!delta_block.is_final(), "delta without +1000 should not be marked is_final");
    // Verify the remaining ParsedFireBlock fields are well-formed.
    assert!(
        !delta_block.block_hash.is_empty(),
        "block_hash should be non-empty on delta FIRE BLOCK line"
    );
    assert!(
        !delta_block.prev_block_hash.is_empty(),
        "prev_block_hash should be non-empty on delta FIRE BLOCK line"
    );
    // prev_block_number for block 1 is 0 (parent is genesis).
    assert_eq!(delta_block.prev_block_number, 0, "prev_block_number should be 0 (genesis parent)");
    // lib_num is always <= block_number.
    assert!(
        delta_block.lib_num <= delta_block.block_number,
        "lib_num should not exceed block_number"
    );
    // timestamp_ns is non-zero (derived from the header timestamp).
    assert!(delta_block.timestamp_ns > 0, "timestamp_ns should be non-zero");
}
