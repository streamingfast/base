//! Traits for the Flashblocks module.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, TxHash, U256};
use alloy_rpc_types_eth::{Filter, Log, state::StateOverride};
use arc_swap::Guard;
use base_common_flashblocks::Flashblock;
use base_common_network::Base;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use tokio::sync::broadcast;

use crate::PendingBlocks;

/// Trait for receiving flashblock updates.
pub trait FlashblocksReceiver {
    /// Called when a new flashblock is received.
    fn on_flashblock_received(&self, flashblock: Flashblock);

    /// Like [`Self::on_flashblock_received`] but with an optional peek at the next
    /// already-queued flashblock (i.e. the one the dispatch loop will deliver next
    /// if any is currently buffered, without blocking).
    ///
    /// Receivers can use the peek to implement look-ahead optimisations such as
    /// squashing intermediate same-block deltas (executing them only when the
    /// next delta of the same block is not already waiting) or driving is_final
    /// directly when the next-block base is already queued.
    ///
    /// The default implementation discards the peek and delegates to
    /// [`Self::on_flashblock_received`], so existing receivers continue to work
    /// unchanged.
    fn on_flashblock_received_with_peek(
        &self,
        flashblock: Flashblock,
        peek: Option<&Flashblock>,
    ) {
        let _ = peek;
        self.on_flashblock_received(flashblock);
    }
}

/// Core API for accessing flashblock state and data.
pub trait FlashblocksAPI {
    /// Retrieves the pending blocks.
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>>;

    /// Subscribes to flashblock updates.
    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>>;
}

/// API for accessing pending blocks data.
pub trait PendingBlocksAPI {
    /// Get the canonical block number on top of which all pending state is built
    fn get_canonical_block_number(&self) -> BlockNumberOrTag;

    /// Get the pending transactions count for an address
    fn get_transaction_count(&self, address: Address) -> U256;

    /// Retrieves the current block. If `full` is true, includes full transaction details.
    fn get_block(&self, full: bool) -> Option<RpcBlock<Base>>;

    /// Gets transaction receipt by hash.
    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Base>>;

    /// Gets transaction details by hash.
    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Base>>;

    /// Gets balance for an address. Returns None if address not updated in flashblocks.
    fn get_balance(&self, address: Address) -> Option<U256>;

    /// Gets the state overrides for the pending blocks
    fn get_state_overrides(&self) -> Option<StateOverride>;

    /// Gets logs from pending state matching the provided filter.
    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log>;
}
