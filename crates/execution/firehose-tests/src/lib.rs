#![doc = include_str!("../README.md")]

// The tracing-regression framework is chain-agnostic and lives in `firehose-tracer-test`.
pub use firehose_tracer_test::{
    BlockDiff, BlockInvariants, BlockProjection, FirehoseCapture, Golden, InvariantConfig,
    ProductionReplay, SymbolTable, Violation, VolatilePolicy,
};
pub use reth_firehose_tests::RunOutcome;

mod prestate;
pub use prestate::run_prestate;

mod capture;
pub use capture::BaseFirehoseCapture;
