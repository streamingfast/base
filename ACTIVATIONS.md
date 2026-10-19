# Network activations and the Firehose block

One row per Base upgrade defined in the code this fork ships, with its status on the networks we
serve and its effect on the Firehose block. Review new and changed entries on every upstream bump,
and refresh the statuses when an activation is announced.

Denim and Everest are contract-backed: an L1 upgrade signal (`crates/utilities/upgrade-signal`) can
schedule them on a network with no new node release. Zenith can only be set in genesis.

Statuses checked 2026-10-16 against Base `v1.5.0` (`crates/common/chains/src/config.rs`).

| Upgrade | Base mainnet | Base Sepolia | Effect on the Firehose block |
|---------|--------------|--------------|------------------------------|
| Bedrock … Jovian | Active (Jovian 2025-12-02) | Active | Already in the stream. Canyon's `create2deployer` install is a direct DB write in `apply_pre_execution_changes`; it is emitted as a block-level code change when the block is traced with `v1.5.0-fh3.1` or later. |
| Azul | Active 2026-05-28 18:00 UTC | Active 2026-04-20 18:00 UTC | Already in the stream. |
| Beryl | Active 2026-06-25 18:00 UTC | Active 2026-06-18 18:00 UTC | Already in the stream. |
| Cobalt | Active 2026-09-30 18:00 UTC | Active 2026-09-23 18:00 UTC | B-20 V2 precompiles. Covered by `etc/systems/tests/firehose_cobalt_b20.rs`. |
| Denim | Not scheduled | 2026-10-21 18:00 UTC (`1_792_605_600`) | See [Denim](#denim). |
| Everest | Not scheduled | Not scheduled | See [Everest](#everest). **The node stops on the first EIP-8130 transaction.** |
| Zenith | Never (genesis only) | Never (genesis only) | Gates the EIP-8130 keystore (account configuration changes, non-k1 authenticators). Unreachable on real networks. |

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

## Everest

- Gates EIP-8130 transactions (type `0x79` = 121) in the EVM, the transaction pool, RPC and
  derivation. Before v1.5.0 this was Zenith, which no real network can activate.
- `reth-firehose` cannot map type 121: `firehose-tracer` 5.5.0 `TxType` and the block model have
  no such type. The mapper panics on it with a message naming the transaction and its type
  (`crates/firehose/src/mapper.rs` in `streamingfast/reth`). Halting is deliberate: dropping the
  transaction would emit a block that differs from the chain, and failing block execution would
  make the node report a valid block as invalid.
- Tracing 8130 properly needs a new transaction type in the block model and the tracer, the 8130
  fields, and handling of its execution shape: pre-call writes and `NonceIncremented` logs outside
  any call frame, phases that revert after their calls reported success, payer prepay and refund,
  and fee vault credits outside the EVM. See [the preparation plan](docs/prepare_EIP-8130.md).
- The activation block writes a code stub to the nonce manager in `apply_pre_execution_changes`
  (`ensure_eip8130_system_accounts`). `BasePreExecutionWrites` lists it, so it is emitted as a
  block-level code change.
