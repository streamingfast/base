//! Regression tests for the single serialized command queue
//! ([`base_firehose_flashblocks::FirehoseFlashblocksDispatcher`]).
//!
//! In production the processor's in-flight state is fed by three independent tasks: the WebSocket
//! flashblock stream and the two canonical-block signal sources (the early in-engine notification
//! and the post-commit canonical-state broadcast). Previously each called the processor directly,
//! serialized only by an internal mutex that `process_inner` releases across EVM execution — so a
//! canonical signal could mutate or reset the very state being executed against, corrupting
//! `accumulated_db` (wrong state roots) and emitting duplicate `is_final` FIRE BLOCKs.
//!
//! The fix funnels all three sources through one queue drained by a single consumer task. These
//! tests drive that real queue path via [`framework::run_flashblock_sequence_via_dispatcher`].

mod framework;

use base_execution_chainspec::BaseChainSpec;

use framework::{
    FireEvent, GenesisClient, assembled_block_hash, assert_fire_events_metadata_eq, canonical_block,
    flash_base, flash_delta, hash, parse_fire_events, run_flashblock_sequence_via_dispatcher,
    test_genesis,
};

/// Two canonical signals for the same block — exactly what the early in-engine notification and
/// the post-commit canonical-state broadcast deliver — must produce a **single** `is_final` FIRE
/// BLOCK. Under the old concurrent wiring the two `on_canonical_block` calls could race and
/// double-emit (the `FirstOfNextBlock` fallback never set `final_part_sent`); routed through the
/// serialized queue they are applied in order, so the first emits is_final and sets
/// `final_part_sent`, and the second hits the "already-finalized" no-op branch.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_canonical_signals_emit_single_is_final() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let placeholder =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("any"), 1)];
    let recomputed_block1_hash = assembled_block_hash(&placeholder);

    let raw = run_flashblock_sequence_via_dispatcher(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("wire-hash"), 1),
            // Early in-engine canonical signal for block 1.
            canonical_block(1, recomputed_block1_hash),
            // Post-commit canonical-state broadcast for the SAME block — a duplicate.
            canonical_block(1, recomputed_block1_hash),
        ],
        ts,
    )
    .await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Base(1) squashed; delta(1,1) emits as non-final, carrying base+delta txs.
            FireEvent::flash_block(1, hash("wire-hash"), 1, false),
            // First canonical signal recomputes block 1's hash and emits is_final (idx 1002).
            FireEvent::flash_block(1, recomputed_block1_hash, 2, true),
            // Second (duplicate) canonical signal: no further emission.
        ],
    );

    let is_final_count =
        events.iter().filter(|e| matches!(e, FireEvent::FlashBlock { is_final: true, .. })).count();
    assert_eq!(is_final_count, 1, "duplicate canonical signals must emit exactly one is_final");
}

/// A canonical(N) signal enqueued between block N's finalization and block N+1's base must apply
/// in strict arrival order as a harmless no-op, and block N+1 must bootstrap on the carried-forward
/// state and emit normally — proving the extra canonical command neither reset nor corrupted the
/// in-flight state. Here block 1 is finalized by the peek-driven path (block 2's base is already
/// queued, so the WS peek catches the transition); the interleaved canonical(1) then lands on the
/// already-finalized block and is dropped by the `final_part_sent` guard.
#[tokio::test(flavor = "multi_thread")]
async fn canonical_interleaved_with_deltas_preserves_next_block() {
    let genesis = test_genesis();
    let genesis_hash = BaseChainSpec::from_genesis(genesis.clone()).inner.genesis_hash();
    let client = GenesisClient::new(genesis);
    let ts = 0x67d00000u64;

    let block1 =
        vec![flash_base(1, hash("1a"), genesis_hash, ts + 2), flash_delta(1, hash("w1"), 1)];
    let recomputed_block1_hash = assembled_block_hash(&block1);

    let raw = run_flashblock_sequence_via_dispatcher(
        client,
        vec![
            flash_base(1, hash("1a"), genesis_hash, ts + 2),
            flash_delta(1, hash("w1"), 1),
            // Early canonical(1) arriving before block 2's base — finalizes block 1.
            canonical_block(1, recomputed_block1_hash),
            // Block 2 builds on block 1's recomputed hash.
            flash_base(2, hash("2a"), recomputed_block1_hash, ts + 4),
            flash_delta(2, hash("w2"), 1),
        ],
        ts,
    )
    .await;

    let events: Vec<FireEvent> = parse_fire_events(&raw)
        .into_iter()
        .filter(|e| matches!(e, FireEvent::FlashBlock { .. }))
        .collect();

    assert_fire_events_metadata_eq(
        &events,
        &[
            // Block 1's delta is the final partial (peek sees block 2's base): single is_final
            // emission at idx 1, sealed with the recomputed hash. The interleaved canonical(1)
            // signal is a serialized no-op.
            FireEvent::flash_block(1, recomputed_block1_hash, 1, true),
            // Block 2 proceeds normally on top of the (uncorrupted) carried-forward state.
            FireEvent::flash_block(2, hash("w2"), 1, false),
        ],
    );
}
