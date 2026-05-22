# Improve Flashblocks Test Framework

mode: feature
state: review
root_git: .worktrees/feature/improve-flashblocks-test-framework
worktree: .worktrees/feature/improve-flashblocks-test-framework
branch: feature/improve-flashblocks-test-framework
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

In flashblocks tests at `crates/firehose-flashblocks/tests/flashblock_sequence.rs`, improve the test framework so we can simulate when the chain's state is made available.

### Core Change

Update `run_flashblock_sequence` to accept input as `TestEvent` enum instead of raw `Flashblock`:

```rust
enum TestEvent {
    Flashblock(Flashblock),
    CanonicalBlock(<tbd>),
}
```

This enables tests like:

```rust
let raw = run_flashblock_sequence(client, vec![base_1, delta_1, canonical_1, base_2, delta_2]).await;
```

### Behavior

- When `run_flashblock_sequence` sees a `TestEvent::Flashblock`, it behaves exactly as today — emits the flashblock through the WebSocket server.
- When `run_flashblock_sequence` sees a `TestEvent::CanonicalBlock`, it modifies `GenesisClient` so that when `.state_by_block_number_or_tag(BlockNumberOrTag::Number(parent_block))` is called, it returns the correct new canonical block state.

### Goal

This change enables better coverage where we can simulate when a canonical block on the chain actually happens — testing the cross-block state carry-forward path where the processor bootstraps from a canonical provider (instead of carrying `accumulated_db` forward).

## Dev Feedback

2. It seems `GenesisClient` .header_by_number which is used in processor.rs for flashblocks isn't properly implemented, should look into canonical block list to return the correct one.
2. Add a test that check for `base1, canonical 1, canonical 2, base3` sequence which should correctly emit Flash1 Canonical1 Canonical2 Flash3.

### Applied (2026-05-22)

**Item 1 — Fix `header_by_number`:**
Added `header_for_block(n)` method to `GenesisClient` that synthesises a header with `number = n` and `timestamp = genesis_timestamp + n * 2` using the genesis header as a template. `header_by_number(n)` now delegates to this helper so the processor's `next_evm_env` correctly computes `block_env.number = parent.number + 1` for any block N.

**Item 2 — Canonical FIRE BLOCK emission + new test:**
Changed `run_flashblock_sequence` from WS-based to direct sequential processing:
- `TestEvent::Flashblock` → calls `processor.on_flashblock_received` directly (synchronous, preserves ordering).
- `TestEvent::CanonicalBlock(n)` → marks block N available in the provider AND emits a canonical FIRE BLOCK through a dedicated canonical tracer sharing the same `InMemoryBuffer`.

Two tracers write to the same buffer so output ordering exactly mirrors event processing order. Tests are now plain `#[test]` (no longer `#[tokio::test]`).

Added `base_canonical_gap_then_base_emits_four_fire_blocks` test verifying `base1 → canonical_block(1) → canonical_block(2) → base3` emits Flash1, Canonical1, Canonical2, Flash3.

Updated `canonical_block_unblocks_next_base` and `canonical_block_unblocks_non_sequential_gap` to expect the canonical FIRE BLOCK now emitted alongside provider availability.

All 13 integration tests pass; clippy clean.

## Spec & Implementation

### TestEvent Enum

Added `TestEvent` enum in `framework/mod.rs`:
- `Flashblock(Box<Flashblock>)` — boxed to avoid large-variant clippy warning (Flashblock is 688 bytes vs u64's 8 bytes)
- `CanonicalBlock(u64)` — marks a block number as available in the provider

Constructor helpers:
- `TestEvent::flashblock(fb: Flashblock) -> Self`
- `TestEvent::canonical_block(block_number: u64) -> Self` (const fn)

### GenesisClient Changes

Added `Arc<Mutex<GenesisClientInner>>` inner state to `GenesisClient`. `GenesisClientInner` holds `available_blocks: HashSet<u64>`.

Key behavior changes:
- `is_block_available(n)` returns `true` for block 0 (genesis) unconditionally, and for any block N that was marked via `mark_canonical_block_available(N)`.
- `state_by_block_number_or_tag(BlockNumberOrTag::Number(n))` now returns `Err(ProviderError::BlockBodyIndicesNotFound(n))` if block `n` is not available. All other tag variants (Latest, Pending, etc.) still return genesis state.
- `GenesisClient` remains `Clone` because the inner state is wrapped in `Arc<Mutex<...>>`.

### run_flashblock_sequence Changes

Signature changed from `Vec<Flashblock>` to `Vec<TestEvent>`. Events are pre-processed before the WS server starts: `CanonicalBlock` events call `client.mark_canonical_block_available(n)` synchronously; `Flashblock` events are collected and forwarded to `ws_server_once`.

This pre-processing approach (apply canonical blocks before starting the subscriber) means provider calls succeed on the first attempt in tests, avoiding the 20-retry timeout.

### New Tests

Three new tests total:
1. `canonical_block_unblocks_next_base` — sends base_1, marks block 1 canonical, sends base_2; verifies three events (Flash1, Canonical1, Flash2).
2. `canonical_block_unblocks_non_sequential_gap` — sends canonical_block(1) then base_2; verifies two events (Canonical1, Flash2).
3. `base_canonical_gap_then_base_emits_four_fire_blocks` — sends base1, canonical_block(1), canonical_block(2), base3; verifies four events (Flash1, Canonical1, Canonical2, Flash3).

Total: 13 integration tests, all passing.

## State Tracker

**Last Updated:** 2026-05-22
**Current Step:** Step 4 — Second dev feedback applied, ready for review
**Status:** Ready for review

| Step | Status | Notes |
|---|---|---|
| Initial setup | Done | Worktree created at .worktrees/feature/improve-flashblocks-test-framework |
| Implementation | Done | TestEvent enum, GenesisClient inner state, updated tests, 2 new tests, all 12 pass, clippy clean |
| Dev feedback (round 1) | Done | flash_base/flash_delta/canonical_block return TestEvent directly; delta vars renamed to delta<N>_<I> pattern; all 12 tests pass, clippy clean |
| Dev feedback (round 2) | Done | Fixed header_by_number; canonical FIRE BLOCK emission; WS server removed; new 4-event sequence test; all 13 tests pass, clippy clean |
