//! Firehose tracer wiring for `base-reth-node`.
//!
//! Initializes the process-wide Firehose tracer and installs:
//! * the canonical-block `ExEx` that re-executes committed chains through the
//!   global Firehose inspector;
//! * (optionally) the [`FirehoseFlashblocksExtension`] that subscribes to a
//!   flashblock WebSocket feed and emits per-flashblock partial-block FIRE
//!   events via a separate, dedicated tracer.

use base_firehose_flashblocks::{
    FirehoseFlashblocksProcessor, FirehoseFlashblocksStreamer, FlashblocksTracerHandle,
};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_chain_state::CanonStateSubscriptions;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::{info, warn};
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
            let tracer_config = firehose_tracer::config::Config {
                chain_client: firehose_tracer::config::ChainClient::Reth,
                ..Default::default()
            };
            let tracer = FlashblocksTracerHandle::new(tracer_config, chain_config);
            let processor =
                FirehoseFlashblocksProcessor::new(full_node.provider.clone(), tracer);
            info!(url = %ws_url, "starting Firehose flashblocks streamer");
            let streamer = FirehoseFlashblocksStreamer::new(processor, ws_url);
            let processor_for_canonical = streamer.processor();
            streamer.start();

            // Drive `FirehoseFlashblocksProcessor::on_canonical_block` from the node's
            // canonical-state notifications. When a block N becomes canonical, any flashblocks
            // for block N+1 that were buffered because their parent state wasn't yet committed
            // get replayed and emitted to the dedicated flashblocks tracer.
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
                        processor_for_canonical.on_canonical_block(block.number, block.hash());
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
