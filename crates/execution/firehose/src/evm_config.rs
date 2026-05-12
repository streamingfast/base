//! OP Stack [`ConfigureEvm`] wrapper that routes the staged-sync batch executor through
//! [`reth_firehose::FirehoseBlockExecutor`] with OP chain hooks installed.
//!
//! The live engine-API path installs [`crate::OpPreTxAdjust`] / [`crate::OpPostTxExtras`]
//! explicitly at the validator call site, so it does not rely on this wrapper.
//!
//! ## Why this wrapper exists
//!
//! `reth_firehose::FirehoseEvmConfig` hardcodes [`reth_firehose::NoChainHooks`] in its
//! `batch_executor` impl. Chains with chain-specific fee distribution (OP Stack, ...) need to
//! install their own [`reth_firehose::ChainHooks`] impl. See the author's note at
//! `reth/crates/firehose/src/executor.rs` for why the hook type must be encapsulated in a
//! `ChainHooks` trait rather than threaded through `FirehoseEvmConfig<F, Extras, Adjust>`.

use alloy_consensus::{BlockHeader, Transaction, transaction::TxHashRef};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::Sealable;
use base_common_consensus::BasePrimitives;
use base_common_evm::{BaseEvmFactory, OpTransaction};
use reth_errors::BlockExecutionError;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutionCtxFor, TransactionEnv,
    execute::Executor,
};
use reth_firehose::{
    ChainHooks, FirehoseBlockExecutor, FirehoseBlockTracer, FirehoseWrappedExecutor,
    mapper::SignatureFields,
};
use reth_primitives_traits::{
    Block as BlockTrait, BlockBody, BlockTy, NodePrimitives, RecoveredBlock, TxTy,
};
use alloy_evm::block::BlockExecutionResult;
use reth_revm::revm::context::TxEnv;

use crate::{OpPostTxExtras, OpPreTxAdjust};

/// Chain-hook strategy that installs [`OpPostTxExtras`] and [`OpPreTxAdjust`] for OP Stack.
///
/// The per-block wrap logic is inlined in the trait method body so the
/// `PostTxExtras` / `PreTxAdjust` bound is discharged at a call site where `DB` and the
/// tracer-bound `'a` are method-local concrete types — the HRTB Rust would otherwise demand
/// for a generic `Extras: PostTxExtras<..Evm<&'a mut State<DB>, _>..>` at the
/// [`Executor`]-impl level is avoided entirely.
#[derive(Default, Debug, Clone, Copy)]
pub struct OpChainHooks;

impl<F> ChainHooks<F> for OpChainHooks
where
    F: ConfigureEvm<
            Primitives = BasePrimitives,
            BlockExecutorFactory: alloy_evm::block::BlockExecutorFactory<
                EvmFactory = BaseEvmFactory,
            >,
        > + Unpin
        + 'static,
    OpTransaction<TxEnv>: TransactionEnv,
    BlockTy<F::Primitives>: BlockTrait,
    <BlockTy<F::Primitives> as BlockTrait>::Header: BlockHeader + Sealable,
    <BlockTy<F::Primitives> as BlockTrait>::Body: BlockBody,
    <<BlockTy<F::Primitives> as BlockTrait>::Body as BlockBody>::OmmerHeader:
        BlockHeader + Sealable,
    TxTy<F::Primitives>: Transaction + TxHashRef + SignatureFields,
{
    fn execute_one_traced<DB: reth_evm::Database>(
        &self,
        evm_config: &F,
        db: &mut reth_revm::State<DB>,
        block: &RecoveredBlock<<F::Primitives as NodePrimitives>::Block>,
        tracer: &mut FirehoseBlockTracer,
    ) -> Result<
        BlockExecutionResult<<F::Primitives as NodePrimitives>::Receipt>,
        BlockExecutionError,
    > {
        let evm_env =
            evm_config.evm_env(block.header()).map_err(BlockExecutionError::other)?;
        let exec_ctx = evm_config
            .context_for_block(block.sealed_block())
            .map_err(BlockExecutionError::other)?;

        let inspector = tracer.inspector();
        let evm = evm_config.evm_with_env_and_inspector(db, evm_env, inspector);
        let inner = evm_config.create_executor(evm, exec_ctx);

        let withdrawals = block.body().withdrawals().cloned();
        let wrapped = FirehoseWrappedExecutor::with_hooks(
            inner,
            withdrawals,
            OpPreTxAdjust,
            OpPostTxExtras,
        );

        wrapped.execute_block(block.transactions_recovered())
    }
}

/// Thin wrapper around any [`ConfigureEvm`] that overrides [`ConfigureEvm::batch_executor`]
/// to construct a [`FirehoseBlockExecutor`] carrying [`OpChainHooks`].
///
/// All other methods delegate to the inner config unchanged, so the live engine-API path
/// (which builds its EVM via `evm_with_env_and_inspector` directly and installs the hooks
/// at the validator call site) remains unaffected.
#[derive(Clone, Debug)]
pub struct OpFirehoseEvmConfig<F> {
    /// The wrapped EVM configuration.
    pub inner: F,
}

impl<F> OpFirehoseEvmConfig<F> {
    /// Wraps an existing EVM configuration.
    pub const fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F> ConfigureEvm for OpFirehoseEvmConfig<F>
where
    F: ConfigureEvm<
            Primitives = BasePrimitives,
            BlockExecutorFactory: alloy_evm::block::BlockExecutorFactory<
                EvmFactory = BaseEvmFactory,
            >,
        > + Unpin
        + 'static,
    OpTransaction<TxEnv>: TransactionEnv,
    BlockTy<F::Primitives>: BlockTrait,
    <BlockTy<F::Primitives> as BlockTrait>::Header: BlockHeader + Sealable,
    <BlockTy<F::Primitives> as BlockTrait>::Body: BlockBody,
    <<BlockTy<F::Primitives> as BlockTrait>::Body as BlockBody>::OmmerHeader:
        BlockHeader + Sealable,
    TxTy<F::Primitives>: Transaction + TxHashRef + SignatureFields,
{
    type Primitives = F::Primitives;
    type Error = F::Error;
    type NextBlockEnvCtx = F::NextBlockEnvCtx;
    type BlockExecutorFactory = F::BlockExecutorFactory;
    type BlockAssembler = F::BlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self.inner.block_executor_factory()
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(
        &self,
        header: &reth_primitives_traits::HeaderTy<Self::Primitives>,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &reth_primitives_traits::HeaderTy<Self::Primitives>,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a reth_primitives_traits::SealedBlock<
            reth_primitives_traits::BlockTy<Self::Primitives>,
        >,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &reth_primitives_traits::SealedHeader<
            reth_primitives_traits::HeaderTy<Self::Primitives>,
        >,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }

    fn batch_executor<DB: reth_evm::Database>(
        &self,
        db: DB,
    ) -> impl Executor<DB, Primitives = Self::Primitives, Error = BlockExecutionError> {
        FirehoseBlockExecutor::new_with_chain_hooks(self.inner.clone(), db, OpChainHooks)
    }
}

impl<F, ExecutionData> ConfigureEngineEvm<ExecutionData> for OpFirehoseEvmConfig<F>
where
    F: ConfigureEvm<
            Primitives = BasePrimitives,
            BlockExecutorFactory: alloy_evm::block::BlockExecutorFactory<
                EvmFactory = BaseEvmFactory,
            >,
        > + ConfigureEngineEvm<ExecutionData>
        + Unpin
        + 'static,
    BlockTy<F::Primitives>: BlockTrait,
    <BlockTy<F::Primitives> as BlockTrait>::Header: BlockHeader + Sealable,
    <BlockTy<F::Primitives> as BlockTrait>::Body: BlockBody,
    <<BlockTy<F::Primitives> as BlockTrait>::Body as BlockBody>::OmmerHeader:
        BlockHeader + Sealable,
    TxTy<F::Primitives>: Transaction + TxHashRef + SignatureFields,
{
    fn evm_env_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl reth_evm::ExecutableTxIterator<Self>, Self::Error> {
        self.inner.tx_iterator_for_payload(payload)
    }
}
