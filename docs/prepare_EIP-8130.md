# Preparing Firehose tracing for EIP-8130 (Everest)

EIP-8130 ("Account Abstraction by Account Configuration") adds transaction type `0x79` (121). It
has its own wire format, phased execution, a payer separate from the sender, 2D nonces, and logs
the protocol writes outside any call frame. Base gates it behind the **Everest** upgrade.

Firehose can't trace it. The `reth-firehose` mapper (`crates/firehose/src/mapper.rs` in
`streamingfast/reth`) stops the node on the first `0x79` transaction with an error naming the
transaction and its type. Stopping is deliberate: skipping the transaction would emit a block that
differs from the chain. This document describes what the transaction does under a tracer, the
block model changes agreed in review, and the work in each repository.

## Activation

- Everest is contract-backed: an L1 upgrade signal (the `ProtocolVersions` contract, read by
  `crates/utilities/upgrade-signal`) can schedule it with no new node release. A timestamp in
  `crates/common/chains/src/config.rs` is not a reliable warning. Watch the signal itself.
- No network has scheduled Everest. [ACTIVATIONS.md](../ACTIVATIONS.md) tracks the status.
- Everest requires every earlier Base upgrade, Denim included, so only Base's own stack can run
  it. The local Docker stack (`etc/systems`) and the in-process action harness
  (`actions/harness/tests/everest`) both activate it.
- Zenith gates the keystore part of 8130: account configuration changes, account creation, and
  non-k1 authenticators. Zenith can only be set in genesis, so on real networks an Everest
  transaction can only carry k1 signatures and `Delegation` account changes.

## How a `0x79` transaction runs under a tracer

`BaseEvm::transact_raw` sends 8130 transactions to `Eip8130Executor`
(`crates/common/evm/src/eip8130.rs`) instead of revm's mainnet handler. In order:

1. `authorize_and_apply` validates the sender and payer, then writes straight to the journal, with
   no call frame and no inspector hook:
   - delegation code (`0xef0100 ‖ target`, or empty to clear);
   - for a non-zero `nonce_key`, the 2D nonce `sstore` on the nonce manager
     (`0x813000000000000000000000000000000000aa01`), a `0xEF` code stub rewrite on that account,
     and a `NonceIncremented` log;
   - for `nonce_key == U256::MAX` (nonce-free), the replay ring `sstore`s;
   - transient `TxContext` writes on `0x8130…aa02`.
2. `prepay` bumps the sender's account nonce when `nonce_key == 0` and debits the payer for
   `(gas_limit + payer_auth_gas) × effective_price` plus the L1 and operator fees, again straight
   to the journal.
3. `start_inspection` opens one synthetic depth-0 `CALL` from the sender to the sender, with the
   EIP-2718-encoded transaction as input. It runs even when `calls` is empty.
4. Each phase runs under a journal checkpoint. Each call in a phase is a real revm frame at journal
   depth 1 under the synthetic root, so the inspector sees it and everything inside it. A call
   blocked by the actor policy, or one that can't pay for a new account, gets no frame.
5. A failed phase is rolled back with `checkpoint_revert` after its calls already reported success
   through `call_end`. No hook marks the rollback. Later phases are skipped.
6. `settle_fees` refunds the payer and credits the base fee vault, the block beneficiary, the L1
   fee vault, and the operator fee vault, straight to the journal.
7. `end_inspection` closes the synthetic root as `Revert` if any phase failed.

The receipt status is success only if every phase committed. State from phases that committed
before the failing one stays, along with the nonce, the delegation code, and the fees.

## What the tracing stack gets wrong today

The `0x79` stop hides these, and each needs a fix:

- **Partial success.** The tracer turns a failed receipt into a failed root call, and
  `populate_state_reverted` then reverts every child, including committed phases, the nonce, and
  the fees.
- **Writes before the root call are lost.** The reth inspector skips the journal up to the depth-0
  call, so the pre-call writes in steps 1 and 2 never reach the block.
- **The payer is invisible.** The reth inspector only emits `GasBuy` and `GasRefund` for the
  sender.
- **Double counting.** `OpPostTxExtras` rebuilds the vault credits with the standard OP Stack
  formula, while the same credits also arrive from the journal as `Transfer`.
- **Protocol logs.** `NonceIncremented` is written before the root call. `gather_precompile_logs`
  attaches it to whichever call ends first, or drops it.
- **Fields.** The generic mapper emits `to = 0x0`, empty `input`, `value = 0`, and
  `nonce = nonce_sequence`. It drops `nonce_key`, `payer`, `calls`, `account_changes`, and
  `metadata`.
- **Silent fallback.** In `evm-firehose-tracer-rs`, the transaction type mapping in `tracer.rs`
  sends unknown types to `TRX_TYPE_LEGACY`. The reth mapper stops first, but the tracer should fail
  on its own.

## Block model (`sf/ethereum/type/v2/type.proto`)

Proposed, following review:

- **Enum value.** `TRX_TYPE_EIP8130 = 121`, with no `BASE_` prefix. The wire format comes from the
  EIP, not from Base, so other chains that adopt it share the value. If L1 ever picks a different type byte,
  add a second value then.
- **Reuse existing fields** where the meaning is the same: `hash`, `from` (the resolved sender),
  `nonce` (`nonce_sequence`), `gas_limit`, `max_fee_per_gas`, `max_priority_fee_per_gas`,
  `gas_price` (effective), `gas_used`, `status`, `receipt`, and `calls`. `to`, `value`, `input`,
  `v`/`r`/`s`, `access_list`, and the blob fields stay empty.
- **Add new fields flat on `TransactionTrace`**, with no chain prefix: `nonce_key` (`BigInt`),
  `payer`, `valid_after`, `valid_before`, `metadata`, `account_changes` (a new message, like
  `set_code_authorizations` for EIP-7702), and the phase structure.
- **`phase_statuses` on `TransactionReceipt`**, one byte per phase (`0x01` committed, `0x00`
  failed or skipped), as RPC returns `phaseStatuses`. The receipts root doesn't commit to it, but
  it is persisted with the receipt. With the phases listed on the transaction, it places every
  call frame in its phase: a committed phase ran all its calls, each as one depth-1 frame; at most
  one phase fails and nothing runs after it, so the remaining depth-1 frames belong to the failed
  phase. A failed phase can have no frames at all (blocked by the actor policy or the new-account
  cost), which only `phase_statuses` tells apart from a skipped phase. Calls carry no phase index,
  as in RPC traces.
- **Status.** A transaction with a failed phase has status `FAILED`, matching the receipt. The root
  call isn't reverted, and only the failed phase's calls are `state_reverted`. The proto docs must
  say that, for this type, a failed transaction can carry durable state changes and logs, so
  readers go by each call's `state_reverted` instead of the transaction status.
- **Gas.** One `REASON_GAS_BUY` and at most one `REASON_GAS_REFUND` per transaction, as for other
  types, but on the payer. The payer's authentication gas is part of the same prepay.

Open question: the review preferred one depth-0 call per phase. The executor already produces a
single synthetic root with the phase calls beneath it, which `debug_traceTransaction` also shows,
and which keeps the rule that call 0 is the transaction's root. The proposal is to keep that shape.

## Work in each repository, in order

1. **`firehose-ethereum` proto.** The enum value, the new transaction fields,
   `TransactionReceipt.phase_statuses`, and the status documentation.
2. **`evm-firehose-tracer-rs`** (5.6.0).
   - Add `TxType::Eip8130 = 121`, its `Display` entry, and its proto mapping. Make unknown types an
     error instead of `TRX_TYPE_LEGACY`.
   - Carry the 8130 fields, through `TxEvent` or a `transaction_mut()` accessor like `block_mut()`.
   - Let the caller mark a phase's calls `state_reverted` before `complete_transaction`, and skip
     the "failed receipt fails the root call" step for type 121.
   - Move logs written after the root call exits into the root call; the deferred state's "root"
     branch drops them.
   - Update the test invariants, which check only the first depth-0 call.
3. **`streamingfast/reth`, `crates/firehose`** (new `base-v…-fhX` tag).
   - Let a chain read the journal entries written before the depth-0 call instead of skipping
     them. This covers B-20 and 8130 protocol writes and logs without a hook upstream.
   - Let a chain choose the reason of balance changes read from the journal, instead of
     `Transfer`.
4. **Base** (`crates/execution/firehose`).
   - Have `Eip8130Executor` record what it did: payer, prepay, refund, each vault credit, how many
     calls each phase ran, and which phase failed. Use the same thread-local as
     `Eip8130PhaseStatuses`.
   - Use that record in the Firehose extras: `GasBuy` and `GasRefund` on the payer, fee reasons on
     the vault credits, nonce manager writes and `NonceIncremented` on the root call, and
     `state_reverted` on the failed phase's calls.
   - Skip 8130 in `OpPostTxExtras`. Fill the 8130 fields in a Base `PreTxAdjust`.
   - Ignore the nonce manager's `0xEF` code rewrite when the code doesn't change, and the transient
     `TxContext` writes.
   - Keystore writes (Zenith only) keep stopping the node with a clear error.

## Tests, in order

1. Code-only capture tests first, for a fast loop with no Docker: `run_prestate` goldens in
   `base-firehose-tests`, and `BaseFirehoseCapture` on top of the in-process Everest action tests
   if that harness goes through `FirehoseWrappedExecutor`. Cases:
   - paying for yourself;
   - a named payer;
   - a 2D nonce and a nonce-free transaction;
   - a delegation;
   - two phases where the second fails;
   - a call blocked by the actor policy.
2. A `firehose_everest.rs` system test in `etc/systems/tests`, combining the transaction builders
   of `eip8130.rs` with the activation-block pattern of `firehose_denim.rs`.
3. Battlefield (`op-reth-devnet`) can't run Everest, since its stack uses upstream `op-node` and
   `op-geth`. Run it on the new reth tag as a regression check for every other transaction type:
   the tracer change to `complete_transaction` runs on all of them, so its goldens must not change.
