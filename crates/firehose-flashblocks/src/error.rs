//! Error type for the firehose flashblocks subsystem.

use alloy_primitives::B256;
use thiserror::Error;

/// Errors raised while re-executing a flashblock through the Firehose tracer.
///
/// `process_flashblock` only ever observes these errors internally: it logs them and resets
/// the processor state (clears `current_block_number`, drops the accumulated DB) so that the
/// next base flashblock restarts tracking from scratch. The variant carries enough context
/// for the log line; nothing downstream consumes it.
#[derive(Debug, Error)]
pub enum Error {
    /// Could not fetch a [`reth_provider::StateProvider`] for the parent block within the
    /// retry window. Happens when canonical state hasn't caught up to flashblock pace and the
    /// processor has no carried-forward `accumulated_db` to fall back on.
    #[error(
        "state provider for parent of block {block_number} (parent {parent_hash:?}) not available after retries"
    )]
    StateProviderTimeout {
        /// Block number whose parent state we attempted to fetch.
        block_number: u64,
        /// Parent block hash we attempted to load.
        parent_hash: B256,
    },

    /// `BlockAssembler::assemble` returned an error — typically because the accumulated
    /// flashblocks miss the base payload or are empty. The inner error is preserved as a
    /// boxed dyn for diagnostic logging.
    #[error("failed to assemble partial block from flashblocks: {0}")]
    BlockAssembly(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// EVM execution failed during transaction processing or pre-execution changes. The inner
    /// error is preserved as a boxed dyn for diagnostic logging.
    #[error("EVM execution failed: {0}")]
    Execution(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A flashblock transaction failed RLP decoding or sender recovery. The inner message
    /// is captured as a string because the underlying decoder produces different concrete
    /// error types for the two failure modes.
    #[error("could not decode/recover transaction at index {tx_index}: {message}")]
    TransactionDecoding {
        /// Index of the offending transaction inside the flashblock delta.
        tx_index: usize,
        /// Human-readable description of the failure (RLP decode error or recovery failure).
        message: String,
    },

    /// EVM environment construction failed for the assembled partial block.
    #[error("could not construct EVM environment for block {block_number}: {source}")]
    EvmEnv {
        /// Block number whose env construction failed.
        block_number: u64,
        /// Underlying error (boxed because the inner type varies by `ConfigureEvm` impl).
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_provider_timeout_display_includes_block_and_parent() {
        let err = Error::StateProviderTimeout {
            block_number: 123,
            parent_hash: B256::repeat_byte(0xab),
        };
        let s = err.to_string();
        assert!(s.contains("123"), "missing block number in: {s}");
        assert!(s.contains("0xab"), "missing parent hash in: {s}");
    }

    #[test]
    fn transaction_decoding_display_includes_index_and_message() {
        let err = Error::TransactionDecoding {
            tx_index: 4,
            message: "boom".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains('4'), "missing tx_index in: {s}");
        assert!(s.contains("boom"), "missing message in: {s}");
    }

    #[test]
    fn evm_env_display_includes_block() {
        let err = Error::EvmEnv {
            block_number: 999,
            source: Box::new(std::io::Error::other("noise")),
        };
        let s = err.to_string();
        assert!(s.contains("999"), "missing block_number in: {s}");
        assert!(s.contains("noise"), "missing source in: {s}");
    }
}
