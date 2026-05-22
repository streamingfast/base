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

1. flash_base and flash_delta, they both should return right away a `TestEvent` object so we avoid having to wrap all emitted events inside `TestEvent::flashblock`, same for `TestEvent::canonical_block(1),` we should have locally a `canonical_block` helper or it can be assigned to a variable like other test cases.
1. Modify all delta variables to be on the form `delta<blockNum>_<flashIndex>` so it reads like `vec![base1, delta1_1, delta1_2, base2, etc...]`

**Applied in commit `94053caa4`.**

2. Rebase on top of firehose/0.x branch

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

Two new tests added:
1. `canonical_block_unblocks_next_base` — sends base_1, marks block 1 canonical, sends base_2; verifies both produce FIRE BLOCK events.
2. `canonical_block_unblocks_non_sequential_gap` — sends base_2 as the very first flashblock (no prior context); without `canonical_block(1)`, the provider would fail and block 2 would be skipped. With it, block 2 processes successfully.

Total: 12 integration tests, all passing.

## State Tracker

**Last Updated:** 2026-05-22
**Current Step:** Step 3 — Dev feedback applied, ready for review
**Status:** Ready for review

| Step | Status | Notes |
|---|---|---|
| Initial setup | Done | Worktree created at .worktrees/feature/improve-flashblocks-test-framework |
| Implementation | Done | TestEvent enum, GenesisClient inner state, updated tests, 2 new tests, all 12 pass, clippy clean |
| Dev feedback | Done | flash_base/flash_delta/canonical_block return TestEvent directly; delta vars renamed to delta<N>_<I> pattern; all 12 tests pass, clippy clean |
