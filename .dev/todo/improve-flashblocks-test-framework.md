# Improve Flashblocks Test Framework

mode: feature
state: ready
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

<empty>

## Spec & Implementation

<agent to fill in>

## State Tracker

**Last Updated:** 2026-05-22
**Current Step:** Step 1 — Start
**Status:** Ready for implementation

| Step | Status | Notes |
|---|---|---|
| Initial setup | Done | Worktree created at .worktrees/feature/improve-flashblocks-test-framework |
