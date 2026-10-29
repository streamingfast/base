# Network activations and the Firehose block

One row per Base upgrade defined in the code this fork ships, with its status on the networks we
serve and its effect on the Firehose block. Review new and changed entries on every upstream bump,
and refresh the statuses when an activation is announced.

Denim and Everest are contract-backed: an L1 upgrade signal (`crates/utilities/upgrade-signal`) can
schedule them on a network with no new node release. Zenith can only be set in genesis.

Statuses checked 2026-10-28 against Base `v1.5.1` (`crates/common/chains/src/config.rs`).

| Upgrade | Base mainnet | Base Sepolia | Effect on the Firehose block |
|---------|--------------|--------------|------------------------------|
| Bedrock … Jovian | Active (Jovian 2025-12-02) | Active | Already in the stream. Canyon's `create2deployer` install is a direct DB write in `apply_pre_execution_changes`; it is emitted as a block-level code change when the block is traced with `v1.5.0-fh3.1` or later. |
| Azul | Active 2026-05-28 18:00 UTC | Active 2026-04-20 18:00 UTC | Already in the stream. |
| Beryl | Active 2026-06-25 18:00 UTC | Active 2026-06-18 18:00 UTC | Already in the stream. |
| Cobalt | Active 2026-09-30 18:00 UTC | Active 2026-09-23 18:00 UTC | B-20 V2 precompiles. Covered by `etc/systems/tests/firehose_cobalt_b20.rs`. |
| Denim | Scheduled 2026-11-05 18:00 UTC (`1_793_901_600`) from `v1.5.1`; first Denim block is 53_556_127 at `1_793_901_601` | Active 2026-10-21 18:00 UTC (`1_792_605_600`) | See [Denim](#denim). **Mainnet nodes must run `v1.5.1-fh3.1` or later before activation.** |
| Everest | Not scheduled | Not scheduled | See [Everest](#everest). Traced from `v1.5.0-fh3.1-2` as `TRX_TYPE_ACCOUNT_ABSTRACTION`. **Earlier versions stop on the first EIP-8130 transaction.** |
| Zenith | Never (genesis only) | Never (genesis only) | Gates the EIP-8130 keystore (account configuration changes, non-k1 authenticators). Unreachable on real networks. Traced from `v1.5.0-fh3.1-2`: create and config change entries in `account_changes`, non-k1 sender authentication in `sender_auth`. |

## Denim

- **Activation block installs the BaseTime predeploy outside the EVM.** `BaseTime::ensure_predeploy`
  (`crates/common/evm/src/base_time.rs`) runs in `apply_pre_execution_changes` and writes with
  `db.commit`, which no tracer hook sees. `BasePreExecutionWrites` lists both locations, so the
  block carries a block-level code change for the implementation account
  `0xc0D3C0d3C0d3C0D3c0d3C0d3c0D3C0d3c0d30030` and a system call to the proxy
  `0x4200000000000000000000000000000000000030` holding its EIP-1967 implementation slot write.
  Covered by `etc/systems/tests/firehose_denim.rs`.
- **New deposit at tx index 1 in every block**: `DEPOSITOR_ACCOUNT` calls BaseTime
  `setTimestampMillisPart(uint16)`. Traced like the L1 info deposit; the depositor nonce goes up by
  2 per block. No code change needed.
- **Up to 5 blocks per second share the header `timestamp`.** A block's timestamp is never lower
  than its parent's but is often equal. The milliseconds are only in the tx[1] calldata and BaseTime
  storage slot 0; the Firehose header keeps `nanos: 0`.
- First Denim block divides the gas limit by 10 and multiplies the EIP-1559 denominator by 10:
  values only.
- B-20 precompiles switch to V3 (new policy rules). Same storage and event paths as V2.
- New post-execution checks (base time schedule) run before the Firehose block is printed on the
  live and pipeline paths.
- The FIRE BLOCK line time, which becomes the Firehose block time, has millisecond precision from
  the activation block on, computed from the chain's Denim schedule (`bin/node/src/firehose.rs`).
  RPC adds `timestampMs` and `blockTimestampMs` to blocks, transactions, receipts and logs; they
  carry the same value, which the Firehose block has only as its block time.
- Flashblocks stop. From the activation timestamp the sequencer builds with the basic builder and
  publishes no flashblocks (`crates/builder/multiplex/src/router.rs`), so a node started with
  `--firehose-flashblocks-url` emits no partial blocks after the last pre-Denim block. Full blocks
  are unaffected and come every 200 ms.

## Everest

- Gates EIP-8130 transactions (type `0x79` = 121) in the EVM, the transaction pool, RPC and
  derivation. Before v1.5.0 this was Zenith, which no real network can activate.
- From `v1.5.0-fh3.1-2` (`firehose-tracer` 5.7.0, `streamingfast/reth` `base-v2.5.2.8-fh3.1-2`),
  type 121 is traced as `TRX_TYPE_ACCOUNT_ABSTRACTION`:
  - `TransactionTrace.account_abstraction_trx_input` holds the nonce key, the payer, the validity
    window, the metadata, the account changes, the sender's non-k1 authentication, and the signed
    calls grouped in `call_phases`, each linked by `call_index` to the call that executed it (0 if
    it never ran). `TransactionReceipt.account_abstraction_trx_output.committed_phases` counts the
    phases that committed.
  - The root call goes from the sender to itself and runs no code. It carries the writes the
    executor makes outside the calls: the payer's gas purchase and refund, the fee vault and
    beneficiary credits, the nonce or nonce manager writes, the account changes and their logs.
    Each signed call that ran is a child of the root call.
  - A transaction whose phase reverted is `FAILED`, but the phases before it keep their state: only
    the reverted phase's calls are `state_reverted`, and the root call never is.
  - Covered by `crates/execution/firehose-tests/tests/eip8130.rs`, which runs signed transactions
    through the Base executor on an Everest genesis (and a Zenith one for the Keystore cases). No
    Docker system test yet.
- Versions before `v1.5.0-fh3.1-2` cannot map type 121 and stop on the first EIP-8130 transaction,
  with a message naming it. Run `v1.5.0-fh3.1-2` or later before Everest activates on a network we
  serve.
- The activation block writes a code stub to the nonce manager in `apply_pre_execution_changes`
  (`ensure_eip8130_system_accounts`). `BasePreExecutionWrites` lists it, so it is emitted as a
  block-level code change.
