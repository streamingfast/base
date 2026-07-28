//! Base's reth binding for the shared [`FirehoseCapture`] framework.
//!
//! The capture / invariants / projection / golden machinery lives in `firehose-tracer-test` and is
//! chain-agnostic. Installing the process-wide tracer is the one part that cannot: the
//! `GLOBAL_TRACER` singleton and the `is_tracer_initialized()` gate the live engine path checks
//! (`crates/execution/engine-tree/src/validator.rs`) both live in `reth-firehose`. This module is
//! the thin adapter that installs reth's buffer-backed tracer and hands the buffer to
//! [`FirehoseCapture`].

use firehose_tracer_test::FirehoseCapture;

/// Base-specific installer for the shared Firehose capture framework.
#[derive(Debug)]
pub struct BaseFirehoseCapture;

impl BaseFirehoseCapture {
    /// Installs the process-wide buffer-backed tracer and returns a capture handle over it.
    ///
    /// Must be called before any block is validated by the node under test — the traced execution
    /// path is gated on `reth_firehose::is_tracer_initialized()`. Panics if a tracer was already
    /// installed in this process (the tracer is a process-wide singleton, so a test binary using
    /// this holds a single `#[test]`).
    ///
    /// The fork timestamps only affect how block contents are mapped, not whether a block is
    /// emitted; `Some(0)` means "active from genesis" and `None` means "never".
    pub fn install(
        chain_id: u64,
        shanghai_time: Option<u64>,
        cancun_time: Option<u64>,
        prague_time: Option<u64>,
    ) -> FirehoseCapture {
        let buffer = reth_firehose::init_tracer_with_buffer(
            chain_id,
            shanghai_time,
            cancun_time,
            prague_time,
        );
        FirehoseCapture::new(buffer)
    }
}
