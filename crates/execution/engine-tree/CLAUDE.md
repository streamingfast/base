# Firehose wiring in `base-engine-tree`

Base is an OP-Stack reth fork. This crate hosts the engine-tree payload validator that
executes live (engine-API) blocks. Firehose tracing is ported from
`streamingfast/reth` (`release/reth-1.x`). Two execution paths exist in the node, and
they hit Firehose through different entry points — **both** now install the OP chain
hooks (`OpPostTxExtras` / `OpPreTxAdjust`).

## Two execution paths

1. **Staged-sync / pipeline** — used during initial sync and on restart until the head
   catches up. Wired via `OpExecutorBuilder` in `crates/execution/node/src/node.rs`
   returning `base_execution_firehose::OpFirehoseEvmConfig<OpEvmConfig<..>>`. That
   wrapper's `batch_executor` constructs
   `reth_firehose::FirehoseBlockExecutor::new_with_chain_hooks(.., OpChainHooks)`.
   `OpChainHooks` (in `base-execution-firehose`) implements `reth_firehose::ChainHooks`
   and inlines the per-block wrap: builds the inspector-capable EVM, wraps it in
   `FirehoseWrappedExecutor::with_hooks(.., OpPreTxAdjust, OpPostTxExtras)`, and runs
   `execute_block`.

2. **Live / engine-API** — used once the node is synced and is receiving payloads via
   `engine_newPayload`. Hits `BaseEngineValidator::validate_block_with_state` in
   `validator.rs`. At `validator.rs:501` we check `reth_firehose::is_tracer_initialized()`
   and, for non-genesis blocks, branch into `execute_and_trace_block`
   (`validator.rs:902`). That method installs the `FirehoseInspector` via
   `evm_with_env_and_inspector` and wraps `OpBlockExecutor` in
   `reth_firehose::FirehoseWrappedExecutor::with_hooks(..., OpPreTxAdjust, OpPostTxExtras)`
   directly. `CachedExecutor` is deliberately bypassed on this path — the cache
   short-circuits the EVM on hits, which would leave the inspector blind.

Both paths emit the same OP-specific extras (L1/Base/Operator fee-vault credits and
deposit-tx nonce patching) via the shared hook types in
`base_execution_firehose::{OpPostTxExtras, OpPreTxAdjust}`.

## Why the extra `ChainHooks` indirection?

Before ~2026-04-24, reth's `FirehoseEvmConfig` installed no-op hooks
(`NoPostTxExtras` / `NoPreTxAdjust`) unconditionally in its `batch_executor`, and its
author's note said chains needing chain-specific hooks should provide their own
`ConfigureEvm` wrapper. Attempting that in base surfaces a Rust type-system blocker:
`FirehoseBlockExecutor`'s `Executor<DB>` impl carried a
`for<'a> Extras: PostTxExtras<..Evm<&'a mut State<DB>, FirehoseInspector<'a>>..>` HRTB.
For a *narrow* hook impl like `impl<DB,I,P> PostTxExtras<OpEvm<DB,I,P>>` (what OP
needs) the universally quantified `'a` combined with a method-level `DB` generic
effectively demands `DB: 'static`, which the `ConfigureEvm::batch_executor<DB: Database>`
trait signature can't tighten.

The fix was to refactor `reth_firehose` to move per-block wrapping into a
`ChainHooks<F>` trait. `FirehoseBlockExecutor<F, DB, H: ChainHooks<F>>`'s
`Executor<DB>` impl no longer carries the HRTB — it calls `H::execute_one_traced<DB>`,
whose impl body installs the hooks at a call site where `'a` is a method-local
concrete lifetime (so the `PostTxExtras<OpEvm<&'_ mut State<DB>, ..>>` bound is
checked non-generically and Rust discharges it for any `DB: Database`).

`NoChainHooks` is the mainnet/Ethereum default; `base_execution_firehose::OpChainHooks`
is base's.

## Tracer initialization

`bin/node/src/main.rs:31` calls `firehose::init()` (in `bin/node/src/firehose.rs`),
which invokes `reth_firehose::init_tracer(..)` once process-wide. That triggers the
`[Firehose] tracer initialized (chain_id=...)` line (info-level) from
`firehose_tracer::Tracer::new`.

## Log levels — the usual source of "why don't I see X?"

Firehose logging is a separate knob from reth's `tracing` / `RUST_LOG`. It's controlled
by `FIREHOSE_ETHEREUM_TRACER_LOG_LEVEL` (see
`/Users/stepd/repos/evm-firehose-tracer-rs/firehose-tracer/src/logging.rs`):

| env value     | `firehose_info!` | `firehose_debug!` | `firehose_trace!` | `firehose_trace_full!` |
| ------------- | :--------------: | :---------------: | :---------------: | :--------------------: |
| *unset*       | no               | no                | no                | no                     |
| `info`        | yes              | no                | no                | no                     |
| `debug`       | yes              | yes               | no                | no                     |
| `trace`       | yes              | yes               | yes               | no                     |
| `trace_full`  | yes              | yes               | yes               | yes                    |

If you see the `tracer initialized (chain_id=...)` line but not `OpPostTxExtras::emit_post_tx_extras called`, you're almost certainly at `info`. The extras hook logs at
debug.

## Debug checkpoints on the live path

With `FIREHOSE_ETHEREUM_TRACER_LOG_LEVEL=debug`, expect per block:

- `validator: firehose tracer initialized (block=N, is_genesis=…)` — validator.rs:507.
  Confirms the live payload reached `validate_block_with_state` and found the tracer.
- `Executing block (with Firehose tracing)` — validator.rs:927 at `tracing::debug`
  target `engine::tree::payload_validator` (this one is controlled by `RUST_LOG`, not
  by `FIREHOSE_ETHEREUM_TRACER_LOG_LEVEL`).
- Per-tx: `OpPostTxExtras::emit_post_tx_extras called (gas_used=…, base_fee=…)` and
  one of `emitting vault=…` / `skipping zero-amount credit` / `skipping deposit tx`
  (in `base_execution_firehose::OpPostTxExtras`).

If `validator: firehose tracer initialized` prints but `emit_post_tx_extras called`
does not, the block has no non-deposit txs or no txs at all. If neither prints at
debug, the live payload isn't reaching `validate_block_with_state` — likely a wiring
regression.

## Debug checkpoints on the pipeline path

Pipeline (staged-sync) replays blocks via `ExecutionStage::execute` →
`FirehoseBlockExecutor::execute_and_trace_one` → `OpChainHooks::execute_one_traced`.
The `emit_post_tx_extras called` line fires per-tx the same way.

## Genesis quirk

Block 1 is the genesis marker. `FirehoseBlockTracer::start` emits `on_genesis_block` as
a standalone event and does NOT leave the tracer in "block state", so wrapping the
executor would panic in `on_system_call_start`. The `(!is_genesis).then_some(tracer)`
at `validator.rs:512` drops the guard for genesis and the code falls through to the
non-traced `execute_block`. On the pipeline path, `FirehoseBlockExecutor::execute_and_trace_one`
handles the same genesis skip internally. This is expected and only affects block 1.

## Sibling repos assumed at `/Users/stepd/repos/`

- `reth` — branch `release/reth-1.x`, houses `reth-firehose` (`crates/firehose/...`).
  The `ChainHooks` refactor is commit `49b3af7d8`. Base's `Cargo.toml` pulls from
  `{ git = "https://github.com/streamingfast/reth.git", branch = "release/reth-1.x" }`
  (no local-path fallback).
- `evm-firehose-tracer-rs` — `firehose-tracer` crate (the `Tracer`, logging macros,
  protobuf types).
