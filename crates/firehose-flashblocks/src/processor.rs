//! Core Firehose flashblock processor: turns incoming [`Flashblock`] events into per-flashblock
//! `FIRE BLOCK` partial-block emissions on a dedicated tracer.

use std::{sync::Mutex, thread::sleep, time::Duration};

use alloy_consensus::{
    Header,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockNumberOrTag, eip2718::Decodable2718};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{B256, Bytes};
use base_common_chains::Upgrades;
use base_common_consensus::{BasePrimitives, BaseTxEnvelope};
use base_common_evm::{BaseBlockExecutionCtx, BaseBlockExecutor};
use base_common_flashblocks::Flashblock;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use base_execution_firehose::{OpPostTxExtras, OpPreTxAdjust};
use base_flashblocks::{
    AssembledBlock, BlockAssembler, FlashblockSequenceValidator, FlashblocksReceiver,
    SequenceValidationResult,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_firehose::{FirehoseBlockTracer, FirehoseWrappedExecutor};
use reth_primitives_traits::SealedBlock;
use reth_provider::{BlockReaderIdExt, StateProvider, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use tracing::{debug, error, info, warn};

use crate::{Error, FlashblocksTracerHandle};

/// Number of retries when waiting for a parent [`StateProvider`] to materialize.
///
/// The processor retries `state_by_block_number_or_tag(parent)` this many times with
/// [`STATE_PROVIDER_RETRY_DELAY`] between attempts before giving up and entering the
/// `is_skipping` state for the current block.
const STATE_PROVIDER_MAX_RETRIES: u32 = 20;

/// Sleep duration between [`StateProviderFactory`] retries on the bootstrap path.
const STATE_PROVIDER_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Boxed dynamically-dispatched [`StateProvider`] (matches `reth_provider::StateProviderBox`).
type BoxedStateProvider = Box<dyn StateProvider + Send + 'static>;

/// Accumulated EVM state DB, carried across flashblocks within a block and across blocks on
/// the sequential fast path. The inner `Box<dyn StateProvider>` is the canonical-chain
/// snapshot fetched once on bootstrap; subsequent reads are served from the `State`'s bundle
/// cache (which holds the committed effects of all prior flashblocks).
type AccumulatedDb = State<StateProviderDatabase<BoxedStateProvider>>;

/// Mutable state held by the processor, guarded by a single mutex so flashblock callbacks
/// from the WS subscriber are processed in arrival order.
#[derive(Debug)]
struct ProcessorState {
    /// Current block number we are tracing. `None` until the first base flashblock.
    current_block_number: Option<u64>,
    /// Latest flashblock index applied for `current_block_number`. `None` while no base is seen.
    latest_flashblock_index: Option<u64>,
    /// All flashblocks accumulated for the current block. Cleared on a new base.
    stored_flashblocks: Vec<Flashblock>,
    /// EVM state shared across flashblocks (and across blocks on the sequential fast path).
    accumulated_db: Option<AccumulatedDb>,
    /// `true` between an error and the next base flashblock — drops further deltas for the block.
    is_skipping: bool,
}

impl ProcessorState {
    const fn new() -> Self {
        Self {
            current_block_number: None,
            latest_flashblock_index: None,
            stored_flashblocks: Vec::new(),
            accumulated_db: None,
            is_skipping: false,
        }
    }
}

/// Re-executes flashblocks through a dedicated [`firehose_tracer::Tracer`] and emits one
/// partial-block FIRE event per flashblock.
///
/// Implements [`FlashblocksReceiver`] so it can be plugged into the existing
/// [`base_flashblocks::FlashblocksSubscriber`]. Construction order matters: build this AFTER
/// [`reth_firehose::init_tracer`] has been called — [`FlashblocksTracerHandle`] needs the
/// shared stdout lock that `init_tracer` installs.
#[derive(Debug)]
pub struct FirehoseFlashblocksProcessor<Client> {
    client: Client,
    state: Mutex<ProcessorState>,
    tracer: Mutex<FlashblocksTracerHandle>,
}

impl<Client> FirehoseFlashblocksProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Creates a new processor backed by the supplied client (provider) and dedicated tracer.
    pub const fn new(client: Client, tracer: FlashblocksTracerHandle) -> Self {
        Self { client, state: Mutex::new(ProcessorState::new()), tracer: Mutex::new(tracer) }
    }

    /// Process a single flashblock event. Errors are logged and swallowed: the processor sets
    /// the `is_skipping` flag for the rest of the current block and drops the accumulated DB,
    /// then waits for the next base flashblock.
    fn process(&self, flashblock: Flashblock) {
        if let Err(err) = self.process_inner(flashblock) {
            error!(error = %err, "flashblock processing failed; skipping rest of current block");
            let mut state = self.state.lock().expect("flashblock state mutex poisoned");
            state.is_skipping = true;
            state.accumulated_db = None;
        }
    }

    fn process_inner(&self, flashblock: Flashblock) -> Result<(), Error> {
        let block_number = flashblock.metadata.block_number;
        let index = flashblock.index;

        let mut state = self.state.lock().expect("flashblock state mutex poisoned");

        // Validate sequence vs current state.
        if let Some(latest_block) = state.current_block_number {
            let latest_idx = state
                .latest_flashblock_index
                .expect("latest_flashblock_index must be Some when current_block_number is Some");
            match FlashblockSequenceValidator::validate(
                latest_block,
                latest_idx,
                block_number,
                index,
            ) {
                SequenceValidationResult::NextInSequence
                | SequenceValidationResult::FirstOfNextBlock => {}
                SequenceValidationResult::Duplicate => {
                    debug!(block = block_number, index, "duplicate flashblock; ignoring");
                    return Ok(());
                }
                SequenceValidationResult::InvalidNewBlockIndex { .. } => {
                    warn!(
                        block = block_number,
                        index,
                        latest_block,
                        latest_idx,
                        "new block with non-zero index; dropping accumulated DB and skipping"
                    );
                    state.is_skipping = true;
                    state.accumulated_db = None;
                    return Ok(());
                }
                SequenceValidationResult::NonSequentialGap { expected, actual } => {
                    warn!(
                        block = block_number,
                        expected,
                        actual,
                        "non-sequential flashblock gap; dropping accumulated DB and skipping"
                    );
                    state.is_skipping = true;
                    state.accumulated_db = None;
                    return Ok(());
                }
            }
        } else if index != 0 {
            warn!(
                block = block_number,
                index,
                "first observed flashblock is not a base (index != 0); waiting for next base"
            );
            state.is_skipping = true;
            return Ok(());
        }

        if state.is_skipping {
            if index == 0 {
                state.is_skipping = false;
            } else {
                debug!(
                    block = block_number,
                    index, "skipping flashblock while in error-recovery state"
                );
                return Ok(());
            }
        }

        // On a new base, drop accumulated_db iff the new block is not the strict successor of
        // the previous one (so a fresh bootstrap from the canonical provider runs below).
        if index == 0 {
            let is_sequential =
                state.current_block_number.is_some_and(|prev| prev + 1 == block_number);
            if !is_sequential {
                state.accumulated_db = None;
            }
            state.stored_flashblocks = vec![flashblock];
            state.latest_flashblock_index = Some(0);
            state.current_block_number = Some(block_number);
        } else {
            state.stored_flashblocks.push(flashblock);
            state.latest_flashblock_index = Some(index);
        }

        let stored_flashblocks = state.stored_flashblocks.clone();
        let mut accumulated_db = state.accumulated_db.take();
        drop(state);

        let assembled = BlockAssembler::assemble(&stored_flashblocks)
            .map_err(|e| Error::BlockAssembly(Box::new(e)))?;

        let new_transactions: Vec<Bytes> = if index == 0 {
            assembled.flashblocks[0].diff.transactions.clone()
        } else {
            stored_flashblocks
                .last()
                .expect("stored_flashblocks contains at least the new delta")
                .diff
                .transactions
                .clone()
        };

        let parent_hash = assembled.base.parent_hash;

        if accumulated_db.is_none() {
            let parent_block = block_number.saturating_sub(1);
            let provider = match self.bootstrap_provider(parent_block) {
                Some(p) => p,
                None => {
                    let mut state = self.state.lock().expect("flashblock state mutex poisoned");
                    state.is_skipping = true;
                    return Err(Error::StateProviderTimeout { block_number, parent_hash });
                }
            };
            accumulated_db = Some(
                State::builder()
                    .with_database(StateProviderDatabase::new(provider))
                    .with_bundle_update()
                    .without_state_clear()
                    .build(),
            );
        }

        let mut db = accumulated_db.expect("accumulated_db was populated just above");
        let exec_result = self.execute_flashblock(&assembled, index, &new_transactions, &mut db);

        let mut state_guard = self.state.lock().expect("flashblock state mutex poisoned");
        match &exec_result {
            Ok(()) => {
                state_guard.accumulated_db = Some(db);
            }
            Err(_) => {
                state_guard.is_skipping = true;
                state_guard.accumulated_db = None;
            }
        }
        exec_result
    }

    fn bootstrap_provider(&self, parent_block: u64) -> Option<BoxedStateProvider> {
        for attempt in 0..STATE_PROVIDER_MAX_RETRIES {
            match self.client.state_by_block_number_or_tag(BlockNumberOrTag::Number(parent_block)) {
                Ok(provider) => return Some(provider),
                Err(err) => {
                    debug!(
                        attempt,
                        parent_block,
                        error = %err,
                        "state provider not yet available; retrying"
                    );
                    sleep(STATE_PROVIDER_RETRY_DELAY);
                }
            }
        }
        None
    }

    fn execute_flashblock(
        &self,
        assembled: &AssembledBlock,
        index: u64,
        new_transactions: &[Bytes],
        accumulated_db: &mut AccumulatedDb,
    ) -> Result<(), Error> {
        let block_number = assembled.base.block_number;
        let parent_hash = assembled.base.parent_hash;

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let receipt_builder = *evm_config.block_executor_factory().receipt_builder();

        let block_env_attributes = BaseNextBlockEnvAttributes {
            timestamp: assembled.base.timestamp,
            suggested_fee_recipient: assembled.base.fee_recipient,
            prev_randao: assembled.base.prev_randao,
            gas_limit: assembled.base.gas_limit,
            parent_beacon_block_root: Some(assembled.base.parent_beacon_block_root),
            extra_data: assembled.base.extra_data.clone(),
        };

        let parent_header = self
            .client
            .header_by_number(block_number.saturating_sub(1))
            .map_err(|e| Error::EvmEnv {
                block_number,
                source: Box::new(std::io::Error::other(e.to_string())),
            })?
            .ok_or_else(|| Error::EvmEnv {
                block_number,
                source: Box::new(std::io::Error::other("parent header missing")),
            })?;

        let evm_env =
            evm_config.next_evm_env(&parent_header, &block_env_attributes).map_err(|e| {
                Error::EvmEnv {
                    block_number,
                    source: Box::new(std::io::Error::other(e.to_string())),
                }
            })?;

        let txs_with_senders = self.decode_and_recover_transactions(new_transactions)?;

        let sealed_block: SealedBlock<base_common_consensus::BaseBlock> =
            SealedBlock::new_unchecked(assembled.block.clone(), B256::ZERO);

        let mut tracer = self.tracer.lock().expect("flashblock tracer mutex poisoned");
        let is_final = false;
        let mut block_tracer = FirehoseBlockTracer::start_flashblock_local::<BasePrimitives>(
            tracer.tracer_mut(),
            &sealed_block,
            None,
            index,
            is_final,
        );

        let ctx = BaseBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: Some(assembled.base.parent_beacon_block_root),
            extra_data: assembled.base.extra_data.clone(),
        };
        let inspector = block_tracer.inspector();
        let evm = evm_config.evm_with_env_and_inspector(accumulated_db, evm_env, inspector);
        let inner = BaseBlockExecutor::new(evm, ctx, self.client.chain_spec(), receipt_builder);

        let withdrawals = assembled
            .block
            .body
            .withdrawals
            .clone()
            .map(|ws| alloy_eips::eip4895::Withdrawals::new(ws.0));
        let mut executor =
            FirehoseWrappedExecutor::with_hooks(inner, withdrawals, OpPreTxAdjust, OpPostTxExtras);

        if index == 0 {
            executor.apply_pre_execution_changes().map_err(|e| Error::Execution(Box::new(e)))?;
        }

        let new_tx_count = new_transactions.len();
        for (tx_idx, recovered) in txs_with_senders.into_iter().enumerate() {
            if let Err(err) = executor.execute_transaction(recovered) {
                error!(
                    block = block_number,
                    index,
                    tx_idx,
                    error = %err,
                    "flashblock tx execution failed"
                );
                block_tracer.mark_failed(&err);
                return Err(Error::Execution(Box::new(err)));
            }
        }

        executor.finish().map_err(|e| Error::Execution(Box::new(e)))?;

        block_tracer.mark_flashblock();
        drop(tracer);

        info!(
            block = block_number,
            index,
            tx_count = new_tx_count,
            "emitted flashblock partial FIRE event"
        );

        Ok(())
    }

    fn decode_and_recover_transactions(
        &self,
        transactions: &[Bytes],
    ) -> Result<Vec<Recovered<BaseTxEnvelope>>, Error> {
        transactions
            .iter()
            .enumerate()
            .map(|(tx_index, bytes)| {
                let tx = BaseTxEnvelope::decode_2718(&mut bytes.as_ref()).map_err(|err| {
                    Error::TransactionDecoding {
                        tx_index,
                        message: format!("RLP decode failed: {err}"),
                    }
                })?;
                let signer = tx.recover_signer().map_err(|err| Error::TransactionDecoding {
                    tx_index,
                    message: format!("sender recovery failed: {err}"),
                })?;
                Ok(Recovered::new_unchecked(tx, signer))
            })
            .collect()
    }
}

impl<Client> FlashblocksReceiver for FirehoseFlashblocksProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        self.process(flashblock);
    }
}
