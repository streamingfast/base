//! Docker-free prestate coverage of the invariant and projection layers.
//!
//! Replays the `nop_transfer` prestate through the tracer and asserts the property invariants plus
//! a projection golden over its single transaction — the same two layers `base-system-tests`'
//! `firehose_b20` uses, without a node or Docker, so a regression in the harness itself surfaces in
//! the default workspace run.

use std::path::PathBuf;

use base_firehose_tests::{
    BlockInvariants, BlockProjection, Golden, SymbolTable, VolatilePolicy, run_prestate,
};

#[test]
fn nop_transfer() {
    let folder = case_dir();
    let outcome = run_prestate(&folder).expect("nop_transfer prestate must succeed");

    BlockInvariants::assert(&outcome.block).expect("captured block must satisfy the invariants");

    let trace = outcome
        .block
        .transaction_traces
        .first()
        .expect("nop_transfer block must hold one transaction");

    let symbols = SymbolTable::new()
        .with(&trace.from, "sender")
        .with(&trace.to, "recipient")
        .with(&outcome.block.header.as_ref().expect("header").coinbase, "coinbase");

    // A prestate replay is fully reproducible (no live node moving the base fee or the clock), so
    // keep every field — `VolatilePolicy::none()` — and let the golden catch the most it can.
    let projection = BlockProjection::new()
        .with_symbols(symbols)
        .with_policy(VolatilePolicy::none())
        .transaction(&outcome.block, &trace.hash)
        .expect("projecting the transaction must succeed");

    Golden::is_json_equal(&projection, &folder.join("block.2099.json"))
        .expect("comparing against golden must not error")
        .assert_equal();
}

fn case_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("cases").join("nop_transfer")
}
