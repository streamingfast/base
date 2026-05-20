# Firehose Flashblocks Support

mode: feature
state: in_progress
root_git: .worktrees/feature/firehose-flashblocks-support
worktree: .worktrees/feature/firehose-flashblocks-support
branch: feature/firehose-flashblocks-support
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

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

Ok we have implemented the necesary changes requested in plan on `streamingfast/reth`, update to `branch firehose/1.x` to benefit from it. Adjust plan now with this new information

## Spec & Implementation

### Summary

Port the Firehose flashblock tracing support from `streamingfast/go-ethereum` (op-geth) into
`base-reth`. The feature adds a new `--firehose-flashblocks-url <WS_URL>` CLI flag. When set
(non-empty), a dedicated background streamer subscribes to the flashblock WebSocket feed, re-executes
each flashblock (base + deltas) through a **dedicated** Firehose tracer instance (separate from the
global live-block tracer), and emits partial-block Firehose events after each flashblock. When the
canonical block eventually arrives via the engine-API path, the already-emitted partial traces are
superseded by the canonical full-block trace as usual.

The feature lives in a new top-level crate `crates/firehose-flashblocks/` named
`base-firehose-flashblocks`. If `--firehose-flashblocks-url` is absent or empty, the entire
subsystem is disabled and has zero runtime cost.

---

### Scope

**In scope:**

- New `--firehose-flashblocks-url <WS_URL>` CLI argument in `bin/node/src/cli.rs`.
- New crate `crates/firehose-flashblocks/` (`base-firehose-flashblocks`) containing all Firehose+flashblocks logic.
- WebSocket subscriber reusing `FlashblocksSubscriber<Receiver>` + `FlashblocksReceiver` trait from `base-flashblocks`.
- Sequence validation reusing `FlashblockSequenceValidator` and `CanonicalBlockReconciler` from `base-flashblocks`.
- Block assembly reusing `BlockAssembler` from `base-flashblocks`.
- Traced block execution using `OpFirehoseEvmConfig` / `FirehoseWrappedExecutor::with_hooks` + `OpPreTxAdjust` + `OpPostTxExtras` from `base-execution-firehose`.
- Incremental Firehose event emission: one partial-block event per flashblock (base + each delta).
- **Cross-block state accumulation**: the accumulated EVM `State<DB>` is carried across flashblocks within the same canonical block AND across blocks (so Block N+1's base can start directly from Block N's completed accumulated state, without waiting for the canonical `StateProvider` to reflect Block N).
- A dedicated (non-global) `firehose_tracer::Tracer` instance owned by the flashblock processor, with **stdout write coordination** between the global (live) tracer and the flashblock tracer.
- Reconnect/backoff logic inherited from `FlashblocksSubscriber`.
- Integration into `bin/node/src/main.rs` alongside the existing `FirehoseExtension`.

**Out of scope:**

- Modifying the existing `FlashblocksExtension` / `StateProcessor` / `PendingBlocks` RPC path.
- Canonical block reconciliation / reorg replay in the flashblock processor (the engine-API path handles canonical blocks; the flashblock tracer is purely additive and pre-canonical).
- Testing against a live flashblock feed (unit tests only for the processor logic).

---

### Key Design Decisions

| Decision | Rationale |
|---|---|
| New crate `base-firehose-flashblocks` | Keeps all Firehose+flashblocks logic isolated; avoids bloating `base-execution-firehose`. The boundary is substantial enough to warrant a crate. |
| Separate CLI flag `--firehose-flashblocks-url` | The existing `--flashblocks-url` drives RPC pending state. The two features are independent and may be used independently. |
| **Dedicated (non-global) tracer instance** | The global tracer is locked by the live-block execution path. Two concurrent executions using the same tracer would corrupt output. The flashblock processor owns its own `firehose_tracer::Tracer`. |
| **Stdout write coordination via a shared Mutex** | Both the global tracer and the flashblock tracer write to stdout. Lines must not interleave. A process-wide `Mutex<()>` guards each `write_all` call; both tracer instances must acquire it before flushing a line. This mutex is already installed on `firehose/1.x` via `reth_firehose::init_stdout_lock()` and is wired into the global tracer automatically by `reth_firehose::init_tracer(Config)`. The flashblock tracer obtains the same `Arc<Mutex<()>>` via `reth_firehose::stdout_lock()`. |
| **Cross-block state accumulation** | The processor keeps `accumulated_db: State<StateProviderDatabase>` alive across flashblocks of the same block AND across blocks. On a new base (index 0), if the current block number equals `previous_block_number + 1`, the state is carried forward directly (no provider lookup needed). If there is a gap (skipped blocks, restart), a fresh `StateProvider` is fetched for the parent. |
| Reuse `FlashblocksSubscriber` | WS connection management + reconnect is already battle-tested. The subscriber calls `FlashblocksReceiver::on_flashblock_received` — we implement that on `FirehoseFlashblocksProcessor`. |
| Reuse `BlockAssembler` | Converts accumulated `Vec<Flashblock>` → `AssembledBlock` (header + body) needed for EVM env construction and `FirehoseBlockTracer` initialization. |
| Reuse `FlashblockSequenceValidator` | Stateless; validates that each incoming flashblock follows the expected sequence. |
| Do NOT reuse `StateProcessor` or `PendingStateBuilder` | Those accumulate EVM state for RPC, not for traced execution. We need `FirehoseWrappedExecutor`. |
| Emit partial block after each flashblock | Matches Geth behavior: `on_block_start(BlockEvent{flash_block: Some(FlashBlockData{idx, is_final})})` → execute txs → `on_block_end(None)` for this partial. |
| Only execute when Firehose is enabled | Guard the entire streamer startup behind `reth_firehose::is_tracer_initialized()`. If not initialized, the flag is a no-op (with a warning log). |

---

### Cross-Block State Accumulation — Edge Cases (Ultra-Think)

This is the most complex part of the design. The fundamental challenge: flashblock events arrive
**before** the canonical chain updates its in-memory `StateProvider`. We may receive Block 2 base
& several deltas before the node's state provider reflects Block 1 as finalized.

#### State Machine

The processor maintains:

```
current_block_number: Option<u64>
accumulated_db: Option<State<StateProviderDatabase<Box<dyn StateProvider>>>>
stored_flashblocks: Vec<Flashblock>   // all flashblocks for current block
latest_flashblock_index: Option<u64>
is_skipping: bool                      // set on errors; cleared on new base
```

#### Event Flows

**Happy path — sequential blocks:**

```
Event: Block N, index 0 (base)
  → if current_block_number == Some(N-1): carry accumulated_db forward
    (the state already reflects all of Block N-1's transactions)
  → if current_block_number == None or Some(M) where M != N-1:
    → must bootstrap from canonical StateProvider at block N-1
    → if StateProvider not yet available for N-1: WAIT (see below)
  → reset stored_flashblocks = [flashblock], latest_flashblock_index = Some(0)
  → apply pre-execution changes (EIP-4788, create2 deployer) on fresh execution context
  → execute base transactions, emit partial block (idx=0, is_final=false)
  → update accumulated_db

Event: Block N, index K (delta, K > 0)
  → validate sequence: NextInSequence expected
  → execute ONLY the new transactions from this delta on accumulated_db
  → emit partial block (idx=K, is_final=false or true if last known)
  → update accumulated_db
```

**Edge case 1 — StateProvider not yet available:**

When we receive Block N base (index 0) but `StateProvider` for block N-1 is not yet available
(canonical chain hasn't caught up), we must wait. Strategy:

- Before processing the base event, attempt `client.state_by_block_hash(parent_hash)` or
  `client.latest()` and check if it reflects block N-1.
- If not available: **park the event** in a `pending_base: Option<Flashblock>` field, spawn a
  retry task (e.g., loop with 5ms sleep up to 2 seconds). If timeout exceeded: log warning, set
  `is_skipping = true`, discard this base event. Deltas for this block will also be discarded
  (sequence validator returns `NextInSequence` but `is_skipping = true` causes immediate return).
- If the carried `accumulated_db` from the previous block is available (normal sequential case):
  skip the provider lookup entirely — this is the fast path.

**Edge case 2 — gap in block numbers (restart / reorg):**

If `current_block_number == Some(M)` and the new base is for block N where N ≠ M+1:
- Drop `accumulated_db`.
- Attempt to fetch `StateProvider` at block N-1 from the canonical chain.
- Same timeout/retry logic as edge case 1.
- If N < M: this could be a reorg; treat as a fresh start. Log a warning.

**Edge case 3 — duplicate base (index 0 for same block number):**

- `FlashblockSequenceValidator` returns `Duplicate`. Log + skip.

**Edge case 4 — non-sequential gap within a block:**

- `FlashblockSequenceValidator` returns `NonSequentialGap`. Log warning, set `is_skipping = true`
  for this block. All subsequent deltas for this block are discarded. On the next base event
  (new block number), `is_skipping` is cleared and we attempt fresh bootstrap.

**Edge case 5 — processor falls behind (slow execution):**

- The WS subscriber channel may accumulate events. The processor must process events sequentially
  (single-threaded inner loop). If the WS feed gets too far ahead, we will observe
  `NonSequentialGap` (missed deltas) which triggers `is_skipping`. This is acceptable: we emit
  whatever partial blocks we can and skip the rest. The canonical block is still traced correctly.

**Edge case 6 — is_final semantics:**

- The `FlashBlockData.is_final` field tells the downstream consumer whether this is the last
  flashblock for this block number. The flashblock WS protocol encodes "final" blocks differently
  (the last delta has a specific marker). The processor must inspect the incoming `Flashblock`
  to determine if this is the final delta and set `is_final` accordingly when building `FlashBlockData`.

**Edge case 7 — accumulated_db corruption on execution error:**

- If `FirehoseWrappedExecutor` returns an error mid-execution, the `accumulated_db` may be in a
  partially-mutated state. To avoid carrying corrupt state forward, on any execution error:
  - Set `is_skipping = true`.
  - Drop `accumulated_db` (set to `None`).
  - Log the error with block/index fields.
  - The next base event will attempt a fresh bootstrap from the canonical provider.

#### State Accumulation Implementation

```rust
// On successful execution of flashblock transactions:
// We commit the EVM state changes into accumulated_db
// The State<DB> bundle grows with each flashblock's transactions.
// We do NOT call db.merge_transitions() between flashblocks:
//   that would collapse history and break incremental execution.
// We DO NOT call db.take_bundle() until we want to discard the state.
// The natural progression: db.commit(result.state) accumulates all changes.
```

The key: `State<StateProviderDatabase>` in Reth wraps a read-through cache over the provider. After
`db.commit(state_changes)`, subsequent reads from the same address see the committed values. So Block
N+1's execution naturally reads Block N's post-state without needing a new `StateProvider`. This
eliminates the canonical-chain delay for the common sequential case.

---

### New Crate: `base-firehose-flashblocks`

**Location:** `crates/firehose-flashblocks/`

**Cargo.toml name:** `base-firehose-flashblocks`

**Files:**

```
crates/firehose-flashblocks/
  Cargo.toml
  README.md
  src/
    lib.rs              — minimal; re-exports only
    processor.rs        — FirehoseFlashblocksProcessor (core logic)
    streamer.rs         — FirehoseFlashblocksStreamer (wires subscriber → processor)
    tracer.rs           — FlashblocksTracerHandle (dedicated non-global tracer)
    error.rs            — Error enum
    metrics.rs          — Metrics struct (processing duration, error counts, skip counts)
```

**Dependencies (`Cargo.toml`):**

```toml
[dependencies]
# core
tokio.workspace = true
tracing.workspace = true
url.workspace = true

# firehose
reth-firehose.workspace = true
firehose-tracer.workspace = true

# flashblocks (reused components)
base-flashblocks.workspace = true

# execution (for OpFirehoseEvmConfig, hooks, executor)
base-execution-firehose.workspace = true
base-execution-evm.workspace = true      # OpNextBlockEnvAttributes, etc.

# reth primitives/provider
reth-primitives.workspace = true
reth-provider.workspace = true
reth-revm.workspace = true
alloy-primitives.workspace = true

[lints]
workspace = true
```

---

### Upstream Prerequisites (already landed on `streamingfast/reth` `firehose/1.x`)

All the upstream changes that the original plan called for are now merged on the
`firehose/1.x` branch of `streamingfast/reth`. The workspace dependency has been bumped
accordingly:

- `Cargo.toml`: every `git = "https://github.com/streamingfast/reth.git"` line now uses
  `branch = "firehose/1.x"` instead of `tag = "v1.11.4-fh-1"` (72 dependency lines
  + 1 `[workspace.dependencies] reth-firehose` line).
- `Cargo.lock`: regenerated; all `reth-*` crates now resolve to commit `c052fdfe…`
  (head of `firehose/1.x`). The transitive `firehose-tracer` resolution remains
  `5.1.0` (registry), which is the version that already exposes `FlashBlockData`,
  `BlockEvent.flash_block: Option<FlashBlockData>`, and
  `Tracer::new_with_writer(Config, Box<dyn Write + Send>)`.

What the upstream now provides (read directly from
`/tmp/reth-firehose-1x/crates/firehose/src/{lib.rs,block_tracer.rs}`):

#### 1. Stdout write coordination — `crates/firehose/src/lib.rs`

```rust
pub struct SynchronizedStdout { lock: Arc<Mutex<()>> }
impl SynchronizedStdout {
    pub fn new(lock: Arc<Mutex<()>>) -> Self;
}
impl Write for SynchronizedStdout { /* acquires lock, then delegates to stdout */ }

pub fn init_stdout_lock() -> Arc<Mutex<()>>;   // idempotent; returns the shared lock
pub fn stdout_lock() -> Arc<Mutex<()>>;        // panics if init_tracer hasn't run
pub fn is_tracer_initialized() -> bool;
pub fn init_tracer(config: firehose_tracer::config::Config);
pub fn tracer() -> MutexGuard<'static, firehose_tracer::Tracer>;
```

Key semantics:

- `init_tracer` now takes a `firehose_tracer::config::Config` (it used to take a pre-built
  `Tracer`). Internally it calls `init_stdout_lock()`, wraps the lock in
  `SynchronizedStdout`, and builds the global tracer with
  `Tracer::new_with_writer(config, Box::new(SynchronizedStdout::new(lock)))`. The global
  tracer is therefore already serialised against the shared lock.
- Any additional tracer (e.g. our flashblock tracer) must be built **after** `init_tracer`
  has run, using `stdout_lock()` + `SynchronizedStdout::new(...)` + the tracer's
  `new_with_writer` so it shares the same `Arc<Mutex<()>>`.
- When only one tracer is active the lock is uncontested → effectively zero overhead.

#### 2. `FirehoseBlockTracer::start_flashblock_local` — `crates/firehose/src/block_tracer.rs`

```rust
impl<'a> FirehoseBlockTracer<&'a mut firehose_tracer::Tracer> {
    pub fn start_flashblock_local<N>(
        tracer: &'a mut firehose_tracer::Tracer,
        block: &SealedBlock<N::Block>,
        finalized: Option<firehose_tracer::types::FinalizedBlockRef>,
        flash_block_idx: u64,
        is_final: bool,
    ) -> Self
    where
        N: NodePrimitives,
        N::Block: BlockTrait,
        <N::Block as BlockTrait>::Header: BlockHeader + Sealable,
        <N::Block as BlockTrait>::Body: BlockBody,
        <<N::Block as BlockTrait>::Body as BlockBody>::OmmerHeader: BlockHeader + Sealable,
    { /* emits on_block_start with flash_block: Some(FlashBlockData { idx, is_final }) */ }
}
```

Only the `_local` variant exists upstream — there is intentionally **no**
`start_flashblock` (global) constructor. Flashblock emission *always* uses a dedicated
tracer, so the design no longer needs the global-tracer flavor.

#### 3. `FirehoseBlockTracer::mark_flashblock` — same file

```rust
impl<G> FirehoseBlockTracer<G> where G: DerefMut<Target = firehose_tracer::Tracer> {
    pub fn mark_flashblock(mut self) {
        self.guard.on_block_end(None);
        self.status = Status::Consumed;
    }
}
```

Identical to `mark_verified` but without the genesis-skip path (a flashblock never represents
block 1). The doc-comment explicitly tells callers to only invoke this on guards created via
`start_flashblock_local`.

#### 4. `firehose-tracer 5.1.0` is_final wire-protocol behavior

The `is_final` bit on `FlashBlockData` is not a passive label — when `true`, the tracer
encodes the flashblock index as `idx + 1000` in the FIRE BLOCK line
(`printer::compute_printed_flash_block_index`). The downstream Firehose consumer uses this
sentinel to identify the last partial of a block. Our processor must set this bool correctly
based on the WebSocket payload's "final" marker; no extra wire-protocol work is needed.

#### Summary of upstream landed changes (no additional reth-firehose work required)

| File | Symbol | Status |
|---|---|---|
| `crates/firehose/src/lib.rs` | `SynchronizedStdout`, `init_stdout_lock`, `stdout_lock` | Landed |
| `crates/firehose/src/lib.rs` | `init_tracer(Config)` (signature change) | Landed |
| `crates/firehose/src/block_tracer.rs` | `start_flashblock_local` | Landed |
| `crates/firehose/src/block_tracer.rs` | `mark_flashblock` | Landed |

---

### Architecture Overview

```
bin/node/src/main.rs
  │
  ├── FirehoseExtension          (existing – ExEx for canonical block tracing via global tracer)
  └── FirehoseFlashblocksExtension  (new – installed only if --firehose-flashblocks-url is set
        │                             AND is_tracer_initialized())
        └── FirehoseFlashblocksStreamer::new(ws_url, provider).start()
              │
              └── FlashblocksSubscriber<FirehoseFlashblocksProcessor>
                    └── calls processor.on_flashblock_received(flashblock)
                          └── FirehoseFlashblocksProcessor
                                ├── FlashblockSequenceValidator  (reused)
                                ├── BlockAssembler               (reused)
                                ├── accumulated_db: Option<State<DB>>
                                ├── FlashblocksTracerHandle (dedicated tracer)
                                └── emits partial FIRE lines via mark_flashblock()

crates/firehose-flashblocks/src/
  ├── lib.rs
  ├── processor.rs   (FirehoseFlashblocksProcessor)
  ├── streamer.rs    (FirehoseFlashblocksStreamer)
  ├── tracer.rs      (FlashblocksTracerHandle — owns the dedicated Tracer)
  ├── error.rs
  └── metrics.rs
```

---

### Implementation Plan

**Step 0 — Dependency bump (already done in this replan)**

The workspace `Cargo.toml` and `Cargo.lock` have been updated to track `streamingfast/reth`
on `branch = "firehose/1.x"` (commit `c052fdfe` at the time of writing). This brings in:

- `FirehoseBlockTracer::start_flashblock_local`
- `FirehoseBlockTracer::mark_flashblock`
- `reth_firehose::{SynchronizedStdout, init_stdout_lock, stdout_lock, is_tracer_initialized}`
- `init_tracer` now takes `firehose_tracer::config::Config`

`firehose-tracer 5.1.0` (with `FlashBlockData` and `Tracer::new_with_writer`) is pulled
transitively. No additional upstream changes are required for the rest of this plan.

Verify before implementing:

```bash
cd /Users/maoueh/work/sf/base/.worktrees/feature/firehose-flashblocks-support
cargo check -p reth-firehose 2>&1 | tail -10        # should compile clean
cargo doc -p reth-firehose --no-deps --open          # confirm new symbols visible
```

**Step 1 — New crate `crates/firehose-flashblocks/`**

- Create directory structure as described above.
- `Cargo.toml`: name = `"base-firehose-flashblocks"`, add all dependencies listed in §New Crate.
- Add to workspace `Cargo.toml` members list and `[workspace.dependencies]`.
- Write `README.md` (brief description).

**Step 2 — `error.rs`**

Define `Error` enum:

```rust
pub enum Error {
    StateProviderTimeout { block_number: u64, parent_hash: B256 },
    SequenceGap { block_number: u64, expected_index: u64, got_index: u64 },
    BlockAssembly(anyhow::Error),
    Execution(anyhow::Error),
    TransactionDecoding(alloy_rlp::Error),
}
```

**Step 3 — `metrics.rs`**

Following the existing `Metrics` pattern in `base-flashblocks/src/metrics.rs`:

```rust
pub struct Metrics {
    pub flashblocks_processed: Counter,
    pub flashblocks_skipped: Counter,
    pub flashblocks_errors: Counter,
    pub execution_duration: Histogram,
    pub state_bootstrap_duration: Histogram,
}
```

**Step 4 — `tracer.rs` — `FlashblocksTracerHandle`**

Owns a dedicated (non-global) `firehose_tracer::Tracer` for flashblock execution. The
tracer writes through `SynchronizedStdout` so its output is serialised against the global
live-block tracer at the stdout-write level. Must be constructed **after**
`reth_firehose::init_tracer(...)` has been called by the main binary (otherwise
`stdout_lock()` panics).

```rust
use std::io::Write;

pub struct FlashblocksTracerHandle {
    tracer: firehose_tracer::Tracer,
}

impl FlashblocksTracerHandle {
    /// Constructs a dedicated Tracer that shares the process-wide stdout lock with the
    /// global tracer. The supplied `config` should typically mirror the one used for the
    /// global tracer (e.g. same `chain_client`, same emission mode), with the caveat that
    /// the cursor file path must NOT be shared — the flashblock tracer is pre-canonical
    /// and emits separate FIRE BLOCK lines.
    pub fn new(config: firehose_tracer::config::Config) -> Self {
        let lock = reth_firehose::stdout_lock();
        let writer: Box<dyn Write + Send> =
            Box::new(reth_firehose::SynchronizedStdout::new(lock));
        let mut tracer = firehose_tracer::Tracer::new_with_writer(config, writer);
        // on_blockchain_init must be emitted once before any block events — but the global
        // tracer has already emitted it. The downstream consumer is keyed by tracer-id, so
        // we either (a) emit a separate init using a distinct tracer-id, or (b) skip the
        // init and rely on the global one. The decision is captured in the spec below.
        Self { tracer }
    }

    pub fn tracer_mut(&mut self) -> &mut firehose_tracer::Tracer {
        &mut self.tracer
    }
}
```

**On `on_blockchain_init` for the dedicated tracer:** The implementor must inspect
`firehose_tracer::Tracer::on_blockchain_init` semantics. If the downstream Firehose
consumer expects exactly one init per FIRE stream, the flashblock tracer should NOT emit
its own init (the global tracer already did). If init is per-tracer-instance and the
consumer can handle multiple inits with distinct ids, emit one with a separate
tracer-id (e.g. `"reth-flashblock"`). Inspect
`/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/firehose-tracer-5.1.0/src/tracer.rs`
and the firehose-core consumer side to decide; the safe default is to skip init for the
flashblock tracer.

**Step 5 — `processor.rs` — `FirehoseFlashblocksProcessor`**

Core logic. The struct and its `on_flashblock_received` implementation.

```rust
pub struct FirehoseFlashblocksProcessor<Client> {
    client: Client,
    inner: Mutex<ProcessorState>,
    tracer: Mutex<FlashblocksTracerHandle>,
    metrics: Metrics,
}

struct ProcessorState {
    current_block_number: Option<u64>,
    latest_flashblock_index: Option<u64>,
    accumulated_db: Option<State<StateProviderDatabase<Box<dyn StateProvider>>>>,
    stored_flashblocks: Vec<Flashblock>,
    is_skipping: bool,
}
```

The `process` method (called from `on_flashblock_received` under `inner` lock):

```
fn process(&mut self, flashblock: Flashblock, client: &Client, tracer: &mut FlashblocksTracerHandle) -> Result<()>:

  1. Validate sequence:
     - current: (current_block_number, latest_flashblock_index)
     - incoming: (flashblock.metadata.block_number, flashblock.metadata.index)
     - Call FlashblockSequenceValidator::validate(current_num, current_idx, new_num, new_idx)
     - Duplicate → warn + return Ok(())
     - InvalidNewBlockIndex / NonSequentialGap → warn, set is_skipping=true, drop accumulated_db, return Ok(())
     - FirstOfNextBlock or NextInSequence → continue

  2. If is_skipping AND index != 0 → warn + return Ok(()) (still skipping within this block)
     If is_skipping AND index == 0 → clear is_skipping (new block, fresh start)

  3. If index == 0 (new base):
     a. Determine state source:
        - If current_block_number == Some(N-1) where N == flashblock.metadata.block_number:
            FAST PATH: accumulated_db is already at post-Block-N-1 state. Carry it forward.
        - Else (gap, restart, first ever):
            BOOTSTRAP: fetch StateProvider at parent (block N-1) from client.
            Retry loop: attempt client.state_by_block_number(N-1) up to ~20 attempts × 100ms.
            On timeout: warn, set is_skipping=true, return Ok(()).
            On success: State::builder().with_database(StateProviderDatabase::new(provider))
                                       .with_bundle_update().build()
     b. Reset stored_flashblocks = vec![flashblock.clone()], latest_flashblock_index = Some(0)
     c. current_block_number = Some(N)

  4. If index > 0:
     a. stored_flashblocks.push(flashblock.clone())
     b. latest_flashblock_index = Some(index)

  5. Determine is_final from flashblock metadata (inspect the WS protocol's final-block marker field)

  6. Assemble partial block for tracer init:
     assembled = BlockAssembler::assemble(&stored_flashblocks)?
     (assembled.sealed_block has B256::ZERO hash, which is correct for pre-canonical)

  7. Determine new transactions for this flashblock:
     - index == 0: all transactions from the base flashblock
     - index > 0: only transactions in this delta (flashblock.diff.transactions)
     Decode + recover senders.

  8. Initialize per-flashblock tracer guard:
     let tracer_guard = FirehoseBlockTracer::start_flashblock_local(
         tracer.tracer_mut(),
         &assembled.sealed_block,
         None,         // finalized: None for pre-canonical
         index,
         is_final,
     );

  9. Build EVM environment from assembled block's header (using OpNextBlockEnvAttributes or
     equivalent; chain spec from client.chain_spec()).

  10. If index == 0: apply pre-execution changes (EIP-4788, EIP-2935, create2 deployer)
      using the appropriate hook (matches how OpChainHooks does it in the canonical path).

  11. Execute new transactions using FirehoseWrappedExecutor:
      - inspector = tracer_guard.inspector()
      - evm = evm_config.evm_with_env_and_inspector(&mut accumulated_db, evm_env, inspector)
      - For each new_tx: execute through evm + accumulate result into accumulated_db
      On error: tracer_guard.mark_failed(&err), set is_skipping=true, drop accumulated_db, return.

  12. Emit partial block:
      tracer_guard.mark_flashblock()

  13. Update metrics.
```

**Step 6 — `streamer.rs` — `FirehoseFlashblocksStreamer`**

```rust
pub struct FirehoseFlashblocksStreamer<Client> {
    processor: Arc<FirehoseFlashblocksProcessor<Client>>,
    ws_url: Url,
}

impl<Client: ...> FirehoseFlashblocksStreamer<Client> {
    pub fn new(ws_url: Url, client: Client) -> Self {
        let processor = Arc::new(FirehoseFlashblocksProcessor::new(client));
        Self { processor, ws_url }
    }

    pub fn start(self) {
        tokio::spawn(async move {
            let mut subscriber = FlashblocksSubscriber::new(
                Arc::clone(&self.processor),
                self.ws_url,
            );
            subscriber.start().await;
        });
    }
}
```

**Step 7 — CLI flag in `bin/node/src/cli.rs`**

```rust
/// WebSocket URL for the Firehose flashblocks feed.
/// When set, partial Firehose block events are emitted as flashblocks arrive.
/// Disabled (no-op) if empty or if the Firehose tracer is not initialized.
#[arg(long = "firehose-flashblocks-url", default_value = "")]
pub firehose_flashblocks_url: String,
```

**Step 8 — Wiring in `bin/node/src/main.rs`**

```rust
// After FirehoseExtension is installed:
let fb_url = args.firehose_flashblocks_url.trim();
if !fb_url.is_empty() {
    if reth_firehose::is_tracer_initialized() {
        let url = fb_url.parse::<Url>().expect("invalid --firehose-flashblocks-url");
        // Install extension that starts the streamer once the node's provider is ready.
        runner.install_ext(FirehoseFlashblocksExtension::new(url));
    } else {
        warn!("--firehose-flashblocks-url is set but Firehose tracer is not initialized; ignoring");
    }
}
```

**Step 9 — `FirehoseFlashblocksExtension` in `bin/node/src/`**

Following the existing extension pattern (e.g., `runner.rs` / `extension.rs`):

```rust
pub struct FirehoseFlashblocksExtension {
    ws_url: Url,
}

impl FirehoseFlashblocksExtension {
    pub fn new(ws_url: Url) -> Self { Self { ws_url } }
}

// Implement the node extension trait used by runner.install_ext()
// The hook fires after the node is started and a provider handle is available.
// Inspect bin/node/src/runner.rs to find the correct trait and hook point.
impl<Node> RethNodeCommandConfig<Node> for FirehoseFlashblocksExtension
where Node: FullNodeComponents, ...
{
    fn on_node_started(&self, ctx: &FullNodeContext<Node>) -> eyre::Result<()> {
        let provider = ctx.provider().clone();
        let streamer = FirehoseFlashblocksStreamer::new(self.ws_url.clone(), provider);
        streamer.start();
        Ok(())
    }
}
```

(The exact trait and hook API must be determined by reading `bin/node/src/runner.rs` and the
existing `FirehoseExtension` wiring. The implementor should align with that pattern.)

**Step 10 — Cargo.toml workspace integration**

- Add `crates/firehose-flashblocks` to `[workspace]` members.
- Add `base-firehose-flashblocks = { path = "crates/firehose-flashblocks" }` to
  `[workspace.dependencies]`.
- Update `bin/node/Cargo.toml` to add `base-firehose-flashblocks.workspace = true`.
- (No reth-firehose bump needed — already done in Step 0.)

---

### Decisions & Assumptions

| Decision/Assumption | Rationale |
|---|---|
| `base-firehose-flashblocks` as a standalone crate | Clean separation; flashblocks + firehose intersection is non-trivial. |
| Dedicated non-global `firehose_tracer::Tracer` | Prevents live-block tracing from being blocked or corrupted by flashblock execution. |
| `SynchronizedStdout` mutex in `reth-firehose` | Both tracer instances share a single `Arc<Mutex<()>>` to ensure atomic line writes to stdout. Uncontested in normal (no-flashblocks) mode = zero overhead. |
| `--firehose-flashblocks-url` default is empty string | CLI convention; empty = disabled. Matches how the existing `--flashblocks-url` is handled. |
| Fast-path state carry-forward (no provider lookup for sequential blocks) | Critical for performance: avoids waiting for canonical chain state to catch up with in-flight flashblocks. The `State<DB>` carries all prior transactions' effects. |
| Retry loop (up to ~2s) for StateProvider bootstrap on gap/restart | Without some waiting, the first flashblock after a restart would always fail (canonical state may not yet be at the right height). A bounded retry is pragmatic. |
| Carry `accumulated_db` across blocks (not just within a block) | Key insight from the dev feedback: Block N+1's base arrives before canonical reflects Block N. The carried `State<DB>` is the only reliable source of Block N's post-state. |
| `is_final` derived from WS payload | The flashblock WS protocol encodes finality in the payload. The implementor must inspect `Flashblock` struct in `base-flashblocks` to find the right field. |
| Pre-execution changes (EIP-4788, etc.) only on index == 0 | Matches Geth `StateProcessor.Process` `isFirstExecution` gate. These are block-level, not per-delta. |
| Track `streamingfast/reth` `branch = "firehose/1.x"` (not a fixed tag) | The upstream changes landed on `firehose/1.x` after the original plan was written. Branch tracking is acceptable for the duration of this feature work; once stabilised the maintainer will cut a tag (e.g. `v1.11.4-fh-2`) and we will pin to it. |
| `init_tracer` signature now takes `Config` (not a pre-built `Tracer`) | API change on `firehose/1.x`: `init_tracer` builds the global tracer internally using `SynchronizedStdout`. Bin/node already on `firehose/0.x` was calling `init_tracer(Tracer::new(...))`; that call site must be updated to pass `Config` directly. |
| `firehose-tracer` 5.1.0 already has `FlashBlockData` and `new_with_writer` | Confirmed by reading `types.rs` and `tracer.rs` in the registry-vendored source. The flashblock tracer is built via `Tracer::new_with_writer(Config, Box::new(SynchronizedStdout::new(stdout_lock())))`. |
| `is_final` wire encoding (`idx + 1000`) handled inside `firehose-tracer` | The processor only sets the `bool`; the printer adjusts the printed index automatically (`printer::compute_printed_flash_block_index`). No protocol work on our side. |
| Flashblock tracer's `on_blockchain_init` is deferred to the implementor to verify | The global tracer already emits `on_blockchain_init`. Whether the dedicated flashblock tracer should emit a separate init (with a distinct tracer-id) or skip it depends on what the downstream Firehose consumer accepts. Default to skip; revise if integration testing shows the consumer requires per-tracer init. |

---

## State Tracker

**Last Updated:** 2026-05-20 UTC
**Current Step:** Step 1 — Crate scaffolding (`crates/firehose-flashblocks/`)
**Status:** Branch rebased onto `firehose/0.x` so live-tracer prereqs are in place; workspace `cargo check` of default-members passes; ready to scaffold the new crate.

| Step | Status | Notes |
|---|---|---|
| Phase 1 — Contextual Understanding | Done | Explored flashblocks, firehose, engine-tree, runner, bin/node crates |
| Phase 2 — Gap Analysis | Done | Identified: reth-firehose API gaps, state accumulation strategy, CLI wiring |
| Phase 3 — Challenging Dialogue | Done | Dev feedback addressed all open questions |
| Phase 4 — Specification Writing | Done | Full updated spec written above |
| Phase 5 — Spec Review (round 1) | Done | Updated per dev feedback; crate rename, cross-block state, firehose-tracer confirmed, concurrency design, reth-firehose changes section, rebase step added |
| Phase 5 — Spec Review (round 2, 2026-05-20) | Done | Upstream changes landed on `streamingfast/reth` `firehose/1.x` (commits `bb7699f28` + `c9cf230de`). Workspace `Cargo.toml` + `Cargo.lock` bumped from `tag = "v1.11.4-fh-1"` to `branch = "firehose/1.x"`. Removed entire "Required Changes to reth-firehose" section (3 sub-changes were all landed). Removed Step 1 (upstream patch work) — implementation now starts at "create new crate". `FlashblocksTracerHandle::new` rewritten to use `Tracer::new_with_writer(Config, Box::new(SynchronizedStdout::new(stdout_lock())))` matching the actual upstream API. Documented `is_final` wire encoding (`idx + 1000`) handled inside `firehose-tracer 5.1.0`. Added open item: whether the dedicated flashblock tracer should emit its own `on_blockchain_init`. |
| Phase 6 — Rebase + prereqs (2026-05-20) | Done | Rebased branch onto `firehose/0.x` so the live-tracer prereq commits (`3f5b1b124` restoring `firehose::init()` / `FirehoseExtension` + the `OpFirehoseEvmConfig` wrappers) are present. Conflict-resolved `Cargo.toml`: kept `branch = "firehose/1.x"` and the SP1 v6.1.0 `[patch.crates-io]` block (lost on the prior branch base); bumped `firehose-tracer = "5.1.0"`. Removed the duplicate `SignatureFields for BaseTxEnvelope` impl that the rebase produced. Updated `bin/node/src/firehose.rs::init()` for the new `init_tracer(Config)` signature. Workspace `cargo check` of default-members passes. (Pre-existing `base-test-utils` failure due to missing contract artifacts is unrelated and outside default-members.) |
| Step 1 — Crate scaffolding | Current | Next: create `crates/firehose-flashblocks/` directory with Cargo.toml/README/lib.rs/module stubs and wire into workspace `[workspace] members` and `[workspace.dependencies]`. |
