//! Firehose tracer wiring for `base-reth-node`.
//!
//! Initializes the process-wide Firehose tracer and installs:
//! * the startup hook that emits `FIRE INIT` and, on an empty chain, the genesis block. Canonical
//!   blocks are then traced by the engine as it executes them;
//! * (optionally) the [`FirehoseFlashblocksExtension`] that subscribes to a
//!   flashblock WebSocket feed and emits per-flashblock partial-block FIRE
//!   events via a separate, dedicated tracer.

use std::{
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base_common_genesis::DenimTimestampSchedule;
use base_firehose_flashblocks::{
    FirehoseFlashblocksProcessor, FirehoseFlashblocksStreamer, FlashblocksTracerHandle,
};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use firehose_tracer::types::BlockData;
use reth_chain_state::CanonStateSubscriptions;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_engine_primitives::ConsensusEngineEvent;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::{debug, info, warn};
use url::Url;

/// Runner-level extension that emits `FIRE INIT` and, on an empty chain, the genesis block.
///
/// Must run in the component-initialized hook: the tracer panics on a block that starts before
/// `on_blockchain_init`, and the consensus engine can execute blocks before node-started hooks run.
#[derive(Debug)]
pub struct FirehoseExtension;

impl BaseNodeExtension for FirehoseExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_component_initialized_hook(|node| {
            let chain_spec = node.provider.chain_spec();
            let _ = DENIM_SCHEDULE.set(chain_spec.denim_timestamp_schedule()?);
            reth_firehose::tracer().on_blockchain_init(
                "reth",
                concat!("base-", env!("CARGO_PKG_VERSION")),
                firehose_tracer::config::ChainConfig::new(chain_spec.chain().id()),
            );
            reth_firehose::emit_genesis_block_on_empty_chain(&node.provider, chain_spec.genesis())
        })
    }
}

impl FromExtensionConfig for FirehoseExtension {
    type Config = ();

    fn from_config(_: Self::Config) -> Self {
        Self
    }
}

/// The running chain's Denim schedule (`None` when Denim is not scheduled), read by
/// [`fire_block_time`]. Set before `FIRE INIT`, so before any block is traced.
static DENIM_SCHEDULE: OnceLock<Option<DenimTimestampSchedule>> = OnceLock::new();

/// The FIRE BLOCK line time, which becomes the Firehose block time.
fn fire_block_time(block: &BlockData) -> SystemTime {
    block_time(DENIM_SCHEDULE.get().and_then(Option::as_ref), block)
}

/// From Denim on, consensus requires a block's `BaseTime` deposit to carry the millisecond part
/// of the schedule's time for its number, so the schedule gives the block's time in milliseconds.
/// Earlier blocks use the header time in seconds.
fn block_time(schedule: Option<&DenimTimestampSchedule>, block: &BlockData) -> SystemTime {
    match schedule {
        Some(schedule) if schedule.is_denim_active_at_block(block.number) => {
            UNIX_EPOCH + Duration::from_millis(schedule.block_timestamp_millis(block.number))
        }
        _ => UNIX_EPOCH + Duration::from_secs(block.time),
    }
}

fn tracer_config() -> firehose_tracer::config::Config {
    firehose_tracer::config::Config {
        chain_client: firehose_tracer::config::ChainClient::Reth,
        fire_block_time: Some(fire_block_time),
        ..Default::default()
    }
}

/// Initializes the process-wide Firehose tracer with the default `reth` chain-client config.
pub fn init() {
    reth_firehose::init_tracer(tracer_config());
}

/// Runner-level extension that subscribes to a flashblock WebSocket feed and emits one
/// partial-block FIRE event per flashblock on a dedicated tracer (separate from the global
/// canonical-block tracer).
///
/// Construction is gated by [`reth_firehose::is_tracer_initialized`] at the point where the
/// extension is installed; without the global tracer in place, the dedicated tracer has no
/// stdout lock to share and the extension would panic on first construction.
#[derive(Debug)]
pub struct FirehoseFlashblocksExtension {
    ws_url: Url,
}

impl FirehoseFlashblocksExtension {
    /// Creates a new flashblocks extension targeting the given WebSocket URL.
    pub const fn new(ws_url: Url) -> Self {
        Self { ws_url }
    }
}

impl BaseNodeExtension for FirehoseFlashblocksExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let ws_url = self.ws_url;

        hooks.add_node_started_hook(move |full_node| {
            if !reth_firehose::is_tracer_initialized() {
                warn!(
                    url = %ws_url,
                    "skipping Firehose flashblocks streamer: Firehose tracer is not initialized"
                );
                return Ok(());
            }
            let chain_id = full_node.provider.chain_spec().chain().id();
            let chain_config = firehose_tracer::config::ChainConfig::new(chain_id);
            let tracer = FlashblocksTracerHandle::new(tracer_config(), chain_config);
            let processor = FirehoseFlashblocksProcessor::new(full_node.provider.clone(), tracer);
            info!(url = %ws_url, "starting Firehose flashblocks streamer");
            let streamer = FirehoseFlashblocksStreamer::new(processor, ws_url);
            // All canonical signals feed the processor's single serialized command queue, so
            // they are applied in strict arrival order relative to each other and to the
            // WebSocket flashblock stream — never concurrently. The `final_part_sent` guard
            // inside the processor prevents double-emission when several signals deliver the
            // same (block_number, hash).
            let canonical_block_sender = streamer.canonical_block_sender();
            streamer.start();

            // Earliest in-engine signals, from the consensus engine's event broadcast.
            let mut engine_events = full_node.add_ons_handle.engine_events.new_listener();
            let canonical_block_sender_for_engine = canonical_block_sender.clone();
            tokio::spawn(async move {
                while let Some(event) = engine_events.next().await {
                    let (number, hash, source) = match event {
                        // Fires the moment the engine receives a payload from the consensus
                        // layer, before validation and insertion. Mirrors geth's
                        // `SendNotification` call at the top of `newPayload`
                        // (`eth/catalyst/api.go:730`), letting the processor trigger is_final
                        // ~175 ms earlier than the post-commit path. If the payload is later
                        // rejected, the hash-mismatch branch on the subsequent
                        // `CanonicalBlockAdded` / canonical-state notification reconciles.
                        ConsensusEngineEvent::BlockReceived(num_hash) => {
                            (num_hash.number, num_hash.hash, "BlockReceived")
                        }
                        // Fires after the engine has validated and added the block to the
                        // canonical chain. Carries the same (number, hash) — racing with
                        // BlockReceived; whichever arrives first triggers the is_final emission,
                        // the other is a no-op via `final_part_sent`.
                        ConsensusEngineEvent::CanonicalBlockAdded(block, _elapsed) => (
                            block.recovered_block.number,
                            block.recovered_block.hash(),
                            "CanonicalBlockAdded",
                        ),
                        _ => continue,
                    };
                    debug!(
                        target: "firehose::flashblocks",
                        block_number = number,
                        block_hash = %hash,
                        source,
                        "engine canonical signal",
                    );
                    canonical_block_sender_for_engine.send(number, hash);
                }
            });

            // Fallback path: canonical-state notification fires after the canonical chain has
            // been committed. The serialized queue applies it after the early signal (when both
            // deliver the same block), so `final_part_sent` reliably suppresses double-emission.
            let mut canonical_stream =
                BroadcastStream::new(full_node.provider.subscribe_to_canonical_state());
            tokio::spawn(async move {
                while let Some(notification) = canonical_stream.next().await {
                    let notification = match notification {
                        Ok(n) => n,
                        Err(err) => {
                            warn!(error = %err, "canonical-state broadcast lagged; continuing");
                            continue;
                        }
                    };
                    for block in notification.committed().blocks_iter() {
                        canonical_block_sender.send(block.number, block.hash());
                    }
                }
            });
            Ok(())
        })
    }
}

impl FromExtensionConfig for FirehoseFlashblocksExtension {
    type Config = Option<Url>;

    fn from_config(ws_url: Self::Config) -> Self {
        Self::new(ws_url.expect("FirehoseFlashblocksExtension::from_config requires a URL"))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use base_execution_chainspec::BaseChainSpec;

    use super::*;

    #[test]
    fn block_time_follows_the_denim_schedule() {
        // Denim activates at block 5 (time 1010); blocks then come every 200 ms.
        let schedule = DenimTimestampSchedule {
            genesis_block_number: 0,
            genesis_timestamp: 1000,
            legacy_block_interval: NonZeroU64::new(2).unwrap(),
            denim_activation_timestamp: 1010,
        };
        let block = |number, time| BlockData { number, time, ..Default::default() };

        for (number, time, want_ms) in [
            (4, 1008, 1_008_000),
            (5, 1010, 1_010_000),
            (6, 1010, 1_010_200),
            (10, 1011, 1_011_000),
        ] {
            assert_eq!(
                block_time(Some(&schedule), &block(number, time)),
                UNIX_EPOCH + Duration::from_millis(want_ms),
                "block {number}"
            );
        }
        assert_eq!(block_time(None, &block(6, 1010)), UNIX_EPOCH + Duration::from_secs(1010));
    }

    #[test]
    fn mainnet_block_time_switches_to_milliseconds_at_denim() {
        // Denim is configured for 1_793_901_600 and activates on the first 2-second slot at or
        // after it: block 53_556_127, at 1_793_901_601.
        let schedule = BaseChainSpec::mainnet()
            .denim_timestamp_schedule()
            .expect("mainnet has a block time")
            .expect("mainnet schedules Denim");
        let block = |number, time| BlockData { number, time, ..Default::default() };

        for (number, time, want_ms) in [
            (53_556_126, 1_793_901_599, 1_793_901_599_000),
            (53_556_127, 1_793_901_601, 1_793_901_601_000),
            (53_556_128, 1_793_901_601, 1_793_901_601_200),
            (53_556_132, 1_793_901_602, 1_793_901_602_000),
        ] {
            assert_eq!(
                block_time(Some(&schedule), &block(number, time)),
                UNIX_EPOCH + Duration::from_millis(want_ms),
                "block {number}"
            );
        }
    }
}
