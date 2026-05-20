//! Top-level wiring: combines [`FirehoseFlashblocksProcessor`] with the existing
//! [`base_flashblocks::FlashblocksSubscriber`] so a single `start()` call spawns the WebSocket
//! reader, dispatch loop, and per-flashblock Firehose emission task.

use std::sync::Arc;

use alloy_consensus::Header;
use base_common_chains::Upgrades;
use base_flashblocks::FlashblocksSubscriber;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use url::Url;

use crate::FirehoseFlashblocksProcessor;

/// Owns the [`FirehoseFlashblocksProcessor`] + WebSocket subscriber and exposes a single
/// `start()` entrypoint to be called from the node-started hook of the node binary.
#[derive(Debug)]
pub struct FirehoseFlashblocksStreamer<Client> {
    processor: Arc<FirehoseFlashblocksProcessor<Client>>,
    ws_url: Url,
}

impl<Client> FirehoseFlashblocksStreamer<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Constructs the streamer with a pre-built processor (which already carries its dedicated
    /// tracer) and the WebSocket URL to subscribe to.
    pub fn new(processor: FirehoseFlashblocksProcessor<Client>, ws_url: Url) -> Self {
        Self { processor: Arc::new(processor), ws_url }
    }

    /// Spawns the subscriber. The subscriber owns its own reconnect-backoff loop, so this call
    /// returns immediately and the resulting tasks run for the lifetime of the tokio runtime.
    pub fn start(self) {
        let mut subscriber =
            FlashblocksSubscriber::new(Arc::clone(&self.processor), self.ws_url.clone());
        subscriber.start();
    }
}
