# Preparing Firehose tracing for the AA transaction type (EIP-8130, Everest)

Transaction type `0x79` (121) is an account abstraction transaction with phased execution, a payer
separate from the sender, 2D nonces, and in-band EIP-7702-style delegation. Base gates it behind
the **Everest** upgrade. The Base code calls it EIP-8130 (`TxEip8130`, `Eip8130Executor`), but the
specification has been split:

- [EIP-8130](https://eips.ethereum.org/EIPS/eip-8130) covers only Keystore accounts (actors,
  authenticators, policies) and how they plug into the transaction type
  ([ethereum/EIPs#12386](https://github.com/ethereum/EIPs/pull/12386), merged).
- The transaction type moves to a new draft, "AA Transaction Type", numbered EIP-8430 in
  [ethereum/EIPs#12385](https://github.com/ethereum/EIPs/pull/12385), which is not merged. Its
  field list matches `TxEip8130` in `v1.5.0`. This document follows that draft.

Firehose can't trace the type. The `reth-firehose` mapper (`crates/firehose/src/mapper.rs` in
`streamingfast/reth`) stops the node on the first `0x79` transaction with an error naming the
transaction and its type. Stopping is deliberate: skipping the transaction would emit a block that
differs from the chain. This document describes what the transaction does under a tracer, the
proposed block model changes, and the work in each repository.

## Activation

- Everest is contract-backed: an L1 upgrade signal (the `ProtocolVersions` contract, read by
  `crates/utilities/upgrade-signal`) can schedule it with no new node release. A timestamp in
  `crates/common/chains/src/config.rs` is not a reliable warning. Watch the signal itself.
- No network has scheduled Everest. [ACTIVATIONS.md](../ACTIVATIONS.md) tracks the status.
- Everest requires every earlier Base upgrade, Denim included, so only Base's own stack can run
  it. The local Docker stack (`etc/systems`) and the in-process action harness
  (`actions/harness/tests/everest`) both activate it.
- Zenith gates the Keystore integration: account configuration changes, account creation, and
  non-k1 authenticators. Zenith can only be set in genesis, so on real networks an Everest
  transaction can only carry k1 signatures and `Delegation` account changes.

## Changes after `v1.5.x`

Upstream `main` has changes to this type that the `v1.5.x` releases (up to `v1.5.1-rc.6`) don't
have. Check them on each upstream bump:

- **Receipt.** [base/base#5952](https://github.com/base/base/pull/5952) adds `committed_phases` to
  the consensus receipt, as the EIP-8430 draft specifies:
  `0x79 || rlp([status, cumulative_gas_used, logs_bloom, logs, committed_phases])`. In `v1.5.x`,
  the receipt carries `phase_statuses` (one byte per phase) outside the RLP encoding instead.
- **Nonce log.** `main` no longer emits `NonceIncremented`, and the draft defines no such log.

## How a `0x79` transaction runs under a tracer

`BaseEvm::transact_raw` sends these transactions to `Eip8130Executor`
(`crates/common/evm/src/eip8130.rs`) instead of revm's mainnet handler. In `v1.5.x`, in order:

1. `authorize_and_apply` validates the sender and payer, then writes straight to the journal, with
   no call frame and no inspector hook:
   - delegation code (`0xef0100 ‖ target`, or empty to clear);
   - for a `nonce_key` other than `0` and `U256::MAX`, the 2D nonce `sstore` on the nonce manager
     (`0x813000000000000000000000000000000000aa01`), a `0xEF` code stub rewrite on that account,
     and, in `v1.5.x` only, a `NonceIncremented` log;
   - for `nonce_key == U256::MAX` (nonce-free), the replay ring `sstore`s;
   - transient `TxContext` writes on `0x8130…aa02`. The draft defines the transaction context as
     in-memory client state, not EVM state.
2. `prepay` bumps the sender's account nonce when `nonce_key == 0` and debits the payer for
   `(gas_limit + payer_auth_gas) × effective_price` plus the L1 and operator fees, again straight
   to the journal. The draft orders these steps differently (payer charge, nonce, then account
   changes); the final state is the same, but the order of changes in the block follows the node.
3. `start_inspection` opens one synthetic depth-0 `CALL` from the sender to the sender, with the
   EIP-2718-encoded transaction as input. It runs even when `calls` is empty. This root is a Base
   inspector construct: the draft makes each call "a separate top-level call frame".
4. Each phase runs under a journal checkpoint. Each call in a phase is a real revm frame at journal
   depth 1 under the synthetic root, so the inspector sees it and everything inside it. A call
   blocked by the actor policy, or one that can't pay for a new account, gets no frame.
5. A failed phase is rolled back with `checkpoint_revert` after its calls already reported success
   through `call_end`. No hook marks the rollback. Later phases are skipped.
6. `settle_fees` refunds the payer and credits the base fee vault, the block beneficiary, the L1
   fee vault, and the operator fee vault, straight to the journal. The draft's L1 profile burns the
   base fee; Base credits it to the vault.
7. `end_inspection` closes the synthetic root as `Revert` if any phase failed.

The receipt status is success only if every phase committed. State from phases that committed
before the failing one stays, along with the nonce, the delegation code, and the fees. The draft
states this too: `status == 0` does not imply no state change or no logs.

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
- **Protocol logs (`v1.5.x`).** `NonceIncremented` is written before the root call.
  `gather_precompile_logs` attaches it to whichever call ends first, or drops it.
- **Fields.** The generic mapper emits `to = 0x0`, empty `input`, `value = 0`, and
  `nonce = nonce_sequence`. It drops `nonce_key`, `payer`, `calls`, `account_changes`, and
  `metadata`.
- **Silent fallback.** In `evm-firehose-tracer-rs`, the transaction type mapping in `tracer.rs`
  sends unknown types to `TRX_TYPE_LEGACY`. The reth mapper stops first, but the tracer should fail
  on its own.

## Block model (`sf/ethereum/type/v2/type.proto`)

Proposed, following review:

- **Enum value.** `121`, matching the wire byte like `TRX_TYPE_OPTIMISM_DEPOSIT = 126`, with no
  `BASE_` prefix. The type now belongs to the AA Transaction Type draft, not EIP-8130, and that
  draft's number isn't merged, so a name that doesn't carry an EIP number avoids both:
  `TRX_TYPE_AA = 121`.
- **Reuse existing fields** where the meaning is the same, as the draft's RPC object does: `hash`,
  `from` (the resolved sender), `nonce` (`nonce_sequence`), `gas_limit`, `max_fee_per_gas`,
  `max_priority_fee_per_gas`, `gas_price` (effective), `gas_used`, `status`, `receipt`, and
  `calls`. `to`, `value`, `input`, `v`/`r`/`s`, `access_list`, and the blob fields stay empty.
- **Add new fields flat on `TransactionTrace`**, with no chain prefix: `nonce_key` (`BigInt`),
  `payer` (resolved), `valid_after`, `valid_before`, `metadata`, `account_changes` (a new message,
  like `set_code_authorizations` for EIP-7702), and the phases with their calls (`to`, `value`,
  `data`).
- **`committed_phases` on `TransactionReceipt`** (`uint64`), the consensus receipt field that RPC
  returns as `committedPhases`. With the phases listed on the transaction, it places every call
  frame in its phase: a committed phase ran all its calls, each as one depth-1 frame; phase number
  `committed_phases` failed, and the remaining depth-1 frames belong to it; later phases have no
  frames. A failed phase can have no frames at all (blocked by the actor policy or the new-account
  cost). Calls carry no phase index, as in RPC traces. For blocks from `v1.5.x`, the tracer derives
  the count from `phase_statuses`: the number of leading `0x01` bytes.
- **Root call.** One depth-0 call from the sender to the sender, with the encoded transaction as
  input, and the phase calls beneath it. This keeps the rule that call 0 is the transaction's root
  and matches what Base's inspector and `debug_traceTransaction` produce. It is a Firehose choice:
  the draft defines no root call, and another client wouldn't necessarily produce one.
- **Status.** A transaction with a failed phase has status `FAILED`, matching the receipt. The root
  call isn't reverted, and only the failed phase's calls are `state_reverted`. The proto docs must
  say that, for this type, a failed transaction can carry durable state changes and logs, so
  readers go by each call's `state_reverted` instead of the transaction status.
- **Gas.** One `REASON_GAS_BUY` and at most one `REASON_GAS_REFUND` per transaction, as for other
  types, but on the payer. The payer's authentication gas is part of the same prepay.

## Work in each repository, in order

1. **`firehose-ethereum` proto.** The enum value, the new transaction fields,
   `TransactionReceipt.committed_phases`, and the status documentation.
2. **`evm-firehose-tracer-rs`** (5.6.0).
   - Add `TxType::Aa = 121`, its `Display` entry, and its proto mapping. Make unknown types an
     error instead of `TRX_TYPE_LEGACY`.
   - Carry the new fields, through `TxEvent` or a `transaction_mut()` accessor like `block_mut()`.
   - Let the caller mark a phase's calls `state_reverted` before `complete_transaction`, and skip
     the "failed receipt fails the root call" step for type 121.
   - Move logs written after the root call exits into the root call; the deferred state's "root"
     branch drops them.
   - Update the test invariants, which check only the first depth-0 call.
3. **`streamingfast/reth`, `crates/firehose`** (new `base-v…-fhX` tag).
   - Let a chain read the journal entries written before the depth-0 call instead of skipping
     them. This covers B-20 and `0x79` protocol writes and logs without a hook upstream.
   - Let a chain choose the reason of balance changes read from the journal, instead of
     `Transfer`.
4. **Base** (`crates/execution/firehose`).
   - Have `Eip8130Executor` record what it did: payer, prepay, refund, each vault credit, how many
     calls each phase ran, and which phase failed. Use the same thread-local that hands phase
     outcomes to the receipt builder (`Eip8130PhaseStatuses` in `v1.5.x`).
   - Use that record in the Firehose extras: `GasBuy` and `GasRefund` on the payer, fee reasons on
     the vault credits, nonce manager writes (and `NonceIncremented` in `v1.5.x`) on the root call,
     and `state_reverted` on the failed phase's calls.
   - Skip type 121 in `OpPostTxExtras`. Fill the new fields in a Base `PreTxAdjust`.
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
