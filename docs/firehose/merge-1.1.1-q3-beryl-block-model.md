# Q3 — Does the Beryl activation affect the block model / firehose blocks?

**Short answer: no.** Beryl does not change the block model (header fields, transaction
types, receipt format) that firehose serializes. v1.1.1 only *schedules* Beryl on mainnet;
the Beryl code already shipped in v1.1.0-fh.

## What the merge actually did

The only Beryl change in v1.1.1 is a timestamp flip in `crates/common/chains/src/config.rs`:

```rust
// base mainnet
beryl_timestamp: Some(1_782_410_400),   // was None  ("Never")
```

Plus the matching test assertions. No Beryl *logic* changed — that all predates the merge.
So the question reduces to: "what does activating Beryl do, and does any of it reach the
firehose block?"

## Beryl introduces no new Ethereum EVM spec

`BaseUpgrade::Beryl` maps to `SpecId::OSAKA` — **the same Ethereum spec as the preceding
`Azul` upgrade** (`crates/common/chains/src/upgrade.rs`, `crates/common/evm/src/spec.rs`):

```
Azul   => SpecId::OSAKA
Beryl  => SpecId::OSAKA
Cobalt => SpecId::OSAKA
```

Therefore Beryl adds **no new transaction types, no new header fields, and no new receipt
fields** at the EL-spec level. Anything OSAKA-related (and everything from Prague/Cancun
below it) was already reachable under Azul. The firehose Ethereum block model already
handles this spec.

## What Beryl actually adds: Base-native precompiles

Beryl installs a dynamic precompile lookup (`BerylLookup`, in
`crates/common/precompiles/`): the activation registry, the B20 factory / stablecoin /
asset precompiles, and policy precompiles. These are contracts at fixed addresses, invoked
by ordinary `CALL`s from transactions.

Firehose impact: **none structural.** A precompile call appears to the firehose tracer as
a normal internal call frame to the precompile's address, with its gas, input, output, and
any resulting account/storage/balance changes — all of which firehose already captures the
same way it captures any other call. There is no new block-, transaction-, or
receipt-level field to serialize.

The one operational note (not a block-model issue): Beryl's activation-registry precompile
requires an admin configured at genesis (`config.rs` comment). That is a chain-config /
genesis concern, not something that alters the firehose block.

## Bottom line for firehose

- No firehose protobuf / block-model change is required for Beryl.
- When mainnet crosses `1782410400`, firehose blocks will start containing transactions
  that call the Beryl precompiles; these serialize as ordinary call traces.
- Worth a one-time sanity check on a Beryl-active block (sepolia activates earlier, at
  `1781805600`) to confirm precompile call frames trace as expected — but this exercises
  pre-existing tracer code, not anything introduced by this merge.
