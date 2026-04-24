# base-execution-firehose

OP Stack chain hooks for Firehose tracing, plus a `ConfigureEvm` wrapper that wires
those hooks into the staged-sync (pipeline) path.

## Why this crate exists

`reth_firehose` ships two no-op hook types (`NoPostTxExtras`, `NoPreTxAdjust`) and a
`FirehoseEvmConfig<F>` wrapper that installs them unconditionally. Chains with their
own fee distribution (OP Stack credits the L1 / base-fee / operator fee vaults via
`Journal::balance_incr`, which fires no inspector hooks) must ship their own
`ConfigureEvm` wrapper. See the author's note in
`reth/crates/firehose/src/executor.rs:865–878` for the type-system reason upstream
can't parameterize `FirehoseEvmConfig` over `Extras`/`Adjust`.

This crate is that wrapper for Base / OP Stack.

## What's in it

- [`OpPostTxExtras`] — post-tx emitter for the three OP fee-vault balance changes
  (L1FeeVault / BaseFeeVault / OperatorFeeVault) with reason `RewardTransactionFee`.
- [`OpPreTxAdjust`] — pre-tx patcher that overrides the `TxEvent` nonce for OP deposit
  envelopes (they carry no nonce field; the effective nonce is the sender's pre-exec
  account nonce).
- [`OpFirehoseEvmConfig`] — `ConfigureEvm` wrapper whose `batch_executor` constructs
  `FirehoseBlockExecutor::new_with_hooks(..., OpPreTxAdjust, OpPostTxExtras)` so the
  pipeline fires the same OP hooks the live engine-API path already uses.

## Paths into the hooks

| Path                          | Where hooks are installed                                 |
| ----------------------------- | --------------------------------------------------------- |
| Staged sync (pipeline)        | `OpFirehoseEvmConfig::batch_executor` in this crate       |
| Engine API (live)             | `execute_and_trace_block` in `base-engine-tree/validator.rs` — explicit `FirehoseWrappedExecutor::with_hooks(..., OpPreTxAdjust, OpPostTxExtras)` |
