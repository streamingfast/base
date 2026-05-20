//! Dedicated (non-global) [`firehose_tracer::Tracer`] used to emit flashblock partial-block
//! FIRE events without contending with the global live-block tracer.

use std::{fmt, io::Write};

use firehose_tracer::{Tracer, config::ChainConfig};
use reth_firehose::{SynchronizedStdout, stdout_lock};

/// Tracer-id stamped on the `FIRE INIT` line emitted by the dedicated flashblock tracer.
///
/// The global (live-block) tracer uses `"reth"`; using a distinct id makes the two streams
/// distinguishable in firehose logs without affecting the downstream encoded protocol.
pub const FLASHBLOCK_TRACER_ID: &str = "reth-flashblock";

/// Owns the per-process dedicated tracer used by the flashblock processor.
///
/// Two invariants matter:
///
/// 1. The handle MUST be constructed AFTER [`reth_firehose::init_tracer`] — that's the call
///    that installs the process-wide stdout lock; calling [`stdout_lock`] before is a panic.
/// 2. The wrapped writer is [`SynchronizedStdout`] backed by the SAME `Arc<Mutex<()>>` used
///    by the global tracer. Each `FIRE …\n` line is therefore atomic w.r.t. lines emitted by
///    the canonical live-block tracer, no matter the interleaving.
///
/// The dedicated tracer also emits its own `FIRE INIT` line with [`FLASHBLOCK_TRACER_ID`]
/// because [`firehose_tracer::Tracer::on_block_start`] panics if `on_blockchain_init` has not
/// been called on that specific tracer instance — `init_sent` is per-tracer.
pub struct FlashblocksTracerHandle {
    tracer: Tracer,
}

impl fmt::Debug for FlashblocksTracerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksTracerHandle").finish_non_exhaustive()
    }
}

impl FlashblocksTracerHandle {
    /// Constructs the dedicated tracer with the given config and chain-config, sharing the
    /// process-wide stdout lock with the global tracer.
    ///
    /// `chain_config` matches what the live tracer received in [`reth_firehose::run_exex`]
    /// (chain id of the running node); a fresh `on_blockchain_init` is emitted on this
    /// instance — required because `Tracer` guards `on_block_start` behind its own per-instance
    /// `init_sent` flag.
    ///
    /// # Panics
    ///
    /// Panics if [`reth_firehose::init_tracer`] has not been called yet (no stdout lock to
    /// share).
    pub fn new(config: firehose_tracer::config::Config, chain_config: ChainConfig) -> Self {
        let lock = stdout_lock();
        let writer: Box<dyn Write + Send> = Box::new(SynchronizedStdout::new(lock));
        let mut tracer = Tracer::new_with_writer(config, writer);
        tracer.on_blockchain_init(
            FLASHBLOCK_TRACER_ID,
            env!("CARGO_PKG_VERSION"),
            chain_config,
        );
        Self { tracer }
    }

    /// Returns a mutable borrow of the underlying tracer for use by `FirehoseBlockTracer`.
    pub const fn tracer_mut(&mut self) -> &mut Tracer {
        &mut self.tracer
    }
}
