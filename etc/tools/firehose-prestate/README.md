Generate replayable Firehose prestate fixtures for real Base transactions.

This is the Base wrapper around
[`firehose-tracer-prestate`](https://crates.io/crates/firehose-tracer-prestate),
the chain-agnostic Rust port of `streamingfast/go-ethereum`'s
[`generate-prestate`](https://github.com/streamingfast/go-ethereum/blob/release/geth-v1.17.x-fh3.0/eth/tracers/internal/tracetest/firehose/generate-prestate/main.go).
Point it at a transaction hash and an archive node and it writes a `prestate.json` that
`base_firehose_tests::run_prestate` replays through the Firehose tracer with no node, no Docker and
no network.

Base supplies the four things the shared crate cannot know: the `BaseTxEnvelope` type, the genesis
`config` (re-projected from `ChainConfig`, because the chain spec's own genesis drops every
timestamped fork), the `L1Block` predeploy slots the L1-cost function reads outside the EVM journal,
and the chain id.

## Generating a fixture

```console
$ export ARCHIVE_ENDPOINT=https://<archive-node>
$ cargo run -p base-firehose-prestate -- generate \
    --network base \
    --tx 0x32ecdb4e72df6ec331edb81256b58a768ba49d1e3e89a1a071b980a85d6b72c0 \
    --out crates/execution/firehose-tests/tests/cases/base_mainnet_replay
```

The endpoint must expose `debug_traceTransaction` with the `prestateTracer` over archive state.
Most public Base RPCs do not; `https://base.drpc.org` does.

The fixture is `{ genesis, context, input }`:

- `genesis` — the parent block's header fields, the `prestateTracer` alloc, and the network's real
  fork schedule taken straight from `BaseChainSpec` rather than a hand-copied table.
- `context` — the traced block's number, timestamp, gas limit, coinbase, difficulty and base fee.
- `input` — the transaction, EIP-2718 encoded.

### Why the `L1Block` predeploy is seeded explicitly

The OP L1-cost function reads the `L1Block` predeploy (`0x4200…0015`) storage straight from the
state database, outside the EVM journal, so `debug_traceTransaction` never reports those slots. A
fixture without them replays with a zero L1 fee and every fee balance change comes out wrong. The
generator therefore seeds the slots `L1BlockInfo::try_fetch` reads, listed locally from the
already-public `L1BlockInfo` slot constants. They are read at the traced block, since those slots
are written by the L1-info deposit at index 0 of that same block. Anything the tracer did report
wins.
