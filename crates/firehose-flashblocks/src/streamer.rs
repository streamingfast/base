//! Top-level wiring: combines [`FirehoseFlashblocksProcessor`] with the existing
//! [`base_flashblocks::FlashblocksSubscriber`] so a single `start()` call spawns the WebSocket
//! reader, the serialized command-queue consumer, and per-flashblock Firehose emission.

use std::{sync::Arc, time::Duration};

use alloy_consensus::Header;
use base_common_chains::Upgrades;
use base_flashblocks::FlashblocksSubscriber;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use url::Url;

use crate::{
    CanonicalBlockSender, FirehoseFlashblocksDispatcher, FirehoseFlashblocksProcessor,
    FlashblockEnqueuer, ProcessorCommand,
};

/// Owns the [`FirehoseFlashblocksProcessor`] + WebSocket subscriber and exposes a single
/// `start()` entrypoint to be called from the node-started hook of the node binary.
///
/// All mutating events — WebSocket flashblocks and every canonical-block signal — are funnelled
/// through a single command queue drained by one consumer task, so the processor's state is
/// only ever mutated serially. Callers obtain a [`CanonicalBlockSender`] via [`Self::canonical_block_sender`]
/// to feed canonical-block notifications into that same queue.
#[derive(Debug)]
pub struct FirehoseFlashblocksStreamer<Client> {
    processor: Arc<FirehoseFlashblocksProcessor<Client>>,
    ws_url: Url,
    command_tx: UnboundedSender<ProcessorCommand>,
    command_rx: Option<UnboundedReceiver<ProcessorCommand>>,
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
    /// tracer) and the WebSocket URL to subscribe to. The command queue is created here so
    /// [`Self::canonical_block_sender`] is available before [`Self::start`] is called.
    pub fn new(processor: FirehoseFlashblocksProcessor<Client>, ws_url: Url) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        Self { processor: Arc::new(processor), ws_url, command_tx, command_rx: Some(command_rx) }
    }

    /// Returns a sender that feeds canonical-block notifications into the shared command queue.
    /// Clone it once per signal source (e.g. the early in-engine notification and the
    /// post-commit canonical-state broadcast).
    pub fn canonical_block_sender(&self) -> CanonicalBlockSender {
        CanonicalBlockSender::new(self.command_tx.clone())
    }

    /// Spawns the command-queue consumer and the WebSocket subscriber. The subscriber owns its
    /// own reconnect-backoff loop, so this call returns immediately and the resulting tasks run
    /// for the lifetime of the tokio runtime. Must be called from within a tokio runtime.
    pub fn start(mut self) {
        let command_rx = self
            .command_rx
            .take()
            .expect("FirehoseFlashblocksStreamer::start must be called exactly once");

        // Single consumer: the only task that mutates the processor's state.
        let dispatcher = FirehoseFlashblocksDispatcher::new(Arc::clone(&self.processor));
        tokio::spawn(dispatcher.run(command_rx));

        // WebSocket flashblocks enqueue into the same serialized queue.
        let enqueuer = Arc::new(FlashblockEnqueuer::new(self.command_tx.clone()));
        let mut subscriber =
            FlashblocksSubscriber::new(enqueuer, self.ws_url.clone(), Duration::from_millis(500));
        subscriber.start();
    }
}
