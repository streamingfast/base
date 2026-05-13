# Firehose Integration Tests for Base Transactions

mode: feature
state: planned
root_git: .worktrees/feature/firehose-integration-tests
worktree: .worktrees/feature/firehose-integration-tests
branch: feature/firehose-integration-tests
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

We want to have Firehose integration tests that run real Base transaction, trace it and ensure it works properly. We would like to have prestate tests working just like in reth https://github.com/streamingfast/reth/tree/firehose/2.x/crates/firehose-tests/src

## Dev Feedback

1. I would like to see some re-use of reth-firehose-tests crate directly especially around the "generic" prestate struct and JSON parsing. Provide needed changes on the reth-firehose-tests crate to improve sharing on that part.

## Spec & Implementation

### Summary

Create a new `base-firehose-tests` library crate under `crates/execution/firehose-tests/` that reuses the generic harness infrastructure from `reth-firehose-tests` (`streamingfast/reth`, tag `v1.11.4-fh-1`). The plan is split into two parts:

1. **Upstream changes to `reth-firehose-tests`** — expose the currently-private generic types (`Prestate`, `TraceContext`, `RunOutcome`) and utility functions (`seed_cache_db`, `parse_fire_block_for`, `assert_block_equals_golden`, `decode_hex`, serde helpers) as `pub` so downstream crates can reuse them.

2. **New `base-firehose-tests` crate** — depends on `reth-firehose-tests` and provides only the Base/OP-specific `run_prestate` function and `TraceContext` extension. All shared machinery comes from `reth-firehose-tests` directly.

### Scope

**In scope:**
- Changes to `reth-firehose-tests` in `streamingfast/reth` (tag `v1.11.4-fh-1` / branch `firehose/2.x`) to expose reusable generic parts
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

### Part 1 — Changes to `reth-firehose-tests` (upstream PR to `streamingfast/reth`)

#### What to expose

The following items in `src/prestate.rs` are currently private but are entirely generic (no Ethereum-specific types). They must be made `pub` so `base-firehose-tests` can import them:

| Item | Current visibility | Change |
|---|---|---|
| `RunOutcome` struct | `pub` ✓ | Already public — no change needed |
| `Prestate` struct | `struct` (private) | Make `pub` |
| `TraceContext` struct | `struct` (private) | Make `pub` |
| `seed_cache_db` fn | `fn` (private) | Make `pub` |
| `build_account_info` fn | `fn` (private) | Make `pub` |
| `parse_fire_block_for` fn | `fn` (private) | Make `pub` |
| `assert_block_equals_golden` fn | `pub` ✓ | Already public — no change needed |
| `decode_hex` fn | `fn` (private) | Make `pub` |
| serde helpers module `private` | private `mod private` | Make `pub mod serde_helpers` (or `pub` items re-exported) |

**Important:** `Prestate` and `TraceContext` use serde `#[serde(deserialize_with = "...")]` referencing local private functions. Once the module is made public, the referenced deserializer functions must also be public (or the private module re-structured so they are callable from outside).

#### Recommended approach for serde helpers

Instead of exposing `deser_u64_str` / `deser_opt_u128_str` / `deser_opt_u256_str` as bare pub functions (which users normally don't call directly), move them into a `pub mod serde_helpers` submodule and re-export from `lib.rs`. The internal `private` module (`parse_decimal_or_hex_u128`) stays private since it is only called by the serde helpers.

Because `Prestate` and `TraceContext` carry `#[serde(deserialize_with = ...)]` attributes that reference these functions by path, the deserializer functions need to be accessible. Since they are referenced in attribute macros, they need to be in scope as `crate::prestate::deser_u64_str` etc. — simply keeping them as `pub fn` in `prestate.rs` (rather than `fn`) is sufficient. The `private` inner module for `parse_decimal_or_hex_u128` can remain private.

#### OP-specific `TraceContext` extension

`base-firehose-tests` may need additional fields in the block context for OP Stack (e.g., `prevRandao`/`mixHash`). Rather than modifying the shared `TraceContext` with OP-specific optional fields, the recommended approach is:

- Keep `reth-firehose-tests`'s `TraceContext` as is (the common Ethereum fields)
- In `base-firehose-tests`'s `src/prestate.rs`, define a separate `OpTraceContext` struct that **contains** a `TraceContext` (via `#[serde(flatten)]`) plus any OP-specific optional fields

This avoids polluting the Ethereum-centric `TraceContext` with OP fields.

#### `lib.rs` changes in `reth-firehose-tests`

Add re-exports of the newly-public items following AGENTS.md conventions:

```rust
// existing
pub mod prestate;
pub use prestate::{assert_block_equals_golden, run_prestate, RunOutcome};

// new re-exports
pub use prestate::{
    Prestate, TraceContext,
    seed_cache_db, build_account_info,
    parse_fire_block_for, decode_hex,
};
```

---

### Part 2 — New `base-firehose-tests` crate

#### Design

The crate depends on `reth-firehose-tests` and re-uses its public generic parts. The only Base-specific code lives in `src/prestate.rs`:

- `OpTraceContext` — extends `TraceContext` with OP-specific optional fields (via `#[serde(flatten)]`)
- `run_prestate` — the Base adaptation of the harness function, using:
  - `BaseChainSpec` instead of `ChainSpec`
  - `BaseEvmConfig` instead of `EthEvmConfig`
  - `BasePrimitives` instead of `EthPrimitives`
  - OP transaction decoding (`OpTxEnvelope::decode_2718`) instead of `TransactionSigned::network_decode`
  - `OpPreTxAdjust` / `OpPostTxExtras` instead of `NoPreTxAdjust` / `NoPostTxExtras`
  - OP-specific `ChainConfig` fields (`canyon_time` → `shanghai_time`, `ecotone_time` → `cancun_time`, `isthmus_time` → `prague_time`)

All other utilities (`assert_block_equals_golden`, `seed_cache_db`, `parse_fire_block_for`, `decode_hex`, `RunOutcome`) come from `reth_firehose_tests::` directly.

#### Key differences from the Ethereum reference (`reth-firehose-tests`)

| Aspect | Ethereum (`reth-firehose-tests`) | Base (`base-firehose-tests`) |
|---|---|---|
| Chain spec | `Arc<ChainSpec>` from `Genesis` | `Arc<BaseChainSpec>` from genesis |
| EVM config | `EthEvmConfig::new(chain_spec)` | `BaseEvmConfig::new(chain_spec)` |
| Primitives | `EthPrimitives` | `BasePrimitives` |
| Tx decode | `TransactionSigned::network_decode` | `OpTxEnvelope::decode_2718` |
| Pre/post hooks | `NoPreTxAdjust` / `NoPostTxExtras` | `OpPreTxAdjust` / `OpPostTxExtras` |
| Trace context | `TraceContext` (shared) | `OpTraceContext` wrapping `TraceContext` |
| Fork timestamps | `shanghai_time`, `cancun_time`, `prague_time` directly | Map `canyon_time`→`shanghai_time`, `ecotone_time`→`cancun_time`, `isthmus_time`→`prague_time` |

#### Transaction decoding

```rust
use alloy_eips::eip2718::Decodable2718;
use alloy_op_consensus::OpTxEnvelope;

let tx_bytes = reth_firehose_tests::decode_hex(&prestate.input)?;
let signed_tx = OpTxEnvelope::decode_2718(&mut tx_bytes.as_slice())
    .context("RLP-decoding prestate.input as an OP signed transaction")?;
```

Verify that `TxTy<BasePrimitives>` is `OpTxEnvelope` (check `base-common-consensus`'s primitives alias) and use accordingly for `RecoveredBlock`.

#### ChainSpec construction

```rust
use base_execution_chainspec::BaseChainSpec;

let chain_spec = Arc::new(BaseChainSpec::from(prestate.genesis.clone()));
```

If `BaseChainSpec` does not implement `From<Genesis>` directly, use `BaseChainSpecBuilder` — check `crates/execution/chainspec/src/builder.rs`.

#### `run_prestate` structure in `base-firehose-tests`

```rust
pub fn run_prestate(case_folder: &Path) -> eyre::Result<RunOutcome> {
    // 1. Read and deserialize prestate.json using shared Prestate type
    let prestate_path = case_folder.join("prestate.json");
    let prestate: Prestate = serde_json::from_slice(&std::fs::read(&prestate_path)?)
        .with_context(|| ...)?;

    // 2. Build BaseChainSpec from genesis
    let chain_spec = Arc::new(BaseChainSpec::from(prestate.genesis.clone()));
    let parent_hash = chain_spec.genesis_hash();

    // 3. Decode OP transaction
    let tx_bytes = decode_hex(&prestate.input)?;
    let signed_tx = OpTxEnvelope::decode_2718(&mut tx_bytes.as_slice())?;

    // 4. Build header + block (using prestate.context fields)
    let header = build_op_header(&prestate.context, parent_hash, &[signed_tx.clone()]);
    let block = OpBlock { header, body: OpBlockBody { transactions: vec![signed_tx], ... } };
    let recovered = block.try_into_recovered()?;

    // 5. Seed CacheDB using shared helper
    let mut db = CacheDB::new(EmptyDB::default());
    seed_cache_db(&mut db, &prestate.genesis)?;  // from reth_firehose_tests
    let mut state = State::builder()...build();

    // 6. Build BaseEvmConfig
    let evm_config = BaseEvmConfig::new(chain_spec.clone());

    // 7. Build tracer with OP fork timestamps
    let (mut tracer, buffer) = firehose_tracer::Tracer::with_buffer(
        firehose_tracer::config::Config::default(),
        firehose_tracer::config::ChainConfig {
            chain_id: prestate.genesis.config.chain_id,
            shanghai_time: prestate.genesis.config.op_config.canyon_time,
            cancun_time:   prestate.genesis.config.op_config.ecotone_time,
            prague_time:   prestate.genesis.config.op_config.isthmus_time,
            verkle_time:   None,
        },
        "base-firehose-tests",
        env!("CARGO_PKG_VERSION"),
    );

    // 8. Start tracer and execute
    let mut block_tracer = FirehoseBlockTracer::start_local::<BasePrimitives>(...);
    let exec_result = run_wrapped_block::<_, _, _, _, _>(
        &evm_config, &mut state, &recovered, &mut block_tracer,
        OpPreTxAdjust, OpPostTxExtras,
    );
    ...

    // 9. Parse output using shared helper
    let raw = buffer.get_bytes();
    let block = parse_fire_block_for(&raw, block_number)?;  // from reth_firehose_tests
    Ok(RunOutcome { block, raw })
}
```

#### Test fixture format

```
crates/execution/firehose-tests/tests/cases/
  nop_transfer/
    prestate.json          # genesis + context + hex-encoded RLP tx
    block.<num>.binpb      # expected Firehose Block (binary protobuf)
```

The `prestate.json` follows the same schema as the reference. For Base, the `genesis.config` must include OP hardfork timestamps (e.g., `"canyonTime": 0, "ecotoneTime": 0`) so that `BaseChainSpec` is correctly initialized.

#### Golden file generation

The implementor generates the `.binpb` goldens by setting `UPDATE_GOLDENS=true` (or similar env-var check inside `assert_block_equals_golden`) on first run, then committing the result.

---

### Implementation Plan

#### Step 0 — Upstream changes to `reth-firehose-tests` in `streamingfast/reth`

These changes must be submitted as a PR to `streamingfast/reth` (branch `firehose/2.x`) **before** the workspace tag used by `base` can be updated to include them. The implementor should:

0a. In `crates/firehose-tests/src/prestate.rs`, make the following items `pub`:
  - `Prestate` struct (and its fields, which are already non-`pub(crate)`)
  - `TraceContext` struct (and its fields)
  - `seed_cache_db` function
  - `build_account_info` function
  - `parse_fire_block_for` function
  - `decode_hex` function
  - The three serde deserializer fns: `deser_u64_str`, `deser_opt_u128_str`, `deser_opt_u256_str` (these are referenced in `#[serde(deserialize_with = "...")]` attributes on the now-public structs and must remain accessible)

0b. In `crates/firehose-tests/src/lib.rs`, add re-exports of all newly-public items:
  ```rust
  pub use prestate::{
      Prestate, TraceContext,
      seed_cache_db, build_account_info,
      parse_fire_block_for, decode_hex,
  };
  ```

0c. Open a PR to `streamingfast/reth` with these changes, get it merged, and cut a new tag (e.g., `v1.11.4-fh-2`) that includes these changes.

0d. Update the `reth-firehose` (and related reth) entries in `base`'s root `Cargo.toml` to point at the new tag, and add `reth-firehose-tests` to `[workspace.dependencies]`:
  ```toml
  reth-firehose-tests = { git = "https://github.com/streamingfast/reth.git", tag = "v1.11.4-fh-2" }
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
| Upstream `reth-firehose-tests` changes come first (Step 0) | Enables clean reuse; the alternative (copy-pasting) violates the user's explicit feedback |
| New tag (e.g. `v1.11.4-fh-2`) required in `streamingfast/reth` | The workspace uses tagged git deps; a new tag is the standard release mechanism |
| `Prestate` and `TraceContext` made `pub` as-is (no generics) | The structs use concrete alloy types that are shared across Ethereum and OP Stack |
| OP-specific context fields go in a local `OpTraceContext` wrapping `TraceContext` | Avoids polluting Ethereum-centric struct with OP fields; `#[serde(flatten)]` provides transparent JSON merging |
| `assert_block_equals_golden` re-exported from `reth-firehose-tests` | Already public and fully generic; no reason to duplicate |
| `RunOutcome` re-exported from `reth-firehose-tests` | Already public and fully generic |
| Start with one test case (`nop_transfer`) | Minimal viable harness; more cases easy to add |
| Use `OpPreTxAdjust` / `OpPostTxExtras` from `base-execution-firehose` | Already implemented and tested for OP Stack |
| Goldens are committed binary files (`.binpb`) | Same pattern as reference; no runtime generation |
| `UPDATE_GOLDENS` env-var convenience in `assert_block_equals_golden` | Standard pattern for regenerating goldens (already exists in reference, lives in `reth-firehose-tests`) |

---

## State Tracker

**Last Updated:** 2026-05-13
**Current Step:** Phase 5 — Spec Review & Acceptance (Revision 2)
**Status:** Spec revised per Dev Feedback, awaiting approval

| Step | Status | Notes |
|---|---|---|
| Phase 1 — Contextual Understanding | Done | Explored execution/firehose, common/evm, common/consensus, execution/chainspec; fetched reference lib.rs + prestate.rs + Cargo.toml from reth |
| Phase 2 — Gap Analysis | Done | Key gaps: OP tx decode, BaseChainSpec construction, ChainConfig mapping |
| Phase 3 — Challenging Dialogue | Skipped | Sufficient info from codebase + reference to write spec without questions |
| Phase 4 — Specification Writing | Done | Full spec written above |
| Phase 5 — Spec Review (Round 1) | Done | User rejected: requested reuse of reth-firehose-tests generic parts |
| Phase 5 — Spec Review (Round 2) | In Progress | Revised spec: Part 1 = upstream changes to reth-firehose-tests; Part 2 = thin base-firehose-tests layer |
