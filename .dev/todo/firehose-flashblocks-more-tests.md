# Flashblocks More Test Coverage

mode: feature
state: review
root_git: .worktrees/feature/firehose-flashblocks-more-tests
worktree: .worktrees/feature/firehose-flashblocks-more-tests
branch: feature/firehose-flashblocks-more-tests
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

Task to add more firehose flashblocks tests as well as making minor refactoring to the test framework.

### Refactoring

- Rename `ParsedFireBlock` to `FireEvent` which would be an enum with three types `Init`, `Block`, `FlashBlock` (when a Block is a flash block). Each type would contain only what makes sense for it. `Block` and `FlashBlock` should contain a Payload `sf.ethereum.type.Block` object.
- In the flashblocks test cases, instead of using assertions when checking the FIRE output, use equalities against `FireEvent` of the right type. Create helpers to reduce the clutter in test cases — ideally checking the vec directly.
- Use the `pretty_assertions` library for testing to improve the rendered diffs when there is a mismatch.

### Coverage

- Improve the overall test coverage once refactoring is done by testing more edge cases:
  - Test sequence of base + delta + next base
  - Improve the overall coverage of various cases highlighted in `.dev/todo/firehose-flashblocks-support.md` (the prior task spec)

## Dev Feedback

1. Payload handling

```
**Payload omitted from `FireEvent`:** The base64-encoded protobuf payload field is intentionally omitted from the enum variants. Adding it would require either a prost decode (adding complexity) or a raw base64 string (preventing meaningful comparison). For all current test assertions, the protocol metadata fields are sufficient. The decision is documented in code.
```

I agree there is a good portion of cases that can be expressed without looking at the payload traced context.

However, it's important for coverage to test that part since tracing is at the heart of Firehose and the saas service we are offering. We need the complexity since we need to ensure that the flash block tracing is actually working correctly. The correct way is to properly read the base64 bytes into a sf.ethereum.type.v2.Block. This Rust struct exists in firehose-tracer has the right type in there, so it should be possible to decode it with Prost into

Let's create two set of test cases, those ignoring payload the other only checking the payload correctness across delta and full.

1. Prepare firehose-tracer helpers

Our FireEvent and parse helper should be made part of firehose-tracer crate upstream library. Other chain-agnostic testing utils could be moved there.

Prepare a plan as a new document at the very end of this task file where you write instructions for a future agent implementation.

1. The coverage of cases seems a bit low

For example, I see no test with two succesive delta, no jumping delta. Covers a 100% all cases that could happen according to Flashblock spec at https://docs.base.org/base-chain/api-reference/flashblocks-api/flashblocks-api-overview#flashblock-object

## Spec & Implementation

### Summary

Implemented the refactoring and new test coverage in a single commit on the feature branch.

### Decisions Made

**`FireEvent` enum design:**
The enum has three variants:
- `Init { version, node_name, node_version }` — for `FIRE INIT` lines
- `Block { block_number, prev_block_number, lib_num, timestamp_ns }` — for `FIRE BLOCK` lines where `printed_flash_idx == 0` (base flashblocks and canonical blocks)
- `FlashBlock { block_number, flash_idx, is_final, prev_block_number, lib_num, timestamp_ns }` — for `FIRE BLOCK` lines where `printed_flash_idx > 0` (delta flashblocks)

**Payload omitted from `FireEvent`:** The base64-encoded protobuf payload field is intentionally omitted from the enum variants. Adding it would require either a prost decode (adding complexity) or a raw base64 string (preventing meaningful comparison). For all current test assertions, the protocol metadata fields are sufficient. The decision is documented in code.

**Constructor helpers:** `FireEvent::canonical_block(n)` and `FireEvent::flash_block(n, idx, is_final)` produce expected values with `lib_num=0` and `timestamp_ns=0`, which are treated as wildcards by `assert_fire_events_eq`. This avoids coupling tests to genesis-derived timestamp values.

**`assert_fire_events_eq` normalisation:** The helper normalises `actual` events before comparing with `pretty_assertions::assert_eq!` — any field set to `0` in the expected event is zeroed in the actual copy, so diffs only show fields the test author cares about.

**`pretty_assertions` workspace integration:** Added to `[workspace.dependencies]` in the root `Cargo.toml` and to `[dev-dependencies]` in `crates/firehose-flashblocks/Cargo.toml`.

**Drive-by fix:** Removed the duplicate `SignatureFields for BaseTxEnvelope` implementation from `crates/common/consensus/src/reth_compat.rs`. The same trait was already implemented in `crates/common/consensus/src/transaction/envelope.rs`. The upstream `reth-firehose c5a3616c` bump made `SignatureFields` a public trait, causing a compile error. The `envelope.rs` implementation is kept as the canonical one.

### Test Coverage Added

1. **`base_plus_delta_emits_two_fire_blocks`** — base + one delta → two FIRE BLOCK lines with consecutive flash_idx values (`Block(1)`, `FlashBlock(1, idx=1, is_final=false)`).

2. **`base_plus_delta_plus_next_base`** — base(N) + delta(N) + base(N+1) → three FIRE BLOCK lines: `Block(1)`, `FlashBlock(1, idx=1)`, `Block(2)`. Exercises cross-block state carry-forward.

3. **`duplicate_base_is_ignored`** — same base sent twice → only one FIRE BLOCK emitted (sequence validator returns `Duplicate`).

4. **`non_sequential_delta_is_skipped`** — base + delta with index=2 (skipping index=1) → only the base FIRE BLOCK emitted (sequence validator returns `NonSequentialGap`, sets `is_skipping=true`).

The original `flash_base_emits_fire_block` test is kept (renamed effectively to `flash_base_emits_fire_block` with updated assertion style using `assert_fire_events_eq`).

### Build & Test Status

- `cargo test -p base-firehose-flashblocks` — 9 tests total (4 unit + 5 integration), all pass.
- `cargo clippy -p base-firehose-flashblocks --tests -- -D warnings` — clean.

## State Tracker

**Last Updated:** 2026-05-21
**Current Step:** Step 8 — Dev feedback applied, ready for re-review
**Status:** Ready for re-review

| Step | Status | Notes |
|---|---|---|
| Initial setup | Done | Worktree created at .worktrees/feature/firehose-flashblocks-more-tests |
| Step 1 — Refactoring (FireEvent enum, parse_fire_events, assert_fire_events_eq) | Done | See Spec & Implementation section |
| Step 2 — pretty_assertions integration | Done | Added to workspace and crate dev-dependencies |
| Step 3 — New tests (4 edge cases) | Done | See Spec & Implementation section |
| Step 4 — Drive-by fix | Done | Removed duplicate SignatureFields impl from reth_compat.rs |
| Step 5 — Add EthBlock payload to FireEvent variants | Done | Added `block: EthBlock` field to Block and FlashBlock variants; prost+base64 decode in parse_fire_events |
| Step 6 — Dual assertion helpers | Done | `assert_fire_events_metadata_eq` (ignores payload) and `assert_fire_events_eq` (full payload comparison) |
| Step 7 — Expand test coverage to 10 integration tests | Done | two_successive_deltas, jumping_delta_is_skipped, three_successive_deltas, two_blocks_with_deltas, block_payload_has_correct_block_number |
| Step 8 — Future Work section + task update | Done | Appended upstream plan at end of task file |

## Future Work: Upstream FireEvent to firehose-tracer

This section is addressed to a future agent that will move the chain-agnostic testing utilities from `base-firehose-flashblocks` into the upstream `firehose-tracer` crate, so that any firehose-instrumented chain (not just Base/OP) can reuse them.

### What to move upstream

The following items in `crates/firehose-flashblocks/tests/framework/mod.rs` are fully chain-agnostic and belong in `firehose-tracer`:

- **`FireEvent` enum** — `Init`, `Block`, `FlashBlock` variants with their metadata fields (`block_number`, `flash_idx`, `is_final`, `prev_block_number`, `lib_num`, `timestamp_ns`) and the decoded `EthBlock` (`firehose_tracer::pb::Block`) payload field. The wire format parsed here (FIRE INIT / FIRE BLOCK) is defined by `firehose-tracer` itself, so the parser naturally belongs there.
- **`parse_fire_events(raw: &[u8]) -> Vec<FireEvent>`** — base64+prost decode of the FIRE line format. The proto type used (`firehose_tracer::pb::Block`) is already in `firehose-tracer`, making it the natural home.
- **`decode_eth_block(payload_base64: &str) -> EthBlock`** — private helper; should be moved alongside `parse_fire_events`.
- **`assert_fire_events_metadata_eq`** — compares protocol metadata, ignores block payload.
- **`assert_fire_events_eq`** — full comparison including decoded block payload.
- **`normalize_metadata` / `normalize_full`** — private normalisation helpers.

### What stays in `base-firehose-flashblocks` tests

The following are Base/OP-specific and must remain in this repository:

- `GenesisClient` and `GenesisStateProvider` — implemented against `reth_provider` traits using Base-specific types (`BasePrimitives`, `BaseChainSpec`, `BaseTxEnvelope`, etc.).
- `flash_base` / `flash_delta` — build `Flashblock` fixtures using Base-specific types (`ExecutionPayloadBaseV1`, `ExecutionPayloadFlashblockDeltaV1`, `Metadata`) from `base-common-flashblocks`.
- `ws_server_once` / `run_flashblock_sequence` — drive the `FirehoseFlashblocksProcessor` which is a Base-specific type.
- `test_genesis()` — returns a Base-chain genesis (chain id 8453 with OP-stack hardforks).

### Target module structure in firehose-tracer

The upstream crate should expose a `testing` module, gated behind a `test-utils` Cargo feature to avoid pulling in `prost`, `base64`, and `pretty_assertions` in non-test builds:

```
firehose-tracer/
  src/
    testing.rs     # FireEvent, parse_fire_events, assert_fire_events_*
```

`Cargo.toml` additions:
```toml
[features]
test-utils = ["dep:base64", "dep:pretty_assertions"]

[dependencies]
# ... existing deps ...

[dependencies.base64]
version = "0.22"
optional = true

[dependencies.pretty_assertions]
version = "1"
optional = true
```

`lib.rs` addition:
```rust
#[cfg(feature = "test-utils")]
pub mod testing;
```

Consumers would add:
```toml
[dev-dependencies]
firehose-tracer = { workspace = true, features = ["test-utils"] }
```

### Steps for the future agent

1. Clone / check out the `firehose-tracer` repository (currently at version 5.1.1).
2. Create `src/testing.rs` with the chain-agnostic items listed above (copy from `framework/mod.rs`, removing the Base-specific imports).
3. Add the `test-utils` feature gate to `Cargo.toml` with optional `base64` and `pretty_assertions` deps.
4. Export `testing` from `lib.rs` behind `#[cfg(feature = "test-utils")]`.
5. Publish a new patch version (e.g. 5.1.2) of `firehose-tracer`.
6. In this repo, bump `firehose-tracer` to 5.1.2 in `Cargo.toml` and update `[dev-dependencies]` for `base-firehose-flashblocks` to add `features = ["test-utils"]`.
7. In `crates/firehose-flashblocks/tests/framework/mod.rs`, replace the local definitions of `FireEvent`, `parse_fire_events`, `assert_fire_events_metadata_eq`, and `assert_fire_events_eq` with `use firehose_tracer::testing::*;`.
8. Run `cargo test -p base-firehose-flashblocks` to confirm everything still passes.

### API considerations

- `EthBlock` (`firehose_tracer::pb::Block`) is already a first-class type in `firehose-tracer`, so no new proto dependencies are needed.
- `prost` is already a dependency of `firehose-tracer` (used for `encode_to_vec` / `decode`), so it does not need to be added as a feature-gated dep — just make it available without the feature gate.
- The `pretty_assertions` crate should be feature-gated because it is a dev/testing tool.
- `base64` is only needed for decoding in tests; gate it behind `test-utils`.
- The `FireEvent` enum derives `Debug + Clone + PartialEq` but not `Eq` (because `EthBlock` contains `Vec<u8>` fields which are `Eq` — in practice it is fine to add `Eq` as well if the prost-generated type implements it).
