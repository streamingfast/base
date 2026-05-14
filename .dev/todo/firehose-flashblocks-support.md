# Firehose Flashblocks Support

mode: feature
state: planned
root_git: .worktrees/feature/firehose-flashblocks-support
worktree: .worktrees/feature/firehose-flashblocks-support
branch: feature/firehose-flashblocks-support
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

**PLAN ONLY** We plan together, DO NOT START IMPLEMENTATION

### Context

We have for base-geth a full Flashblocks support on using op-geth Firehose tracing node at https://github.com/streamingfast/go-ethereum/tree/release/optimism-1.x-fh3.0.

This receives via WebSocket the Flashblocks base + deltas, for each base, get the correct state for block's parent hash. Re-apply the base block on top of that, re-execution transactions and tracing them and sending a `FIRE ...` signal with the partial block's base

For each delta that is then receive, validate first that delta follows correctly previous delta or base, re-execute on top of base state's the delta's transactions, tracing them along the way. Then the traced transaction are append to previously traced data (so we have a Firehose Traced block containing base + delta(s) tracing) and then send the new bigger partial blocks our with a `FIRE ...` signal.

You can all our implementation at https://github.com/streamingfast/go-ethereum/tree/release/optimism-1.x-fh3.0/node/flashblock and the important files are the `processor.go` and `controller.go`.

### Task

The whole idea is to port our Flashblocks support back into `base-reth` directly.

Now, the good news is that Base Rust has already a full Flashblocks support that can be found at:
- crates/execution/flashblocks

This contains websocket code, payload structs but also all the logic to execute flashblock deltas and keeping a partial state that is then used by RPC logic to fetch more "recent" state as well as a few other features.

The plan that we need to define is about how we can re-use existing flashblocks code Base has but with respecting more Firehose desired behavior.

I can see for example that we can most probably re-use the Websocket client, the payload and a few other structures and pieces of code like the one that validate that a delta is valid in the sense that it strictly follows previous delta/base.

Stuff I think we would no-reuse:
- Actual execution of base/deltas and state accumulation, we need execution but with our our traced execution flow.

My own thinking is that we have our own firehose_flashblock_url config. If set, it starts our own "streamer" of Flashblock events, and on each event, would replicate what we had in Geth on how we manage the flow of events how we execute them etc. Of course this woulb be using Reth APIs as well as using Base Reth Firehose Block Executor to ensure we can trace block correctly.

### Goal

Define the best plan possible that ports the logic we had in op-geth for handling events, trace them and propagate them properly.

The `evm-firehose-tracer-rs` dependency (`firehose-tracer`) might requires some adjustement, if it's the case, this plan should have a section that explains what it needs so I can make that work properly.

## Dev Feedback

## Spec & Implementation

### Summary

Port the Firehose flashblock tracing support from `streamingfast/go-ethereum` (op-geth) into
`base-reth`. The feature adds a new `--firehose-flashblocks-url <WS_URL>` CLI flag to the node
binary. When set, a dedicated background streamer subscribes to the flashblock WebSocket feed,
re-executes each flashblock (base + deltas) through the existing Firehose tracing infrastructure
(`OpFirehoseEvmConfig` / `FirehoseWrappedExecutor` / `OpChainHooks`), and emits partial-block
Firehose events after each flashblock. When a block is finalized via the normal engine-API path,
the already-emitted partial traces are superseded by the canonical full-block trace.

This is **separate** from the existing `--flashblocks-url` flag (which drives the RPC-pending-state
feature). The new flag has a different purpose: it feeds the Firehose trace output pipeline.

---

### Scope

**In scope:**

- New `--firehose-flashblocks-url <WS_URL>` CLI argument in `bin/node/src/cli.rs`.
- New `base-execution-firehose-flashblocks` library crate (or additional module in
  `base-execution-firehose`) containing the Firehose flashblock processor.
- WebSocket subscriber that reuses `FlashblocksSubscriber` from `base-flashblocks`.
- Sequence validation reusing `FlashblockSequenceValidator` and `CanonicalBlockReconciler` from
  `base-flashblocks`.
- Block assembly reusing `BlockAssembler` from `base-flashblocks`.
- Traced block execution using `OpFirehoseEvmConfig` / `FirehoseWrappedExecutor::with_hooks` +
  `OpPreTxAdjust` + `OpPostTxExtras` (same hooks as the engine-API live path in `validator.rs`).
- Incremental Firehose event emission: one partial-block event per flashblock received (base + each
  delta), using the `FlashBlock` field on `tracing::BlockEvent`.
- State accumulation: carry forward `reth_revm::State<StateProviderDatabase>` across flashblocks
  of the same block so each delta only re-executes its new transactions on top of the accumulated
  state (matching the Geth `StateProcessor` incremental approach).
- Reconnect/backoff logic inherited from `FlashblocksSubscriber`.
- Metric instrumentation (processing duration, error counts) following the existing `Metrics`
  pattern in `base-flashblocks`.
- Integration into `bin/node/src/main.rs` alongside the existing `FirehoseExtension`.

**Out of scope:**

- Modifying the existing `FlashblocksExtension` / `StateProcessor` / `PendingBlocks` RPC path.
- Any changes to the `reth-firehose` git dependency (unless `firehose-tracer` needs adjustment —
  see §firehose-tracer changes below).
- Canonical block reconciliation / reorg replay (the existing engine-API path already handles
  canonical blocks; the flashblock tracer is purely additive and pre-canonical).
- Testing against a live flashblock feed (unit tests only for the processor logic).

---

### Key Design Decisions

| Decision | Rationale |
|---|---|
| Separate CLI flag `--firehose-flashblocks-url` | The existing `--flashblocks-url` flag drives RPC pending state. The two features are independent (one node may need one, the other, or both). |
| New crate vs. module in `base-execution-firehose` | Prefer a new module/file inside `base-execution-firehose` (or a small new crate `base-execution-firehose-flashblocks`) to avoid bloating the existing crate and to keep flashblocks-specific deps isolated. Lean toward a new module in `base-execution-firehose` first since the boundary is thin. |
| Reuse `FlashblocksSubscriber` | WS connection management + reconnect is already battle-tested; no need to rewrite. The subscriber calls `FlashblocksReceiver::on_flashblock_received` — we implement that trait on our new processor. |
| Reuse `BlockAssembler` | Converts `Vec<Flashblock>` → `AssembledBlock` (header + block body). We need the same thing to construct the block for EVM execution. |
| Reuse `FlashblockSequenceValidator` | Stateless; validates that each incoming flashblock follows the expected sequence. Reuse as-is. |
| Do NOT reuse `StateProcessor` or `PendingStateBuilder` | Those accumulate EVM state for RPC, not for traced execution. We need `FirehoseWrappedExecutor` which uses a different execution path. |
| State accumulation across deltas | Like Geth's `StateProcessor`, we keep a `reth_revm::State<StateProviderDatabase>` open across flashblocks of the same block. On each delta we only execute new transactions, building on the accumulated DB state. On a new base (index 0) we open a fresh `State` from the canonical parent. |
| Emit partial block after each flashblock | Matches Geth behavior: `tracer.OnBlockStart(BlockEvent { FlashBlock: &FlashBlock{Block, Idx} })` then execute txs, then `tracer.OnBlockEnd(nil)`. The `reth-firehose` crate already supports this through `FirehoseBlockTracer::start` which accepts a `FlashBlock` parameter. |
| Only execute on Firehose-enabled runs | Guard the whole streamer startup behind `reth_firehose::is_tracer_initialized()`. If Firehose is not initialized, the flag is a no-op (with a warning log). |

---

### reth-firehose / firehose-tracer Required Changes

Reviewing the current call sites in `validator.rs` and `evm_config.rs`, the live path uses:

```rust
reth_firehose::FirehoseBlockTracer::start::<OpPrimitives>(&sealed, None)
```

The second argument `None` is the finalized block. For flashblocks we need to pass a non-`None`
flashblock index. Looking at the Geth call: `tracer.OnBlockStart(BlockEvent{FlashBlock: &FlashBlock{Block, Idx}})`.

**Likely needed from `reth-firehose`:**

1. `FirehoseBlockTracer::start` (or a sibling `start_flashblock`) must accept a
   `Option<FlashBlockInfo>` parameter where `FlashBlockInfo { index: u64 }`. This maps to the
   Geth `tracing.FlashBlock { Block, Idx }`.
2. The `mark_verified()` path (which calls `on_block_end(None)`) must work for flashblock partial
   emissions. For a non-final flashblock we want to emit the partial event immediately without
   waiting for state-root verification. This may require a new `mark_partial()` or
   `mark_flashblock()` method on `FirehoseBlockTracer` that calls `on_block_end` immediately
   without the "verified" semantics.

**If `reth-firehose` already supports flashblock indices in `OnBlockStart`:** no changes needed;
we just pass the index. This needs to be verified by inspecting the `streamingfast/reth` source.

**Assumption:** We assume the implementor will check the `streamingfast/reth` `reth-firehose` crate
source (tag `v1.11.4-fh-1`) to determine whether `FirehoseBlockTracer::start` already accepts a
flashblock index, and will coordinate with the author (`maoueh`) to add the needed API if missing.

---

### Architecture Overview

```
bin/node/src/main.rs
  │
  ├── FirehoseExtension   (existing – installs ExEx for canonical block tracing)
  └── FirehoseFlashblocksExtension  (new)
        └── starts FirehoseFlashblocksStreamer::new(ws_url, provider).start()

crates/execution/firehose/src/
  ├── evm_config.rs        (existing – OpFirehoseEvmConfig, OpChainHooks)
  ├── extras.rs            (existing – OpPreTxAdjust, OpPostTxExtras)
  └── flashblocks/         (new module)
        ├── mod.rs
        ├── processor.rs   (FirehoseFlashblocksProcessor — core logic)
        └── streamer.rs    (FirehoseFlashblocksStreamer — wires subscriber → processor)
```

The `FirehoseFlashblocksProcessor` implements `FlashblocksReceiver` so it can be plugged into the
existing `FlashblocksSubscriber<Receiver>`.

---

### Implementation Plan

**Step 1 — CLI flag**

- In `bin/node/src/cli.rs`: add `--firehose-flashblocks-url <URL>` (`Option<Url>`).
- In `bin/node/src/main.rs`: if `args.firehose_flashblocks_url.is_some()` and
  `reth_firehose::is_tracer_initialized()`, install the new extension.

**Step 2 — New module `crates/execution/firehose/src/flashblocks/`**

Create three files:

- `mod.rs` — module doc + re-exports
- `processor.rs` — `FirehoseFlashblocksProcessor`
- `streamer.rs` — `FirehoseFlashblocksStreamer`

Update `crates/execution/firehose/src/lib.rs` to add:

```rust
mod flashblocks;
pub use flashblocks::{FirehoseFlashblocksProcessor, FirehoseFlashblocksStreamer};
```

Add `base-flashblocks` as a dependency of `base-execution-firehose` in its `Cargo.toml`.

**Step 3 — `FirehoseFlashblocksProcessor`**

```rust
// crates/execution/firehose/src/flashblocks/processor.rs

pub struct FirehoseFlashblocksProcessor<Client> {
    client: Client,
    // Mutable state protected by Mutex (called from the subscriber task)
    inner: Mutex<ProcessorState<Client>>,
}

struct ProcessorState<Client> {
    // Current in-flight block's accumulated EVM state (None = no block in progress)
    current_block_number: Option<u64>,
    // Number of flashblocks already emitted for the current block
    latest_flashblock_index: Option<u64>,
    // Accumulated revm State carrying all prior flashblock transitions
    // Wrapped in Option so we can take() it on block reset
    accumulated_db: Option<State<StateProviderDatabase<Box<dyn StateProvider>>>>,
    // Accumulated gas used across flashblocks of the current block
    cumulative_gas_used: u64,
    // Accumulated receipts (for partial-block validation)
    // ... (we may or may not need these depending on firehose-tracer API)
    _phantom: PhantomData<Client>,
}
```

`FirehoseFlashblocksProcessor` implements `FlashblocksReceiver`:

```rust
impl<Client> FlashblocksReceiver for FirehoseFlashblocksProcessor<Client>
where Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec<Header=Header> + Upgrades>
            + BlockReaderIdExt<Header=Header> + Clone + 'static
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        if let Err(e) = self.inner.lock().process(flashblock, &self.client) {
            error!(error = %e, "firehose flashblock processing failed");
        }
    }
}
```

`ProcessorState::process` flow:

```
fn process(flashblock: Flashblock, client: &Client) -> Result<(), Error>:
  1. Validate sequence using FlashblockSequenceValidator:
     - If Duplicate → warn + return Ok(())
     - If InvalidNewBlockIndex / NonSequentialGap → error + reset + return Ok(())
     - If FirstOfNextBlock or NextInSequence → continue
  2. If index == 0 (new base):
     a. Fetch canonical parent header (block_number - 1) from client
     b. If parent not found → warn + return Ok(()) (not yet synced)
     c. Open fresh State<StateProviderDatabase> at parent block
     d. Reset accumulated_db, cumulative_gas_used, etc.
     e. current_block_number = flashblock.metadata.block_number
     f. latest_flashblock_index = Some(0)
  3. Assemble block from [flashblocks_seen_so_far]: 
     - We need to track the flashblocks for the current block as they arrive
     - On index 0: stored_flashblocks = vec![flashblock]
     - On delta: stored_flashblocks.push(flashblock); then assemble from stored_flashblocks
     - Actually, for incremental execution we only need the NEW txs from this flashblock
       (the delta's transactions), not a full re-assembly each time
  4. Determine which transactions are new this flashblock:
     - On index 0: all transactions in the base flashblock
     - On delta: transactions from this flashblock's diff only (flashblock.diff.transactions)
  5. Decode new transactions (RLP → BaseTxEnvelope), recover senders
  6. Build EVM env using OpNextBlockEnvAttributes (from stored base metadata):
     - Only needed once per block (index 0); cache the env or rebuild from stored data
  7. Execute new transactions through FirehoseWrappedExecutor:
     a. Create inspector: tracer.inspector() where tracer = FirehoseBlockTracer::start_flashblock(...)
        This emits OnBlockStart(FlashBlock{block, index})
     b. evm = evm_config.evm_with_env_and_inspector(&mut accumulated_db, evm_env, inspector)
     c. executor = BaseBlockExecutor::new(evm, ctx, chain_spec, receipt_builder)
     d. wrapped = FirehoseWrappedExecutor::with_hooks(executor, withdrawals, OpPreTxAdjust, OpPostTxExtras)
     e. If index == 0: wrapped.apply_pre_execution_changes() (EIP-4788, EIP-2935, create2 deployer)
     f. For each new tx: wrapped.execute_transaction(recovered_tx)
     g. wrapped.finish() → accumulates result into accumulated_db
     h. tracer.mark_flashblock() → emits OnBlockEnd(None) for this partial block
  8. Update cumulative_gas_used, latest_flashblock_index
```

**Step 4 — Block-level EVM state bookkeeping details**

The key insight from the Geth code: the `StateDB` is opened once per block (at parent state root)
and mutated incrementally. Transactions committed to it persist across deltas. In Reth terms:

- `State::builder().with_database(StateProviderDatabase::new(parent_state_provider)).with_bundle_update().build()`
- After each flashblock's transactions, call `db.commit(state)` for each tx result (this is what
  `execute_with_evm` in `PendingStateBuilder` does). The `State<DB>` accumulates the bundle.
- We do NOT call `db.merge_transitions` between flashblocks — that would collapse the bundle.
  Only at block end (when canonical block arrives via engine-API) do we discard this state anyway.
- Re-using the existing `accumulated_db` for the next flashblock's execution avoids the full
  re-execution of all prior transactions on each delta (unlike the RPC `StateProcessor` which
  rebuilds from scratch on each delta using `prev_pending_blocks` bundle prestate).

**Step 5 — FirehoseBlockTracer integration**

For each flashblock emission, the flow is:
```
let tracer = FirehoseBlockTracer::start_flashblock::<BasePrimitives>(
    &assembled_sealed_block,
    finalized_header,
    flashblock_index,
);
// execute transactions through the inspector...
tracer.mark_flashblock(); // emits partial OnBlockEnd
```

The `assembled_sealed_block` is built from `BlockAssembler::assemble(&stored_flashblocks_so_far)`.
Its block hash is `B256::ZERO` (unknown until finalized) — this matches what `BlockAssembler`
already does (`seal(B256::ZERO)`).

**If `reth-firehose` does not yet have `start_flashblock` / `mark_flashblock`:** the implementor
must:
1. Add `FirehoseBlockTracer::start_flashblock(sealed_block, finalized, index: u64) -> Self` that
   calls `on_block_start` with the flashblock index.
2. Add `FirehoseBlockTracer::mark_flashblock(self)` that calls `on_block_end(None)` immediately
   (no state-root gate).
3. Update `firehose-tracer` if needed to support the `FlashBlock { index }` field in
   `OnBlockStart`.

**Step 6 — `FirehoseFlashblocksStreamer`**

```rust
pub struct FirehoseFlashblocksStreamer<Client> {
    processor: Arc<FirehoseFlashblocksProcessor<Client>>,
    ws_url: Url,
}

impl<Client: ...> FirehoseFlashblocksStreamer<Client> {
    pub fn new(ws_url: Url, client: Client) -> Self { ... }
    
    pub fn start(&self) {
        let mut subscriber = FlashblocksSubscriber::new(
            Arc::clone(&self.processor),
            self.ws_url.clone(),
        );
        subscriber.start();
    }
}
```

**Step 7 — `FirehoseFlashblocksExtension` in `bin/node/src/firehose.rs`**

```rust
pub struct FirehoseFlashblocksExtension {
    ws_url: Url,
}

impl BaseNodeExtension for FirehoseFlashblocksExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.on_node_started(move |ctx| {
            let provider = ctx.provider().clone();
            let streamer = FirehoseFlashblocksStreamer::new(self.ws_url, provider);
            streamer.start();
            Ok(())
        })
    }
}
```

(The exact hook API — `on_node_started` vs. `add_started_callback` — depends on what
`NodeHooks` / `BaseNodeExtension` exposes; the implementor should align with the existing
extension pattern in `runner.rs`.)

**Step 8 — `bin/node/src/main.rs` wiring**

```rust
// After FirehoseExtension is installed:
if let Some(url) = args.firehose_flashblocks_url {
    if reth_firehose::is_tracer_initialized() {
        runner.install_ext::<FirehoseFlashblocksExtension>(url);
    } else {
        warn!("--firehose-flashblocks-url is set but Firehose tracer is not initialized; ignoring");
    }
}
```

**Step 9 — Cargo.toml changes**

`crates/execution/firehose/Cargo.toml` — add:
```toml
base-flashblocks.workspace = true
base-execution-evm.workspace = true   # for OpNextBlockEnvAttributes, extract_l1_info
base-node-core.workspace = true       # for OpRethReceiptBuilder, BaseBlockExecutor
```

`bin/node/Cargo.toml` — no additional deps needed (already has `reth-firehose` and
`base-execution-firehose`).

---

### Decisions & Assumptions

| Decision/Assumption | Rationale |
|---|---|
| The `FlashblocksReceiver` trait + `FlashblocksSubscriber` are reused as-is | They live in `base-flashblocks` and are generic over the receiver. Zero duplication. |
| `BlockAssembler::assemble` is used to build the partial block for the tracer | It already handles conversion of `Vec<Flashblock>` → `BaseBlock` + sealed header with `B256::ZERO` hash. |
| We use `FlashblockSequenceValidator` for sequence validation | Stateless, battle-tested logic. We mirror the Geth controller's validation logic. |
| Pre-execution changes (EIP-4788, create2 deployer) are only applied once per block (on index 0) | Matches Geth `StateProcessor.Process` `isFirstExecution` gate. |
| The Firehose ExEx (canonical block tracing) is not touched | It operates on finalized chains; flashblock tracing is additive and operates on in-flight blocks. They coexist without conflict. |
| `reth-firehose` needs a `start_flashblock` / `mark_flashblock` API | Assumption; implementor must verify by reading the `streamingfast/reth` source at tag `v1.11.4-fh-1`. If it already has flashblock support, Steps 5's API calls may need to be adjusted accordingly. |
| No canonical-block reconciliation in the flashblock processor | The canonical path (ExEx / `FirehoseBlockExecutor`) already handles canonical blocks. The flashblock processor only needs to reset on a new base (index 0). |
| Incremental state (not full re-execution) | We carry `accumulated_db` forward across deltas to avoid O(N²) cost. This matches the Geth approach exactly. |
| Error handling: log + skip (don't crash) | Matches the Geth controller which calls `c.state.Skipping = true` and logs but keeps running. |

---

### Open Questions for Implementor

1. **`reth-firehose` API**: Does `FirehoseBlockTracer::start` at tag `v1.11.4-fh-1` already
   support a flashblock index parameter? If not, coordinate with `maoueh` to add
   `start_flashblock(sealed, finalized, index)` + `mark_flashblock()`.

2. **`firehose-tracer` protobuf**: Does `OnBlockStart` in `firehose-tracer 5.0.0` accept a
   flashblock index? The Go tracer has `tracing.FlashBlock{Block, Idx}`. Verify whether the Rust
   `firehose-tracer` crate's `BlockEvent` / `on_block_start` already carries the flashblock index.
   If not, `firehose-tracer` will need a new version with the `flash_block_index: Option<u64>`
   field on `BlockEvent`.

3. **`NodeHooks` API for late startup**: `FirehoseFlashblocksStreamer` needs access to the
   provider (to fetch parent state). The `on_node_started` / `add_started_callback` hooks in
   `runner.rs` currently only get `()` from `BaseNodeRunner::add_started_callback`. The
   `BaseNodeExtension::apply` hooks may get a richer context. Implementor should check
   `extension.rs` and `builder.rs` to confirm how to obtain the provider after node initialization,
   or consider passing the `BaseProvider` through the extension config.

---

## State Tracker

**Last Updated:** 2026-05-14 UTC
**Current Step:** Phase 5 — Spec Accepted / Planned
**Status:** Plan complete; awaiting implementation

| Step | Status | Notes |
|---|---|---|
| Phase 1 — Contextual Understanding | Done | Explored flashblocks, firehose, engine-tree, runner, bin/node crates |
| Phase 2 — Gap Analysis | Done | Identified: reth-firehose API gaps, state accumulation strategy, CLI wiring |
| Phase 3 — Challenging Dialogue | Skipped | Sufficient context from codebase; no blocking ambiguities |
| Phase 4 — Specification Writing | Done | Full spec written above |
| Phase 5 — Spec Review | Done | Marked planned |
