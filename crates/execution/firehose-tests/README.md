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
