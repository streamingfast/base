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

## Spec & Implementation

### Summary

Create a new `base-firehose-tests` library crate under `crates/execution/firehose-tests/` that mirrors the `reth-firehose-tests` harness from `streamingfast/reth` (branch `firehose/2.x`), adapted for the Base/OP Stack primitives already present in this repository. The crate provides a prestate-driven test framework: each test case folder contains a `prestate.json` (genesis + block context + RLP-encoded signed transaction) and a `*.binpb` golden file (expected Firehose `Block` protobuf). The framework executes the transaction through the Base Firehose tracer and asserts the captured protobuf output matches the golden.

### Scope

**In scope:**
- New workspace crate `base-firehose-tests` at `crates/execution/firehose-tests/`
- `src/lib.rs` and `src/prestate.rs` — the harness library (adapted from reth's `reth-firehose-tests`)
- `tests/prestate.rs` — the Cargo integration test driver
- At minimum one test case: a simple ETH transfer (`nop_transfer`) with `prestate.json` + golden `*.binpb`
- The crate registered in `Cargo.toml` workspace members

**Out of scope:**
- Generating golden files programmatically (goldens are committed artifacts, generated once via a manual run with `UPDATE_GOLDENS=1` or equivalent)
- CI pipeline changes (the implementor can wire that separately)
- More than one initial test case (additional cases are easy to add after the harness exists)

### Design

#### Key differences from the Ethereum reference (`reth-firehose-tests`)

The reference crate (`streamingfast/reth`, `firehose/2.x`) uses:
- `reth_chainspec::ChainSpec` — plain Ethereum chain spec
- `reth_evm_ethereum::EthEvmConfig` — Ethereum EVM config
- `reth_ethereum_primitives::EthPrimitives` / `TransactionSigned` — Ethereum transaction type
- `reth_firehose::{NoPreTxAdjust, NoPostTxExtras}` — no OP-specific hooks

The **Base adaptation** must use:
- `base_execution_chainspec::BaseChainSpec` — OP Stack chain spec wrapper (wraps `ChainSpec` with OP hardforks)
- `base_execution_evm::BaseEvmConfig<BaseChainSpec, ...>` — the Base/OP EVM config already used in production
- `base_common_consensus::BasePrimitives` — OP primitives (handles deposit transactions, etc.)
- `base_execution_firehose::{OpPreTxAdjust, OpPostTxExtras}` — the OP-specific hooks already implemented
- `alloy_op_consensus::OpTxEnvelope` (via `alloy_eips::eip2718::Decodable2718`) — for decoding OP transactions (including deposit type 0x7E)

#### Transaction decoding

Reth's `TransactionSigned::network_decode` handles Ethereum-format transactions. For Base we must use the OP transaction envelope decoder — specifically `OpPooledTransactionElement::decode_2718` or the equivalent from `alloy_op_consensus` that handles deposit transactions (type `0x7E`). The prestate.json `input` field carries the hex-encoded RLP bytes of a signed transaction, which may be any OP transaction type.

The harness needs to decode into `base_common_consensus`'s transaction type (whatever implements `TxTy<BasePrimitives>`). Since `BasePrimitives` is re-exported from `base_common_consensus`, the concrete type is `alloy_op_consensus::OpTxEnvelope` wrapped as a `WithEncoded` if needed — inspect the actual `TxTy` alias for `BasePrimitives` to confirm and use it in the decode step.

#### ChainSpec construction

The reth reference builds `Arc<ChainSpec>` directly from `Genesis`. For Base we construct `Arc<BaseChainSpec>` — the `BaseChainSpec` has a `From<Genesis>` or `TryFrom<Genesis>` conversion (check `crates/execution/chainspec/src/builder.rs`). If no direct `From<Genesis>` exists, use `BaseChainSpecBuilder` seeded from the genesis config's OP fields.

#### CacheDB seeding

Identical to the reference: iterate `genesis.alloc`, calling `db.insert_account_info` and `db.insert_account_storage` for each entry.

#### Block structure

The Base/OP Stack block has OP-specific header fields (no `blob_gas_used`/`excess_blob_gas` for pre-Cancun, different receipt type). The header construction mirrors the reference but uses OP primitives. Concretely:
- Use `op_alloy_consensus::OpBlock` (or the type aliased as `BlockTy<BasePrimitives>`) with the same header field pinning logic as the reference
- Include `withdrawals: Some(Withdrawals::default())` for post-Shanghai blocks (same as reference)

#### `run_prestate` function

Same structure as the reference:
1. Read and deserialize `prestate.json`
2. Build `Arc<BaseChainSpec>` from genesis
3. Decode the RLP-encoded transaction
4. Build header + block, recover senders
5. Seed `CacheDB<EmptyDB>` from genesis alloc
6. Build `State`
7. Create the EVM config: `BaseEvmConfig::new(Arc::clone(&chain_spec))` (check exact constructor from `crates/execution/evm/src/lib.rs`)
8. Create tracer with `firehose_tracer::Tracer::with_buffer(...)` — same call as reference but use `chain_id` from genesis
9. Start `FirehoseBlockTracer::start_local::<BasePrimitives>(...)`
10. Call `run_wrapped_block::<_, _, _, _, _>(&evm_config, &mut state, &recovered, &mut block_tracer, OpPreTxAdjust, OpPostTxExtras)`
11. Parse `FIRE BLOCK` line, return `RunOutcome`

#### `assert_block_equals_golden`

Identical to the reference: decode the golden `.binpb`, compare with `==` on `FirehoseBlock`, write `.actual.txt` / `.expected.txt` debug files on mismatch.

#### Test fixture format

```
crates/execution/firehose-tests/tests/cases/
  nop_transfer/
    prestate.json          # genesis + context + hex-encoded RLP tx
    block.<num>.binpb      # expected Firehose Block (binary protobuf)
```

The `prestate.json` follows the same schema as the reference:
```json
{
  "genesis": { ... },         // alloy_genesis::Genesis JSON format
  "context": {
    "number": "2099",
    "timestamp": "1234567890",
    "gasLimit": "30000000",
    "miner": "0x...",
    "baseFeePerGas": "1000000000"
  },
  "input": "0x..."            // hex-encoded RLP of a signed transaction
}
```

For the Base-specific genesis, the `config` section must include `optimism: {}` (or OP-specific hardfork timestamps) so that `BaseChainSpec::try_from(genesis)` produces a valid OP chain spec.

#### Golden file generation

The implementor must produce the `.binpb` goldens. The recommended flow:
1. Implement the harness
2. Add a `#[test]` that calls `run_prestate` and writes the `block` to disk if `UPDATE_GOLDENS=1` is set (or simply print `hex::encode(block.encode_to_vec())` and copy manually)
3. Commit the golden

An alternative is to add an `UPDATE_GOLDENS` env-var check inside `assert_block_equals_golden` itself (as a dev convenience, not checked in):
```rust
if std::env::var("UPDATE_GOLDENS").is_ok() {
    std::fs::write(golden_path, captured.encode_to_vec()).unwrap();
    return Ok(());
}
```

### Implementation Plan

1. **Create crate skeleton** at `crates/execution/firehose-tests/`:
   - `Cargo.toml` — `name = "base-firehose-tests"`, `publish = false`, workspace dependencies listed below
   - `README.md` — one-liner describing the crate purpose
   - `src/lib.rs` — minimal re-export following AGENTS.md conventions
   - `src/prestate.rs` — harness implementation (adapted from reference)
   - `tests/prestate.rs` — test driver
   - `tests/cases/.gitkeep` — placeholder until test data is added

2. **Register in workspace** — add `"crates/execution/firehose-tests"` to the `members` list in root `Cargo.toml` (after `"crates/execution/firehose"` for logical grouping). Add `base-firehose-tests = { path = "crates/execution/firehose-tests" }` to `[workspace.dependencies]`.

3. **Implement `src/prestate.rs`** — port the reference `prestate.rs` replacing:
   - `ChainSpec` → `BaseChainSpec`
   - `EthEvmConfig` → `BaseEvmConfig` (constructed as `BaseEvmConfig::new(chain_spec.clone())`)
   - `EthPrimitives` → `BasePrimitives`
   - `TransactionSigned::network_decode` → OP transaction decode (use `alloy_op_consensus`'s decoder)
   - `NoPreTxAdjust` / `NoPostTxExtras` → `OpPreTxAdjust` / `OpPostTxExtras`
   - Chain config: pass `canyon_time`, `ecotone_time`, `fjord_time`, `granite_time`, `holocene_time` etc from the genesis OP config fields to `firehose_tracer::config::ChainConfig`
   - `TraceContext`: add OP-specific optional fields (`prevRandao` / `mixHash`) if needed — start with the same set as reference; the OP chain context may not need extra fields for simple transfers

4. **Implement `src/lib.rs`** — minimal, per AGENTS.md:
   ```rust
   #![doc = include_str!("../README.md")]
   
   mod prestate;
   pub use prestate::{RunOutcome, assert_block_equals_golden, run_prestate};
   ```

5. **Create test data** for `nop_transfer`:
   - Craft a minimal `prestate.json` with a simple ETH-transfer signed transaction, a genesis that seeds the sender with enough ETH, and a block context matching a post-Regolith OP block
   - Run the harness once to produce the golden `.binpb`
   - Commit both files under `tests/cases/nop_transfer/`

6. **Write `tests/prestate.rs`**:
   ```rust
   use std::path::PathBuf;
   use base_firehose_tests::{assert_block_equals_golden, run_prestate};

   #[test]
   fn nop_transfer() {
       let folder = case_dir("nop_transfer");
       let outcome = run_prestate(&folder).expect("nop_transfer prestate must succeed");
       let golden = folder.join("block.<num>.binpb");
       assert_block_equals_golden(&outcome.block, &golden).expect("captured block must match golden");
   }

   fn case_dir(name: &str) -> PathBuf {
       PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("cases").join(name)
   }
   ```

7. **`Cargo.toml` dependencies** for the new crate:

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
   # base
   base-execution-firehose.workspace = true
   base-execution-chainspec.workspace = true
   base-common-consensus.workspace = true
   base-execution-evm.workspace = true

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

   # revm
   revm.workspace = true

   # encoding & errors
   hex.workspace = true
   eyre.workspace = true
   prost.workspace = true
   base64.workspace = true
   serde.workspace = true
   serde_json.workspace = true

   [[test]]
   name = "prestate"
   path = "tests/prestate.rs"
   ```

   Note: `alloy_op_consensus` may need to be added to workspace deps if not present — check first with `grep "alloy-op-consensus" Cargo.toml`.

### Key Implementation Notes

#### Checking `BaseEvmConfig` constructor
The `BaseEvmConfig` struct in `crates/execution/evm/src/lib.rs` line 135 is generic — look at how it is instantiated in the node setup (e.g., `crates/execution/node/`) to find the concrete type parameters used for mainnet. For the test harness, a minimal variant with `BaseEvmFactory` and no op-specific network upgrades tracking may suffice.

#### OP chain config fields for `firehose_tracer::config::ChainConfig`
The `ChainConfig` struct has: `chain_id`, `shanghai_time`, `cancun_time`, `prague_time`, `verkle_time`. For OP Stack Base chains, `shanghai_time` corresponds to `canyon` (which activates Shanghai EIPs on L2), and `cancun_time` to `ecotone`. Pass these from the genesis OP config. The `prague_time` maps to `isthmus` on Base. Set `verkle_time: None`.

#### OP transaction decode
`alloy_op_consensus::OpTxEnvelope` implements `Decodable2718`. Use:
```rust
use alloy_eips::eip2718::Decodable2718;
let tx = OpTxEnvelope::decode_2718(&mut tx_bytes.as_slice())?;
```
Then wrap it into the type expected by `RecoveredBlock` — look at `TxTy<BasePrimitives>` to confirm.

#### `firehose_tracer::config::ChainConfig` fields
The `ChainConfig` in `firehose-tracer` 5.x may have more or different fields than assumed above. Verify by checking the `firehose-tracer` crate docs/source at version `5.0.0`. The chain config fields must match what the tracer expects to correctly classify fork boundaries.

### Decisions & Assumptions

| Decision/Assumption | Rationale |
|---|---|
| New crate under `crates/execution/firehose-tests/` | Consistent with project layout; `base-` prefix matches convention |
| Start with one test case (`nop_transfer`) | Minimal viable harness; more cases easy to add |
| Use `OpPreTxAdjust` / `OpPostTxExtras` from `base-execution-firehose` | Already implemented and tested for OP Stack |
| Goldens are committed binary files (`.binpb`) | Same pattern as reference; no runtime generation |
| `UPDATE_GOLDENS` env-var convenience in `assert_block_equals_golden` | Standard pattern for regenerating goldens without code changes |
| No `#[ignore]` or feature-flag on tests | Tests should be fast (no I/O besides local files) and run in CI |

---

## State Tracker

**Last Updated:** 2026-05-13
**Current Step:** Phase 5 — Spec Review & Acceptance
**Status:** Spec complete, awaiting user approval

| Step | Status | Notes |
|---|---|---|
| Phase 1 — Contextual Understanding | Done | Explored execution/firehose, common/evm, common/consensus, execution/chainspec; fetched reference lib.rs + prestate.rs + Cargo.toml from reth |
| Phase 2 — Gap Analysis | Done | Key gaps: OP tx decode, BaseChainSpec construction, ChainConfig mapping |
| Phase 3 — Challenging Dialogue | Skipped | Sufficient info from codebase + reference to write spec without questions |
| Phase 4 — Specification Writing | Done | Full spec written above |
| Phase 5 — Spec Review | In Progress | Awaiting user approval |
