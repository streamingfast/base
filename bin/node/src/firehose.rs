//! Firehose tracer wiring for `base-reth-node`.
//!
//! Initializes the process-wide Firehose tracer and installs:
//! * the canonical-block `ExEx` that re-executes committed chains through the
//!   global Firehose inspector;
//! * (optionally) the [`FirehoseFlashblocksExtension`] that subscribes to a
//!   flashblock WebSocket feed and emits per-flashblock partial-block FIRE
//!   events via a separate, dedicated tracer.

use alloy_primitives::B256;
use base_firehose_flashblocks::{
    FirehoseFlashblocksProcessor, FirehoseFlashblocksStreamer, FlashblocksTracerHandle,
};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_chain_state::CanonStateSubscriptions;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_engine_primitives::ConsensusEngineEvent;
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::{debug, info, warn};
use url::Url;

/// Runner-level extension that installs the Firehose `ExEx`.
#[derive(Debug)]
pub struct FirehoseExtension;

impl BaseNodeExtension for FirehoseExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.install_exex("firehose", |ctx| async move {
            Ok(async move { reth_firehose::run_exex(ctx).await })
        })
    }
}

impl FromExtensionConfig for FirehoseExtension {
    type Config = ();

    fn from_config(_: Self::Config) -> Self {
        Self
    }
}

/// Initializes the process-wide Firehose tracer with the default `reth` chain-client config.
pub fn init() {
    reth_firehose::init_tracer(firehose_tracer::config::Config {
        chain_client: firehose_tracer::config::ChainClient::Reth,
        ..Default::default()
    });
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

        // Both hooks need to push canonical blocks into the processor, but the processor is only
        // available in the node-started hook. Use an unbounded mpsc so the engine-event hook can
        // forward early signals (`BlockReceived` at newPayload arrival, `CanonicalBlockAdded`
        // after validation+insertion) without knowing the processor's concrete type. The
        // `final_part_sent` guard inside the processor prevents double-emission when both
        // variants deliver the same (block_number, hash).
        let (canonical_tx, mut canonical_rx) = mpsc::unbounded_channel::<(u64, B256)>();

        let hooks = hooks.add_engine_event_listener_hook(move |mut events| {
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
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
                        // Fallback: fires after the engine has validated and added the block
                        // to the canonical chain. Carries the same (number, hash) — racing
                        // with BlockReceived; whichever arrives first triggers the
                        // is_final emission, the other is a no-op via `final_part_sent`.
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
                    if canonical_tx.send((number, hash)).is_err() {
                        return;
                    }
                }
            });
        });

        let ws_url_for_node = ws_url.clone();
        hooks.add_node_started_hook(move |full_node| {
            if !reth_firehose::is_tracer_initialized() {
                warn!(
                    url = %ws_url_for_node,
                    "skipping Firehose flashblocks streamer: Firehose tracer is not initialized"
                );
                return Ok(());
            }
            let chain_id = full_node.provider.chain_spec().chain().id();
            let chain_config = firehose_tracer::config::ChainConfig::new(chain_id);
            let tracer_config = firehose_tracer::config::Config {
                chain_client: firehose_tracer::config::ChainClient::Reth,
                ..Default::default()
            };
            let tracer = FlashblocksTracerHandle::new(tracer_config, chain_config);
            let processor =
                FirehoseFlashblocksProcessor::new(full_node.provider.clone(), tracer);
            info!(url = %ws_url_for_node, "starting Firehose flashblocks streamer");
            let streamer = FirehoseFlashblocksStreamer::new(processor, ws_url_for_node);
            // Both canonical signals feed the processor's single serialized command queue, so
            // they are applied in strict arrival order relative to each other and to the
            // WebSocket flashblock stream — never concurrently.
            let canonical_sender = streamer.canonical_sender();
            streamer.start();

            // Earliest in-engine signal: drain canonical blocks forwarded by the engine-event
            // listener installed above.
            let canonical_sender_for_engine = canonical_sender.clone();
            tokio::spawn(async move {
                while let Some((number, hash)) = canonical_rx.recv().await {
                    canonical_sender_for_engine.send(number, hash);
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
                        canonical_sender.send(block.number, block.hash());
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
