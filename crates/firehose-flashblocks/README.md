# base-firehose-flashblocks

Firehose tracing for pre-canonical flashblock partial blocks.

This crate sits next to [`base-flashblocks`] (RPC pending-state path) and
[`base-execution-firehose`] (canonical-block tracing). It subscribes to a
flashblock WebSocket feed, re-executes each flashblock (base + deltas) through
a dedicated `firehose_tracer::Tracer`, and emits one partial-block `FIRE BLOCK`
event per flashblock so the downstream Firehose consumer can see chain state
ahead of the canonical engine-API confirmation.

The entire subsystem is a no-op unless `--firehose-flashblocks-url` is set on
the node binary; in that case the streamer runs alongside (but independently
of) the canonical live-block tracer driven by `reth_firehose::run_exex`.
