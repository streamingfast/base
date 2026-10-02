//! Firehose tracing of blocks the payload builder assembles from payload attributes alone.
//!
//! The consensus node derives blocks from L1 by asking the execution node to build each one from
//! its payload attributes, with the transaction pool disabled. Such a block enters the chain
//! already executed and skips the engine validation that traces every other live block, so the
//! payload builder traces it through [`BuiltBlock::trace`].

use alloy_consensus::BlockHeader;
use base_common_consensus::BasePrimitives;
use base_execution_evm::BaseEvmConfig;
use reth_evm::{ConfigureEvm, Database, execute::BlockExecutionError};
use reth_firehose::{ChainHooks, FinalizedBlockRef, FirehoseBlockTracer};
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use reth_revm::{State, database::StateProviderDatabase};
use reth_storage_api::{HeaderProvider, StateProviderFactory};

use crate::{OpChainHooks, OpFirehoseEvmConfig};

/// EVM configuration hook that emits an already built block to Firehose.
pub trait BuiltBlockTracer<N: NodePrimitives> {
    /// Re-executes `block` on `state`, a fresh state over the block's parent, and emits it with
    /// `finalized` as its finalized block. Does nothing by default.
    fn trace_built_block<DB: Database>(
        &self,
        _state: &mut State<DB>,
        _block: &RecoveredBlock<N::Block>,
        _finalized: Option<FinalizedBlockRef>,
    ) -> Result<(), BlockExecutionError> {
        Ok(())
    }
}

impl<ChainSpec, N: NodePrimitives, R, EvmFactory> BuiltBlockTracer<N>
    for BaseEvmConfig<ChainSpec, N, R, EvmFactory>
{
}

impl<F> BuiltBlockTracer<BasePrimitives> for OpFirehoseEvmConfig<F>
where
    F: ConfigureEvm<Primitives = BasePrimitives>,
    OpChainHooks: ChainHooks<F>,
{
    fn trace_built_block<DB: Database>(
        &self,
        state: &mut State<DB>,
        block: &RecoveredBlock<<BasePrimitives as NodePrimitives>::Block>,
        finalized: Option<FinalizedBlockRef>,
    ) -> Result<(), BlockExecutionError> {
        if !reth_firehose::is_tracer_initialized() {
            return Ok(());
        }

        let mut tracer =
            FirehoseBlockTracer::start::<BasePrimitives>(block.sealed_block(), finalized);
        match OpChainHooks.execute_one_traced(&self.inner, state, block, &mut tracer) {
            Ok(_) => {
                tracer.mark_verified();
                Ok(())
            }
            Err(error) => {
                tracer.mark_failed(&error);
                Err(error)
            }
        }
    }
}

/// A block the payload builder assembled from payload attributes alone.
#[derive(Debug, Clone, Copy)]
pub struct BuiltBlock;

impl BuiltBlock {
    /// Emits `block` to Firehose by re-executing it on its parent state, read from `client`.
    ///
    /// The block advertises the node's finalized head, or the highest canonical ancestor below
    /// it. Does nothing when the Firehose tracer is not initialized.
    pub fn trace<Evm, N, Client>(
        evm_config: &Evm,
        client: &Client,
        block: &RecoveredBlock<N::Block>,
    ) -> Result<(), BlockExecutionError>
    where
        N: NodePrimitives,
        Evm: BuiltBlockTracer<N>,
        Client: StateProviderFactory + HeaderProvider,
    {
        if !reth_firehose::is_tracer_initialized() {
            return Ok(());
        }

        let parent_hash = block.header().parent_hash();
        let state_provider =
            client.state_by_block_hash(parent_hash).map_err(BlockExecutionError::other)?;
        let mut state = State::builder()
            .with_database(StateProviderDatabase::new(&state_provider))
            .with_bundle_update()
            .build();

        let finalized = reth_firehose::finalized_ref_for_block(
            block.header().number(),
            parent_hash,
            client.finalized_block_num_hash().ok().flatten(),
            |hash| {
                client
                    .header(hash)
                    .ok()
                    .flatten()
                    .map(|header| (header.number(), header.parent_hash()))
            },
            |number| client.block_hash(number).ok().flatten(),
        );

        evm_config.trace_built_block(&mut state, block, finalized)
    }
}
