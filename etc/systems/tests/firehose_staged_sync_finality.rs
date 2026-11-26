//! Firehose finality regression for a reader that starts behind the chain tip, driven by the real
//! system stack.
//!
//! A follower that is more than `MIN_BLOCKS_FOR_PIPELINE_RUN` (32) blocks behind catches up
//! through reth's staged sync (the pipeline) instead of the live engine path. On OP Stack the
//! backfill target is the unsafe head, not the finalized block, so the pipeline replays unsafe
//! blocks. The `FIRE BLOCK` lines it emits must still advertise the node's finalized block as
//! `libNum`, never the replayed block itself.
//!
//! See `firehose_b20.rs` for why this runs against the Docker-backed stack and why the binary holds
//! a single test (the Firehose tracer is a process-wide singleton).

mod common;

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use base_firehose_tests::BaseFirehoseCapture;
use base_system_tests::{SystemTestStack, SystemTestStackBuilder};
use eyre::{OptionExt, Result, WrapErr, ensure};

/// Leaves enough pre-Denim runway for the stack to start, as `firehose_denim.rs` does. Denim
/// switches the chain to the 200 ms cadence of Base mainnet, so the gap below opens quickly.
const DENIM_ACTIVATION_BLOCK: u64 = 25;
/// Last block the first follower traces through the live path before it is replaced.
const LIVE_BLOCKS: u64 = DENIM_ACTIVATION_BLOCK + 5;
/// How far the builder runs ahead of the stopped follower. Its replacement starts from genesis, so
/// it is even further behind, well past reth's 32-block pipeline threshold.
const GAP_BLOCKS: u64 = 64;
/// How long the builder may take to open the gap (64 blocks at 200 ms is about 13 s).
const GAP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the follower to trace a block.
const TRACE_TIMEOUT: Duration = Duration::from_secs(120);
/// How far `libNum` may trail the block before the tracer clamps it up to `number - 200`.
const MAX_LIB_DISTANCE: u64 = 200;
/// The engine tree counter incremented on every staged sync run.
const PIPELINE_RUNS_METRIC: &str = "consensus_engine_beacon_pipeline_runs";

#[tokio::test(flavor = "multi_thread")]
async fn staged_sync_advertises_finalized_block() -> Result<()> {
    // Must happen before the stack starts producing blocks: the traced execution paths are gated
    // on `reth_firehose::is_tracer_initialized()`.
    let capture = BaseFirehoseCapture::install(common::L2_CHAIN_ID, Some(0), Some(0), None);

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(common::L1_CHAIN_ID)
        .with_l2_chain_id(common::L2_CHAIN_ID)
        .with_base_cobalt_activation_block(0)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK)
        .build()
        .await?;

    capture.wait_for_block(LIVE_BLOCKS, TRACE_TIMEOUT).await?;
    let lines_before_replacement = fire_block_libs(&capture.raw_text()).count();

    let system = system.replace_l2_client(GAP_BLOCKS, GAP_TIMEOUT).await?;
    let builder = system.l2_builder_provider()?;
    let tip = builder.get_block_number().await?;
    capture
        .wait_for_block(tip, TRACE_TIMEOUT)
        .await
        .wrap_err("replacement follower did not trace the builder's tip")?;

    ensure!(
        pipeline_runs(&system).await? > 0.0,
        "the replacement follower caught up without staged sync, so the scenario was not exercised"
    );

    // Finality only moves forward, so the builder's finalized block now bounds the finalized block
    // of the follower at the time it emitted any of the lines below.
    let finalized = builder
        .get_block_by_number(BlockNumberOrTag::Finalized)
        .await?
        .map_or(0, |block| block.header.number);

    let replayed: Vec<(u64, u64)> =
        fire_block_libs(&capture.raw_text()).skip(lines_before_replacement).collect();
    ensure!(
        replayed.len() as u64 >= tip,
        "replacement follower traced only {} blocks, fewer than the {tip} it synced",
        replayed.len()
    );

    let above_finalized: Vec<(u64, u64)> = replayed
        .into_iter()
        .filter(|(number, lib)| *lib > finalized.max(number.saturating_sub(MAX_LIB_DISTANCE)))
        .collect();
    ensure!(
        above_finalized.is_empty(),
        "{} blocks traced by the replacement advertise a libNum above the finalized block \
         #{finalized}, as (block, libNum): {above_finalized:?}",
        above_finalized.len()
    );

    system.shutdown().await
}

/// Yields `(block number, libNum)` for each `FIRE BLOCK` line, in emission order.
///
/// Line format: `FIRE BLOCK <num> <flash_idx> <hash> <prev_num> <prev_hash> <lib_num> <ts> <payload>`.
fn fire_block_libs(raw: &str) -> impl Iterator<Item = (u64, u64)> + '_ {
    raw.lines().filter_map(|line| {
        let fields: Vec<&str> = line.strip_prefix("FIRE BLOCK ")?.split(' ').collect();
        Some((fields.first()?.parse().ok()?, fields.get(5)?.parse().ok()?))
    })
}

/// Reads the staged sync run counter from the follower's Prometheus endpoint.
async fn pipeline_runs(system: &SystemTestStack) -> Result<f64> {
    let metrics_url = system.l2_stack().client().metrics_url()?;
    let body = reqwest::get(metrics_url).await?.error_for_status()?.text().await?;
    body.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (name, value) = line.rsplit_once(' ')?;
            name.ends_with(PIPELINE_RUNS_METRIC).then(|| value.parse().ok()).flatten()
        })
        .ok_or_eyre("follower exposes no staged sync run counter")
}
