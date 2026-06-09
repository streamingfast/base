//! Single-consumer command queue that serializes every mutating event to the
//! [`FirehoseFlashblocksProcessor`].
//!
//! The processor's in-flight state (`accumulated_db`, the stored-flashblock buffer and the
//! per-block index bookkeeping) is driven by three independent producers in production: the
//! WebSocket flashblock stream and the two canonical-block signal sources wired in
//! `bin/node/src/firehose.rs` — the early in-engine notification and the post-commit
//! canonical-state broadcast. Previously each producer called the processor directly from its
//! own task, serialized only by the processor's internal `state` mutex — which `process_inner`
//! deliberately releases across the (~100 ms) EVM execution. That open window let a canonical
//! signal mutate or `reset` the very state being executed against, leaving `accumulated_db`
//! inconsistent with the transactions just emitted and producing wrong state roots (and, on the
//! emission side, duplicate `is_final` FIRE BLOCKs that the `final_part_sent` guard could not
//! dedup under the race).
//!
//! Funnelling all three producers through one channel drained by a single consumer task removes
//! the concurrency entirely: commands are applied in strict arrival order, each to completion,
//! before the next is dequeued. No producer ever touches `ProcessorState` directly.

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_primitives::B256;
use base_common_chains::Upgrades;
use base_common_flashblocks::Flashblock;
use base_flashblocks::FlashblocksReceiver;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, warn};

use crate::{FirehoseFlashblocksProcessor, FlashblockPeekClassifier};

/// A single mutating event applied to the processor by the consumer task, in arrival order.
#[derive(Debug)]
pub enum ProcessorCommand {
    /// A flashblock whose peek-derived classification (`squash`, `is_final_expected_hash`) was
    /// computed at ingress while the WS subscriber's peek reference was still live.
    ///
    /// The flashblock is boxed to avoid a large size imbalance with the small
    /// [`Self::CanonicalBlock`] variant.
    Flashblock {
        /// The received flashblock.
        flashblock: Box<Flashblock>,
        /// Whether execution+emission should be deferred to the next non-squashed flashblock.
        squash: bool,
        /// `Some(expected_parent_hash)` when the peek identified this flashblock as the final
        /// partial for its block.
        is_final_expected_hash: Option<B256>,
    },
    /// A canonical-block notification (number + hash) from any signal source.
    CanonicalBlock {
        /// Canonical block number.
        number: u64,
        /// Canonical block hash.
        hash: B256,
    },
}

/// Ingress handle for the WebSocket flashblock stream. Implements [`FlashblocksReceiver`] by
/// classifying the peek and enqueuing a [`ProcessorCommand::Flashblock`]; processing happens
/// later on the single consumer task.
#[derive(Debug, Clone)]
pub struct FlashblockEnqueuer {
    tx: UnboundedSender<ProcessorCommand>,
}

impl FlashblockEnqueuer {
    /// Wraps a command-queue sender as a flashblock ingress handle.
    pub const fn new(tx: UnboundedSender<ProcessorCommand>) -> Self {
        Self { tx }
    }
}

impl FlashblocksReceiver for FlashblockEnqueuer {
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        let block = flashblock.metadata.block_number;
        let index = flashblock.index;
        let command = ProcessorCommand::Flashblock {
            flashblock: Box::new(flashblock),
            squash: false,
            is_final_expected_hash: None,
        };
        if self.tx.send(command).is_err() {
            warn!(block, index, "firehose flashblocks command queue closed; dropping flashblock");
        }
    }

    fn on_flashblock_received_with_peek(&self, flashblock: Flashblock, peek: Option<&Flashblock>) {
        let block = flashblock.metadata.block_number;
        let index = flashblock.index;
        let (squash, is_final_expected_hash) = FlashblockPeekClassifier::classify(&flashblock, peek);
        let command = ProcessorCommand::Flashblock {
            flashblock: Box::new(flashblock),
            squash,
            is_final_expected_hash,
        };
        if self.tx.send(command).is_err() {
            warn!(block, index, "firehose flashblocks command queue closed; dropping flashblock");
        }
    }
}

/// Ingress handle for canonical-block signals. Cloned once per signal source (the early
/// in-engine notification and the post-commit canonical-state broadcast both hold a clone).
#[derive(Debug, Clone)]
pub struct CanonicalSender {
    tx: UnboundedSender<ProcessorCommand>,
}

impl CanonicalSender {
    /// Wraps a command-queue sender as a canonical-signal ingress handle.
    pub const fn new(tx: UnboundedSender<ProcessorCommand>) -> Self {
        Self { tx }
    }

    /// Enqueues a canonical-block notification for the consumer task to apply in arrival order.
    pub fn send(&self, number: u64, hash: B256) {
        if self.tx.send(ProcessorCommand::CanonicalBlock { number, hash }).is_err() {
            warn!(block = number, "firehose flashblocks command queue closed; dropping canonical signal");
        }
    }
}

/// Drains [`ProcessorCommand`]s from the queue and applies each to the processor to completion,
/// one at a time — the only place that calls the processor's mutating methods in production.
#[derive(Debug)]
pub struct FirehoseFlashblocksDispatcher<Client> {
    processor: Arc<FirehoseFlashblocksProcessor<Client>>,
}

impl<Client> FirehoseFlashblocksDispatcher<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Creates a dispatcher bound to `processor`.
    pub const fn new(processor: Arc<FirehoseFlashblocksProcessor<Client>>) -> Self {
        Self { processor }
    }

    /// Consumes commands from `rx` until every sender has dropped, applying each to completion
    /// before the next is dequeued.
    ///
    /// Each command's synchronous handler runs on the blocking pool via
    /// [`tokio::task::spawn_blocking`], and the consumer `await`s it before pulling the next
    /// command. This keeps the (~100 ms) state-root trie traversal off the runtime's worker
    /// threads — so it never starves other tasks — while still applying commands strictly in
    /// arrival order. `spawn_blocking` closures run within the runtime context, so the
    /// processor's per-block speculative state-root precompute (which spawns through
    /// [`tokio::runtime::Handle::try_current`]) keeps working.
    pub async fn run(self, mut rx: UnboundedReceiver<ProcessorCommand>) {
        while let Some(command) = rx.recv().await {
            let processor = Arc::clone(&self.processor);
            let outcome = tokio::task::spawn_blocking(move || match command {
                ProcessorCommand::Flashblock { flashblock, squash, is_final_expected_hash } => {
                    processor.process(*flashblock, squash, is_final_expected_hash);
                }
                ProcessorCommand::CanonicalBlock { number, hash } => {
                    processor.on_canonical_block(number, hash);
                }
            })
            .await;
            if let Err(err) = outcome {
                error!(error = %err, "firehose flashblocks command handler panicked; continuing");
            }
        }
        debug!("firehose flashblocks command queue closed; dispatcher consumer exiting");
    }
}
