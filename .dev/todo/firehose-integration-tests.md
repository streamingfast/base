# Firehose Integration Tests for Base Transactions

mode: feature
state: review
root_git: .worktrees/feature/firehose-integration-tests
worktree: .worktrees/feature/firehose-integration-tests
branch: feature/firehose-integration-tests
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

We want to have Firehose integration tests that run real Base transaction, trace it and ensure it works properly. We would like to have prestate tests working just like in reth https://github.com/streamingfast/reth/tree/firehose/1.x/crates/firehose-tests/src

## Dev Feedback

2. Our reth fork at `https://github.com/streamingfast/reth/tree/firehose/1.x` has all the requested changes defined by you in `Part 1` below. Clone and inspect that the changes are correct according to our plan and adjust the plan with the new details. and removal of part 1 which is not needed.

**Verified (2026-05-13):** Cloned both `v1.11.4-fh-1` tag and `firehose/1.x` branch HEAD.
- Tag `v1.11.4-fh-1` (currently used by base): `Prestate`, `TraceContext`, `seed_cache_db`, `build_account_info`, `parse_fire_block_for`, `decode_hex` are all **private** in prestate.rs — Part 1 changes are NOT in this tag.
- Branch `firehose/1.x` HEAD (commit `06e46c3`, "Reformatted code"): All those items are **already public**, and `lib.rs` already re-exports them all — exactly what Part 1 required.
- There is no `v1.11.4-fh-2` tag yet; only `v1.11.4-fh-1` exists.
- `reth-firehose-tests` is not in base's `[workspace.dependencies]` yet.

**Conclusion:** Part 1 is done on the branch but not yet tagged. Step 0 in the plan is updated to: "cut a new tag on the `firehose/1.x` branch and update base's Cargo.toml to that tag + add `reth-firehose-tests` to workspace deps." Part 1 section removed from the spec.

## Spec & Implementation

### Summary

Create a new `base-firehose-tests` library crate under `crates/execution/firehose-tests/` that reuses the generic harness infrastructure from `reth-firehose-tests` (`streamingfast/reth`). The upstream `reth-firehose-tests` crate on the `firehose/1.x` branch already exposes all required public types (`Prestate`, `TraceContext`, `RunOutcome`, `seed_cache_db`, `build_account_info`, `parse_fire_block_for`, `decode_hex`, serde helpers) — these changes are on the branch but not yet tagged. Step 0 is simply: cut a new tag on the branch and update base's Cargo.toml to point at it.

The new `base-firehose-tests` crate depends on `reth-firehose-tests` and provides only the Base/OP-specific `run_prestate` function and `OpTraceContext` extension. All shared machinery comes from `reth-firehose-tests` directly.

### Scope

**In scope:**
- Step 0: cut a new tag on `streamingfast/reth` `firehose/1.x` branch (all Part 1 changes already exist on the branch) and update base's Cargo.toml to that tag + add `reth-firehose-tests` to `[workspace.dependencies]`
- New workspace crate `base-firehose-tests` at `crates/execution/firehose-tests/`
- `src/lib.rs` and `src/prestate.rs` — the Base-specific harness (thin layer over `reth-firehose-tests`)
- `tests/prestate.rs` — the Cargo integration test driver
- At minimum one test case: a simple ETH transfer (`nop_transfer`) with `prestate.json` + golden `*.binpb`
- The crate registered in `Cargo.toml` workspace members

**Out of scope:**
- Generating golden files programmatically (goldens are committed artifacts, generated once)
- CI pipeline changes (the implementor can wire that separately)
- More than one initial test case

---

### Implementation Plan

#### Step 0 — Tag the upstream changes and update base's Cargo.toml

The `firehose/1.x` branch of `streamingfast/reth` already contains all the required public-visibility changes to `reth-firehose-tests` (verified at commit `06e46c3`). No PR is needed — the code is already there. The implementor should:

0b. Update **all** `reth`-namespaced entries in base's root `Cargo.toml` from `tag = "v1.11.4-fh-1"` to branch `firehose/1.x`

0c. Add `reth-firehose-tests` to `[workspace.dependencies]` in base's root `Cargo.toml`:
  ```toml
  reth-firehose-tests = { git = "https://github.com/streamingfast/reth.git", <branch ...> }
  ```

#### Step 1 — Create crate skeleton at `crates/execution/firehose-tests/`

- `Cargo.toml` — `name = "base-firehose-tests"`, `publish = false`, workspace deps (see below)
- `README.md` — one-liner describing the crate purpose
- `src/lib.rs` — minimal re-export per AGENTS.md
- `src/prestate.rs` — Base-specific harness (thin layer over `reth-firehose-tests`)
- `tests/prestate.rs` — test driver
- `tests/cases/.gitkeep`

#### Step 2 — Register in workspace

Add `"crates/execution/firehose-tests"` to `members` in root `Cargo.toml` (after `"crates/execution/firehose"`). Add `base-firehose-tests = { path = "crates/execution/firehose-tests" }` to `[workspace.dependencies]`.

#### Step 3 — Implement `src/prestate.rs`

Port the Ethereum reference `run_prestate`, replacing:
- `ChainSpec` → `BaseChainSpec`
- `EthEvmConfig` → `BaseEvmConfig`
- `EthPrimitives` → `BasePrimitives`
- `TransactionSigned::network_decode` → `OpTxEnvelope::decode_2718`
- `NoPreTxAdjust` / `NoPostTxExtras` → `OpPreTxAdjust` / `OpPostTxExtras`
- `firehose_tracer::config::ChainConfig` fork timestamps: `canyon_time` → `shanghai_time`, `ecotone_time` → `cancun_time`, `isthmus_time` → `prague_time`

Import and use from `reth_firehose_tests`: `Prestate`, `TraceContext`, `seed_cache_db`, `parse_fire_block_for`, `decode_hex`, `RunOutcome`.

Define a local `build_op_header` (analogous to `build_header` in the reference) using OP block types.

#### Step 4 — Implement `src/lib.rs`

```rust
#![doc = include_str!("../README.md")]

mod prestate;
pub use prestate::{run_prestate};

// Re-export shared types from reth-firehose-tests for convenience
pub use reth_firehose_tests::{assert_block_equals_golden, RunOutcome};
```

#### Step 5 — Create test data for `nop_transfer`

- Craft a minimal `prestate.json` with a simple ETH-transfer signed transaction, a genesis seeding the sender with enough ETH, and a block context matching a post-Regolith OP block
- Run with `UPDATE_GOLDENS=true` to produce the golden `.binpb`
- Commit both files under `tests/cases/nop_transfer/`

#### Step 6 — Write `tests/prestate.rs`

```rust
use std::path::PathBuf;
use base_firehose_tests::{assert_block_equals_golden, run_prestate};

#[test]
fn nop_transfer() {
    let folder = case_dir("nop_transfer");
    let outcome = run_prestate(&folder).expect("nop_transfer prestate must succeed");
    let golden = folder.join("block.2099.binpb");
    assert_block_equals_golden(&outcome.block, &golden).expect("captured block must match golden");
}

fn case_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("cases").join(name)
}
```

#### Step 7 — `Cargo.toml` for `base-firehose-tests`

```toml
[package]
name = "base-firehose-tests"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
homepage.workspace = true
repository.workspace = true
publish = false

[lints]
workspace = true

[dependencies]
# reth firehose shared harness
reth-firehose-tests.workspace = true

# base
base-execution-evm.workspace = true
base-common-consensus.workspace = true
base-execution-firehose.workspace = true
base-execution-chainspec.workspace = true

# reth / firehose
reth-firehose.workspace = true
firehose-tracer.workspace = true
reth-revm.workspace = true
reth-primitives-traits.workspace = true

# alloy / op consensus
alloy-eips.workspace = true
alloy-genesis.workspace = true
alloy-consensus.workspace = true
alloy-primitives.workspace = true
alloy-op-consensus.workspace = true

# revm
revm.workspace = true

# encoding & errors
eyre.workspace = true
prost.workspace = true
serde.workspace = true
serde_json.workspace = true

[[test]]
name = "prestate"
path = "tests/prestate.rs"
```

Note: `hex`, `base64`, and other low-level encoding deps are no longer needed directly — they are encapsulated inside `reth-firehose-tests`.

### Key Implementation Notes

#### `alloy-op-consensus` in workspace deps

Check if `alloy-op-consensus` is already in `[workspace.dependencies]` via `grep "alloy-op-consensus" Cargo.toml`. If not, add it before use.

#### `BaseEvmConfig` constructor

Check `crates/execution/evm/src/lib.rs` around line 135 for the concrete type parameters. Look at how it is instantiated in `crates/execution/node/` for the production configuration and mirror that.

#### OP chain config fields for `firehose_tracer::config::ChainConfig`

The OP genesis config is accessible via `prestate.genesis.config.optimism` (or similar field from `alloy_op_genesis`). The mapping is:
- `canyon_time` → `shanghai_time`
- `ecotone_time` → `cancun_time`
- `isthmus_time` → `prague_time`
- `verkle_time` → `None`

Verify the actual field names in the `alloy` OP genesis types used by this workspace.

#### `firehose_tracer::config::ChainConfig` fields

Verify the exact fields available in `firehose-tracer` at the version pinned in this workspace. The names above (`shanghai_time`, `cancun_time`, `prague_time`, `verkle_time`) are based on the reference code; confirm they match.

### Decisions & Assumptions

| Decision/Assumption | Rationale |
|---|---|
| Step 0 = tag branch + update Cargo.toml (no PR needed) | All Part 1 changes already exist on `firehose/1.x` branch HEAD (verified at commit `06e46c3`); only a new tag and Cargo.toml bump needed |
| New tag (e.g. `v1.11.4-fh-2`) required in `streamingfast/reth` | The workspace uses tagged git deps; a new tag is the standard release mechanism |
| `Prestate` and `TraceContext` already `pub` in branch HEAD | Verified by inspection — no changes needed to reth fork |
| OP-specific context fields go in a local `OpTraceContext` wrapping `TraceContext` | Avoids polluting Ethereum-centric struct with OP fields; `#[serde(flatten)]` provides transparent JSON merging |
| `assert_block_equals_golden` re-exported from `reth-firehose-tests` | Already public and fully generic; no reason to duplicate |
| `RunOutcome` re-exported from `reth-firehose-tests` | Already public and fully generic |
| Start with one test case (`nop_transfer`) | Minimal viable harness; more cases easy to add |
| Use `OpPreTxAdjust` / `OpPostTxExtras` from `base-execution-firehose` | Already implemented and tested for OP Stack |
| Goldens are committed binary files (`.binpb`) | Same pattern as reference; no runtime generation |
| `UPDATE_GOLDENS` env-var convenience in `assert_block_equals_golden` | Standard pattern for regenerating goldens (already exists in reference, lives in `reth-firehose-tests`) |

---

## State Tracker

**Last Updated:** 2026-05-16
**Current Step:** Done — ready for review
**Status:** Implementation complete; `cargo test -p base-firehose-tests --test prestate` passes (`nop_transfer ... ok`)

| Step | Status | Notes |
|---|---|---|
| Phase 1 — Contextual Understanding | Done | Explored execution/firehose, common/evm, common/consensus, execution/chainspec; fetched reference lib.rs + prestate.rs + Cargo.toml from reth |
| Phase 2 — Gap Analysis | Done | Key gaps: OP tx decode, BaseChainSpec construction, ChainConfig mapping |
| Phase 3 — Challenging Dialogue | Skipped | Sufficient info from codebase + reference to write spec without questions |
| Phase 4 — Specification Writing | Done | Full spec written above |
| Phase 5 — Spec Review (Round 1) | Done | User rejected: requested reuse of reth-firehose-tests generic parts |
| Phase 5 — Spec Review (Round 2) | Done | Revised spec: Part 1 = upstream changes to reth-firehose-tests; Part 2 = thin base-firehose-tests layer |
| Dev Feedback — Verify reth fork | Done | Cloned v1.11.4-fh-1 tag and firehose/1.x branch; confirmed Part 1 changes already on branch at commit 06e46c3; no v1.11.4-fh-2 tag yet; Part 1 removed from spec; Step 0 updated to "cut new tag + update Cargo.toml" |
| Step 0 — Update workspace deps to branch | Done | All reth deps changed from tag v1.11.4-fh-1 → branch firehose/1.x; reth-firehose-tests + base-firehose-tests added to workspace; firehose-tracer bumped to 5.1.1 |
| Step 1–7 — Crate implementation | Done | crates/execution/firehose-tests/ created with src/lib.rs, src/prestate.rs, tests/prestate.rs, tests/cases/nop_transfer/ (prestate.json + block.2099.binpb), examples/generate_golden.rs |
| SignatureFields for BaseTxEnvelope | Done | Added to crates/common/consensus/src/reth_compat.rs; deposit txs return (B256::ZERO, B256::ZERO, Bytes::new()) |
| Tests | Done | `cargo test -p base-firehose-tests --test prestate` → `test nop_transfer ... ok` |
