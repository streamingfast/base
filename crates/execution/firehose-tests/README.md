Base-side bindings for the Firehose tracing-regression framework.

The framework itself — `FirehoseCapture`, `BlockInvariants`, `BlockProjection` / `SymbolTable` /
`VolatilePolicy`, `Golden` — is chain-agnostic and lives in `firehose-tracer-test` (in
`evm-firehose-tracer-rs`). This crate re-exports it and adds the two Base-specific pieces:

- `run_prestate` — replays a hand-written `prestate.json` (genesis + block context + one signed
  transaction) through the tracer with no node involved. Fast, hermetic, but the fixture is
  hand-maintained.
- `BaseFirehoseCapture::install` — the reth binding: installs the process-wide buffer-backed tracer
  (`reth_firehose::init_tracer_with_buffer`) and hands the buffer to `FirehoseCapture`, so a test
  driving a real node captures the `FIRE BLOCK` lines it emits.

Assert on the captured `Block` with:

- `BlockInvariants` — content-independent property assertions that never need regenerating.
- `BlockProjection` + `Golden` — a descriptor-driven JSON golden with volatile fields removed by a
  `VolatilePolicy` (`none()` for reproducible prestate replays, `live_node()` for system tests).

Regenerate every golden with `GOLDEN_UPDATE=1`.

## Cases replaying a real transaction

`tests/cases/base_mainnet_replay/` replays a real Base mainnet transaction. Its `prestate.json`
comes from `base-firehose-prestate generate` (`etc/tools/firehose-prestate`) against an archive
node, and its golden was *seeded* by `base-firehose-prestate reference` from StreamingFast's
production Firehose. The first run of the test was therefore a direct comparison against
production — which is the whole point of seeding it that way, and it matched.

That validation is deliberately a one-off. It established that the generator and the tracer agree
with production; from then on the golden is an ordinary one, regenerated with `GOLDEN_UPDATE=1`
like any other, and no test needs credentials or a production endpoint. Further cases only need
`generate`.

`ProductionReplay` is the projection both sides share; it lives in `firehose-tracer-test` and is
re-exported here. The replay runs the transaction alone in a synthetic single-transaction block, so
it excludes exactly the block-wide positional fields — ordinals, the transaction's index, each
log's block index, and the cumulative gas used — and keeps everything else verbatim.
