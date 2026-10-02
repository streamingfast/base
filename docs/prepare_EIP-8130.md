# Preparing Firehose tracing for EIP-8130 (Cobalt)

EIP-8130 ("Account Abstraction by Account Configuration") introduces a new
transaction type (`0x79`) with its own wire format, phased execution, sponsored
gas, 2D nonces, and protocol-injected logs. It is fully implemented in this
codebase (since upstream v1.2.0), gated behind the **Cobalt** hardfork, which
Base targets for **mainnet in September 2026**. Firehose tracing does not
support it yet: a 0x79 transaction is currently recorded as a legacy
transaction with wrong fields, and the injected Keystore logs would likely trip
the call/receipt log-count panic. This document captures the analysis
(2026-08-28) and the preparation plan.

## Launch timeline

- **Now**: live on "vibenet", Base's ephemeral devnet (<https://vibes.base.org>),
  per the [Base engineering blog](https://blog.base.dev/native-account-abstraction).
- **September 2026**: mainnet, via the Cobalt upgrade
  ([announcement coverage](https://www.bitget.com/amp/news/detail/12560605520641)).
  Beryl (reth v2, B20) already activated June 25, 2026.
- In code, `cobalt_timestamp` is `None` for mainnet, sepolia, devnet and
  zeronet in `crates/common/chains/src/config.rs` — **that field being set for
  sepolia is the tripwire**. The only place Cobalt activates today is the local
  Docker devnet (`L2_BASE_COBALT_BLOCK=22` in `etc/docker/devnet-env`).
- The [EIP itself](https://eips.ethereum.org/EIPS/eip-8130) is still **Draft**
  and the wire format is still churning — see "Moving target" below.

## Status in this codebase

Not scaffolding — ~1500 references across 76 files:

- **Type `0x79`** (`EIP8130_TX_TYPE_ID = 121`), payer signature domain `0x7A`.
  Fields: `chain_id`, optional `sender`, `nonce_key: U256` +
  `nonce_sequence: u64`, validity window, 1559 fees, `gas_limit`,
  `account_changes`, `calls: Vec<Vec<Call>>` (phases), `metadata`, optional
  `payer`, plus opaque `sender_auth`/`payer_auth` blobs (no v/r/s).
- **Execution** (`crates/execution/eip8130/`,
  `crates/common/evm/src/eip8130.rs`): `Eip8130Executor` is invoked from
  `BaseEvm::transact_raw`, bypassing the mainnet single-frame handler.
  Authorization, account changes, nonce validation, and payer precharge run
  **directly against the journal with no EVM call frame**; only the `calls`
  phases run as real EVM frames.
- **System contracts**: Keystore (named `AccountConfiguration` in this
  checkout) at `0x2403408177dB7F8512a9593343a7C80371D8f2dF`, `NonceManager`
  precompile at `0x813000000000000000000000000000000000aa01`, `TxContext`
  precompile at `0x813000000000000000000000000000000000aa02`. These addresses
  are documented upstream as **provisional Base Sepolia values**.
- **Receipt**: `Eip8130Receipt` adds `phaseStatuses` — excluded from RLP so
  the receipts-trie root is unchanged; it is RPC-only, handed from executor to
  receipt builder via a thread-local (`eip8130_phase_statuses.rs`).
- Mempool (2D-nonce pool), span-batch derivation, and RPC (`eth_estimateGas`,
  channel nonces) are all done.

### Moving target

Upstream `main` has work that is *not* in this checkout: the
`valid_after`/`valid_before` millisecond validity window (base/base#4330 —
this checkout still has a single `expiry` field, i.e. the RLP layout changed),
the `SignedAccountChanges` redesign (base/base#4327), and the `Keystore.sol`
rename. The wire format is not frozen; do not freeze goldens against it yet.

## Impact on the Firehose block model (`sf/ethereum/type/v2/type.proto`)

1. **New `Type` enum value.** Nothing maps 0x79. Suggest
   `TRX_TYPE_BASE_EIP8130 = 121`, following the convention of matching the
   wire byte (`TRX_TYPE_OPTIMISM_DEPOSIT = 126`).
2. **Transaction fields that don't fit the current message.**
   `TransactionTrace.nonce` is `uint64`, but 8130 has a 256-bit `nonce_key`
   plus `nonce_sequence`. There is no `to` (a phase/call list instead), no
   `value`, no v/r/s (opaque auth blobs), and new concepts: `payer`, validity
   window, `metadata`, `account_changes` (create / config change / delegation
   entries — structurally similar to how `set_code_authorizations` was added
   for EIP-7702). This needs a new set of fields or a sub-message, populated
   only for this type, like the blob and set-code precedents.
3. **Receipt.** `phaseStatuses` is not part of the consensus receipt, but the
   tracer can and should carry it: a consumer cannot otherwise tell which
   phases committed.
4. **Call tree shape.** The model and its documented consumer contract assume
   **one root call at index 0 aligned with the transaction**. An 8130
   transaction produces *multiple top-level frames* (one per call, grouped
   into phases), and a transaction can be `SUCCEEDED` while individual phases
   reverted (cross-phase durability). `state_reverted` per call can express
   the reverted phases, but consumers' "root call #0" processing rules break;
   calls likely need a phase index, and the status semantics need explicit
   documentation.
5. **Logs.** The `ActorAuthorized` / `ActorRevoked` / `AccountCreated` /
   `DelegationApplied` logs are pushed straight into the revm journal
   (`emit_event` → `internals.log(...)`) outside any call frame, and appear in
   the receipt **ahead of** the calls' logs. No LOG opcode executes, so
   inspector log hooks never fire. This is the same mechanism as the B20
   precompile logs already handled by the tracer — good precedent — but it is
   precisely the shape that caused the fh-5 panic
   ("N call logs but N+1 receipt logs").
6. **State changes outside frames.** Payer precharge/settlement means
   `REASON_GAS_BUY` / `REASON_GAS_REFUND` balance changes land on the
   **payer**, not `from`. 2D nonces are `StorageChange`s on the NonceManager
   precompile, not `NonceChange`s. Auto-delegation writes `0xef0100‖target`
   code, and account creation sets code directly at a CREATE2 address —
   `CodeChange`s from the pre-call pipeline, not from a CREATE frame.

## What breaks today

The tracer maps unknown transaction types to `TrxTypeLegacy` with a silent
fallthrough (`firehose-tracer` `tracer.rs`, `tx_type` mapping) — past Cobalt
it would produce **wrong blocks without any error**, plus a probable log-count
mismatch panic from the injected logs (`FIREHOSE_TRACER_IGNORE_LOG_MISMATCH`
would "save" the node by dropping data). Tracing the local devnet past block
22 exercises the broken path right now.

## Preparation plan, in order

1. **Make the tracer fail loud, now.** Replace the silent legacy fallthrough
   for unknown tx types with a hard error (or at least an error log plus an
   explicit unknown marker). Cheap, and it converts "corrupt data" into
   "visible outage" if Cobalt ever front-runs us.
2. **Design the proto extension now.** The model additions (enum value, 8130
   sub-message, phase attribution on `Call`, `phaseStatuses`) can be drafted
   and reviewed in `firehose-ethereum` independent of the wire-format churn —
   the churn affects field *contents*, not the shape of the model. Needs
   coordination since `type.v2` is shared across all EVM chains.
3. **Implement tracing against the local devnet.** Block 22+ on the Docker
   devnet is a working reproduction today; vibenet gives a second, realistic
   source of 8130 traffic. Reuse the B20 injected-log handling for the
   Keystore logs; the new work is multi-root-frame call tracking in the
   tracer callstack and the journal-only pre-call pipeline (payer precharge,
   account changes, nonce writes).
4. **Add a `firehose_8130` system test** alongside `firehose_b20` in
   `base-firehose-tests`, with JSON-projection goldens (not full-protobuf,
   per the `nop_transfer` lesson).
5. **Track two upstream signals**: `cobalt_timestamp` appearing in
   `config.rs` (sepolia first), and the wire-format PRs (base/base#4330 etc.)
   landing in a tagged release — hold golden regeneration until the format
   settles and final contract addresses replace the provisional ones.
