#![doc = include_str!("../README.md")]

mod prestate;
pub use prestate::run_prestate;

mod capture;
pub use capture::BaseFirehoseCapture;
// The tracing-regression framework is chain-agnostic and lives in `firehose-tracer-test`.
pub use firehose_tracer_test::{
    BlockDiff, BlockInvariants, BlockProjection, FirehoseCapture, Golden, InvariantConfig,
    SymbolTable, Violation, VolatilePolicy,
};
pub use reth_firehose_tests::RunOutcome;
