//! Core Firehose flashblock processor: turns incoming [`Flashblock`] events into per-flashblock
//! `FIRE BLOCK` partial-block emissions on a dedicated tracer.

use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

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

/// Maximum age (in seconds) of an incoming flashblock relative to the processor's clock
/// before it is considered stale. Stale flashblocks are dropped without affecting state —
/// they don't trigger a reset, since later flashblocks for the same block would be
/// equally stale and the next live base will simply restart the sequence.
///
/// Flashblocks arrive every ~200 ms in normal operation, so 5 seconds (≈25 flashblocks) of
/// lag is plenty of slack for transient network or processing jitter — anything older
/// than that is almost certainly a stale message we should drop rather than execute.
const STALE_THRESHOLD_SECS: u64 = 5;

/// Function that returns the current Unix timestamp in seconds, used for the staleness
/// check. Boxed behind an `Arc` so it can be cheaply cloned and shared across threads.
pub type ClockFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Returns the current Unix timestamp in seconds. Used as the default [`ClockFn`].
fn system_clock() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Boxed dynamically-dispatched [`StateProvider`] (matches `reth_provider::StateProviderBox`).
type BoxedStateProvider = Box<dyn StateProvider + Send + 'static>;

/// Accumulated EVM state DB, carried across flashblocks within a block and across blocks on
/// the sequential fast path. The inner `Box<dyn StateProvider>` is the canonical-chain
/// snapshot fetched once on bootstrap; subsequent reads are served from the `State`'s bundle
/// cache (which holds the committed effects of all prior flashblocks).
type AccumulatedDb = State<StateProviderDatabase<BoxedStateProvider>>;

/// Mutable state held by the processor, guarded by a single mutex so flashblock callbacks
/// from the WS subscriber are processed in arrival order.
///
/// `current_block_number == None` doubles as the "no valid sequence in progress" signal:
/// errors clear it, and the next incoming flashblock must be a base (index 0) to restart.
#[derive(Debug)]
struct ProcessorState {
    /// Current block number we are tracing. `None` until the first base flashblock and after
    /// any error or out-of-sequence event that forces us to wait for the next base.
    current_block_number: Option<u64>,
    /// Latest flashblock index applied for `current_block_number`. `None` whenever
    /// `current_block_number` is `None`.
    latest_flashblock_index: Option<u64>,
    /// All flashblocks accumulated for the current block. Cleared whenever state is reset.
    stored_flashblocks: Vec<Flashblock>,
    /// EVM state shared across flashblocks (and across blocks on the sequential fast path).
    accumulated_db: Option<AccumulatedDb>,
    /// True when we have accumulated flashblocks for `current_block_number` but cannot yet
    /// execute them because the parent block's state is not available from the provider.
    ///
    /// While pending, incoming flashblocks for the same block are still accepted by the
    /// sequence validator and pushed onto `stored_flashblocks`, but no FIRE BLOCK lines are
    /// emitted. A subsequent call to [`FirehoseFlashblocksProcessor::on_canonical_block`]
    /// for the parent block triggers replay: every stored flashblock is executed in order,
    /// emitting one FIRE BLOCK line each, and the pending flag is cleared.
    pending_state: bool,
    /// True when the flashblocks currently stored for `current_block_number` were produced
    /// via a replay (i.e., previously pending and then unblocked by a canonical-block
    /// notification on the parent). The flashblock chain's tip for this block has therefore
    /// not yet been confirmed against the canonical chain; until a canonical-block
    /// notification arrives for `current_block_number` to confirm (or contradict) that tip,
    /// the processor must defer the next-block transition rather than continuing on the
    /// optimistic sequential fast path — otherwise a divergent fork would silently be
    /// extended through subsequent blocks.
    awaiting_canonical_confirmation: bool,
    /// Most recent canonical-block notification (block number + hash) seen by the
    /// processor. Used to validate a new-block base's `parent_hash` against the
    /// canonical chain at the moment the base is observed — when the canonical hash for
    /// block N-1 is known and disagrees with the incoming base N's parent_hash, the
    /// flashblock chain has diverged and the base (plus any subsequent deltas) is
    /// discarded rather than emitted.
    latest_canonical: Option<(u64, B256)>,
}

/// Carries an `is_final` FIRE BLOCK ready to emit on the flashblock tracer once the
/// caller drops the [`ProcessorState`] mutex. Constructed inside the state lock
/// during a `FirstOfNextBlock` transition when the recomputed block hash matches
/// the new base's `parent_hash`; consumed by the caller right after dropping state.
#[derive(Debug)]
struct PendingFinalEmission {
    /// Sealed block to feed [`FirehoseBlockTracer::start_flashblock_local`].
    sealed_block: SealedBlock<base_common_consensus::BaseBlock>,
    /// Final delta index — stamped on the FIRE BLOCK with the `+1000` sentinel.
    final_index: u64,
}

impl ProcessorState {
    const fn new() -> Self {
        Self {
            current_block_number: None,
            latest_flashblock_index: None,
            stored_flashblocks: Vec::new(),
            accumulated_db: None,
            pending_state: false,
            awaiting_canonical_confirmation: false,
            latest_canonical: None,
        }
    }

    /// Clear all per-sequence state, forcing the next flashblock to start a fresh sequence
    /// (only a base flashblock at index 0 will be accepted; non-zero indices are dropped).
    /// Leaves `latest_canonical` untouched: knowledge of the canonical chain survives
    /// processor resets so that subsequent flashblocks are still validated against it.
    fn reset(&mut self) {
        self.current_block_number = None;
        self.latest_flashblock_index = None;
        self.stored_flashblocks.clear();
        self.accumulated_db = None;
        self.pending_state = false;
        self.awaiting_canonical_confirmation = false;
    }

    /// Begin (or restart) a sequence on a fresh base flashblock at index 0. The accumulated
    /// DB is left untouched — the caller decides whether to keep the carried-forward state
    /// (sequential fast path) or to drop it (block gap / startup) before calling this.
    fn start_block(&mut self, flashblock: Flashblock) {
        debug_assert_eq!(flashblock.index, 0, "start_block requires a base flashblock");
        self.current_block_number = Some(flashblock.metadata.block_number);
        self.latest_flashblock_index = Some(0);
        self.stored_flashblocks = vec![flashblock];
        // Starting a fresh sequence invalidates any prior pending accumulation.
        self.pending_state = false;
        self.awaiting_canonical_confirmation = false;
    }
}

/// Re-executes flashblocks through a dedicated [`firehose_tracer::Tracer`] and emits one
/// partial-block FIRE event per flashblock.
///
/// Implements [`FlashblocksReceiver`] so it can be plugged into the existing
/// [`base_flashblocks::FlashblocksSubscriber`]. Construction order matters: build this AFTER
/// [`reth_firehose::init_tracer`] has been called — [`FlashblocksTracerHandle`] needs the
/// shared stdout lock that `init_tracer` installs.
pub struct FirehoseFlashblocksProcessor<Client> {
    client: Client,
    state: Mutex<ProcessorState>,
    tracer: Mutex<FlashblocksTracerHandle>,
    /// Source of "now" for the staleness check. Production wraps [`SystemTime::now`];
    /// tests inject a constant so timestamps in fixtures are deterministic relative to
    /// the configured clock.
    clock: ClockFn,
}

impl<Client: std::fmt::Debug> std::fmt::Debug for FirehoseFlashblocksProcessor<Client> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirehoseFlashblocksProcessor")
            .field("client", &self.client)
            .field("state", &self.state)
            .field("tracer", &self.tracer)
            .finish_non_exhaustive()
    }
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
    /// Creates a new processor backed by the supplied client (provider) and dedicated
    /// tracer. The staleness clock defaults to wall-clock Unix time; tests that need
    /// deterministic time-of-day should use [`Self::with_clock`] instead.
    pub fn new(client: Client, tracer: FlashblocksTracerHandle) -> Self {
        Self::with_clock(client, tracer, Arc::new(system_clock))
    }

    /// Like [`Self::new`] but accepts a custom clock used to evaluate flashblock
    /// staleness. The closure returns the current Unix timestamp in seconds.
    pub fn with_clock(client: Client, tracer: FlashblocksTracerHandle, clock: ClockFn) -> Self {
        Self { client, state: Mutex::new(ProcessorState::new()), tracer: Mutex::new(tracer), clock }
    }

    /// Process a single flashblock event. Errors are logged and swallowed: the processor clears
    /// its in-flight state and accumulated DB so the next base flashblock restarts tracking.
    fn process(&self, flashblock: Flashblock) {
        if let Err(err) = self.process_inner(flashblock) {
            error!(error = %err, "flashblock processing failed; resetting state and waiting for next base");
            let mut state = self.state.lock().expect("flashblock state mutex poisoned");
            state.reset();
        }
    }

    /// Returns `true` if a new-block base's `parent_hash` is consistent with the most
    /// recently observed canonical-block notification (or if there is no canonical
    /// reference point to validate against yet). Returns `false` when the canonical
    /// chain has confirmed a hash for the parent block that disagrees with the base —
    /// the flashblock sequence has diverged and should be discarded.
    ///
    /// Validation is only attempted when the latest known canonical block number is the
    /// immediate parent of the incoming base. Older canonical info isn't used to validate
    /// further-future bases, since intermediate blocks may have arrived as canonicals we
    /// haven't yet remembered.
    fn parent_matches_canonical(
        latest_canonical: Option<(u64, B256)>,
        base_block_number: u64,
        base_parent_hash: B256,
    ) -> bool {
        match latest_canonical {
            Some((n, hash)) if n == base_block_number.saturating_sub(1) => hash == base_parent_hash,
            _ => true,
        }
    }

    fn process_inner(&self, flashblock: Flashblock) -> Result<(), Error> {
        let block_number = flashblock.metadata.block_number;
        let index = flashblock.index;

        // Carries the `is_final` FIRE BLOCK for the just-finished block when the new
        // base's `parent_hash` matched the locally-recomputed hash. Emitted right
        // after the state lock is released, before the new block's execution starts.
        let mut pending_final_emission: Option<PendingFinalEmission> = None;

        let mut state = self.state.lock().expect("flashblock state mutex poisoned");

        // Staleness check: if the incoming flashblock describes a block whose timestamp
        // is more than `STALE_THRESHOLD_SECS` behind the current clock, discard it
        // outright. Base flashblocks (index 0) carry the timestamp directly; deltas
        // inherit it from the in-flight base. We deliberately do not mutate `state`
        // here — a stale base will simply not start a sequence, and a stale delta lands
        // before the validator updates `latest_flashblock_index`, so subsequent flashblocks
        // for the same block see no progression and are dropped via the normal paths.
        let block_timestamp = if index == 0 {
            flashblock.base.as_ref().map(|b| b.timestamp)
        } else {
            state.stored_flashblocks.first().and_then(|fb| fb.base.as_ref().map(|b| b.timestamp))
        };
        if let Some(ts) = block_timestamp {
            let now = (self.clock)();
            let age = now.saturating_sub(ts);
            if age > STALE_THRESHOLD_SECS {
                warn!(
                    block = block_number,
                    index,
                    flashblock_timestamp = ts,
                    now_secs = now,
                    age_secs = age,
                    threshold_secs = STALE_THRESHOLD_SECS,
                    "flashblock too far in the past; skipping execution"
                );
                return Ok(());
            }
        }

        // Parent-hash sanity check: any base (index 0) must descend from a parent block
        // whose hash the processor has already accepted. Two cases:
        //
        // 1. If a canonical-block notification for the parent block (block_number - 1) has
        //    already been observed, that hash is authoritative — discard on mismatch.
        // 2. Otherwise, if an in-flight flashblock chain exists for the parent block (the
        //    current in-flight `current_block_number == block_number - 1`), the parent
        //    must match the latest emitted tip of that chain. Without this, a base for
        //    block N+1 could silently extend a fork whose tip the processor has no way to
        //    obtain canonical state for.
        //
        // Non-base flashblocks (deltas) inherit the verdict via the in-flight block — they
        // target an already-validated sequence.
        if index == 0 {
            let incoming_parent_hash =
                flashblock.base.as_ref().map(|b| b.parent_hash).unwrap_or_default();
            if !Self::parent_matches_canonical(
                state.latest_canonical,
                block_number,
                incoming_parent_hash,
            ) {
                let (canon_num, canon_hash) =
                    state.latest_canonical.expect("mismatch implies latest_canonical is Some");
                warn!(
                    block = block_number,
                    incoming_parent_hash = %incoming_parent_hash,
                    canonical_block = canon_num,
                    canonical_hash = %canon_hash,
                    "base flashblock parent_hash disagrees with latest canonical; discarding and resetting state"
                );
                state.reset();
                return Ok(());
            }
            let canonical_known_for_parent = matches!(
                state.latest_canonical,
                Some((n, _)) if n == block_number.saturating_sub(1)
            );
            if !canonical_known_for_parent
                && state.current_block_number == Some(block_number.saturating_sub(1))
            {
                let tip_hash = state
                    .stored_flashblocks
                    .last()
                    .map(|fb| fb.diff.block_hash)
                    .unwrap_or_default();
                if tip_hash != incoming_parent_hash {
                    warn!(
                        block = block_number,
                        incoming_parent_hash = %incoming_parent_hash,
                        in_flight_tip_hash = %tip_hash,
                        "base flashblock parent_hash disagrees with in-flight chain tip and no canonical confirms the parent; discarding and resetting state"
                    );
                    state.reset();
                    return Ok(());
                }
            }
        }

        match state.current_block_number {
            // No prior state: only a base flashblock can start (or restart) a sequence.
            None => {
                if index != 0 {
                    warn!(
                        block = block_number,
                        index,
                        "no in-flight sequence and incoming flashblock is not a base (index != 0); dropping and waiting for next base"
                    );
                    return Ok(());
                }
                state.start_block(flashblock);
            }
            Some(latest_block) => {
                let latest_idx = state.latest_flashblock_index.expect(
                    "latest_flashblock_index must be Some when current_block_number is Some",
                );
                match FlashblockSequenceValidator::validate(
                    latest_block,
                    latest_idx,
                    block_number,
                    index,
                ) {
                    SequenceValidationResult::NextInSequence => {
                        // A delta must belong to the same payload as the in-flight base —
                        // its `payload_id` should equal the base flashblock's. A mismatch
                        // means this delta was produced for a different payload (e.g. the
                        // sequencer started a fresh build) and cannot be applied on top of
                        // the current accumulation; treat it as a non-consecutive event,
                        // reset, and wait for the next base flashblock.
                        let base_payload_id = state
                            .stored_flashblocks
                            .first()
                            .expect(
                                "stored_flashblocks contains the base when current_block_number is Some",
                            )
                            .payload_id;
                        if flashblock.payload_id != base_payload_id {
                            warn!(
                                block = block_number,
                                index,
                                base_payload_id = %base_payload_id,
                                delta_payload_id = %flashblock.payload_id,
                                "delta flashblock payload_id disagrees with in-flight base; resetting state and waiting for next base"
                            );
                            state.reset();
                            return Ok(());
                        }
                        state.stored_flashblocks.push(flashblock);
                        state.latest_flashblock_index = Some(index);
                    }
                    SequenceValidationResult::FirstOfNextBlock => {
                        // Strict successor block — keep accumulated_db so the sequential fast
                        // path can carry committed state forward without re-bootstrapping.
                        let awaiting = state.awaiting_canonical_confirmation;
                        let stored_parent_hash =
                            flashblock.base.as_ref().map(|b| b.parent_hash).unwrap_or_default();
                        // Mirror geth's `controller.go:300` flow: re-execute (here:
                        // recompute) the previous block's last flashblock with
                        // `isLastFlashBlock=true` and validate the resulting hash against
                        // the new base's `parent_hash`. On mismatch, geth sets
                        // `Skipping=true` and abandons the new base via early return; we
                        // model that by resetting state (which leaves the processor only
                        // accepting a fresh base, dropping subsequent deltas).
                        if !state.stored_flashblocks.is_empty() {
                            match Self::build_is_final_emission(
                                latest_block,
                                latest_idx,
                                &state.stored_flashblocks,
                                state.accumulated_db.as_ref(),
                                stored_parent_hash,
                            ) {
                                Ok(emission) => {
                                    pending_final_emission = Some(emission);
                                }
                                Err(reason) => {
                                    warn!(
                                        block = latest_block,
                                        new_base_parent_hash = %stored_parent_hash,
                                        reason = %reason,
                                        "is_final hash mismatch on next-base transition; resetting state and dropping new base (geth equivalent: Skipping=true)"
                                    );
                                    state.reset();
                                    return Ok(());
                                }
                            }
                        }
                        state.start_block(flashblock);
                        if awaiting {
                            // The previous block was replayed and has not yet been confirmed
                            // by a canonical-block notification. Defer this transition: the
                            // base is buffered, no FIRE line is emitted yet, and we wait for
                            // the canonical-block signal to either confirm the parent
                            // (replay block N+1) or contradict it (discard the buffered
                            // flashblocks).
                            state.pending_state = true;
                            // Drop the carried-forward DB — if the canonical confirms a
                            // different parent hash we cannot reuse this snapshot. A
                            // matching canonical triggers a fresh bootstrap during replay.
                            state.accumulated_db = None;
                            debug!(
                                block = block_number,
                                parent_hash = %stored_parent_hash,
                                "deferring next-block base while awaiting canonical confirmation of parent"
                            );
                            drop(state);
                            self.emit_final_if_pending(pending_final_emission);
                            return Ok(());
                        }
                    }
                    SequenceValidationResult::Duplicate => {
                        debug!(block = block_number, index, "duplicate flashblock; ignoring");
                        return Ok(());
                    }
                    SequenceValidationResult::InvalidNewBlockIndex { index: 0, .. } => {
                        // Block gap but on a base flashblock — opportunistically restart on it.
                        // The accumulated DB is no longer valid (we missed one or more blocks),
                        // so drop it and let the bootstrap path re-fetch canonical state below.
                        warn!(
                            block = block_number,
                            latest_block,
                            latest_idx,
                            "block gap with base flashblock; restarting from new base and dropping accumulated DB"
                        );
                        state.accumulated_db = None;
                        state.start_block(flashblock);
                    }
                    SequenceValidationResult::InvalidNewBlockIndex { .. } => {
                        warn!(
                            block = block_number,
                            index,
                            latest_block,
                            latest_idx,
                            "new block with non-zero index; resetting state and waiting for next base"
                        );
                        state.reset();
                        return Ok(());
                    }
                    SequenceValidationResult::NonSequentialGap { expected, actual } => {
                        warn!(
                            block = block_number,
                            expected,
                            actual,
                            "non-sequential flashblock gap; resetting state and waiting for next base"
                        );
                        state.reset();
                        return Ok(());
                    }
                }
            }
        }

        // If we're already pending on the parent's state, accept the new flashblock into
        // the buffer (the validator above already pushed it) and defer execution until
        // `on_canonical_block` triggers replay.
        if state.pending_state {
            debug!(
                block = block_number,
                index, "parent state still unavailable; buffering flashblock for replay"
            );
            return Ok(());
        }

        let stored_flashblocks = state.stored_flashblocks.clone();
        let mut accumulated_db = state.accumulated_db.take();

        drop(state); // release the lock on the state

        // If the FirstOfNextBlock transition just confirmed the previous block's
        // recomputed hash, re-emit its final flashblock with `is_final = true`
        // before executing the new block. Order in the wire stream stays:
        // …, last(N) partial, is_final(N), first(N+1).
        self.emit_final_if_pending(pending_final_emission.take());

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

        if accumulated_db.is_none() {
            let parent_block = block_number.saturating_sub(1);
            match self.try_bootstrap_provider(parent_block) {
                Some(provider) => {
                    accumulated_db = Some(
                        State::builder()
                            .with_database(StateProviderDatabase::new(provider))
                            .with_bundle_update()
                            .without_state_clear()
                            .build(),
                    );
                }
                None => {
                    // Parent state not yet available. Mark the in-flight sequence as
                    // pending; subsequent deltas accumulate in `stored_flashblocks` and
                    // replay fires when `on_canonical_block(parent_block)` is invoked.
                    let mut state = self.state.lock().expect("flashblock state mutex poisoned");
                    state.pending_state = true;
                    warn!(
                        block = block_number,
                        parent_block,
                        "parent state not available; buffering flashblock for replay on canonical signal"
                    );
                    return Ok(());
                }
            }
        }

        let mut db = accumulated_db.expect("accumulated_db was populated just above");
        self.execute_flashblock(&assembled, index, &new_transactions, &mut db)?;

        let mut state_guard = self.state.lock().expect("flashblock state mutex poisoned");
        state_guard.accumulated_db = Some(db);
        Ok(())
    }

    /// Attempts a single [`StateProviderFactory`] lookup for `parent_block`.
    ///
    /// Returns `Some(provider)` if the parent state is available, `None` otherwise. There is
    /// no retry loop: when state is not yet committed, callers fall back to the pending
    /// buffer and wait for [`Self::on_canonical_block`] to retry once the parent block has
    /// been finalised.
    fn try_bootstrap_provider(&self, parent_block: u64) -> Option<BoxedStateProvider> {
        match self.client.state_by_block_number_or_tag(BlockNumberOrTag::Number(parent_block)) {
            Ok(provider) => Some(provider),
            Err(err) => {
                debug!(parent_block, error = %err, "parent state provider not available");
                None
            }
        }
    }

    /// Signals that the canonical chain just committed block `canonical_block_number` with
    /// hash `canonical_block_hash`. Drives two distinct flows:
    ///
    /// 1. **Replay or discard a pending buffer.** If the processor has buffered flashblocks
    ///    for block `canonical_block_number + 1` (whose parent state was unavailable when
    ///    they arrived), compare the buffered base's `parent_hash` with
    ///    `canonical_block_hash`:
    ///    - Match → bootstrap the parent state and replay every buffered flashblock,
    ///      emitting one FIRE BLOCK line per entry. After a successful replay the in-flight
    ///      block is itself unconfirmed, so [`awaiting_canonical_confirmation`] is set; the
    ///      next-block base will be deferred until *its* parent is also canonically
    ///      confirmed.
    ///    - Mismatch → the buffered flashblocks descend from a tip that the canonical chain
    ///      did not accept; discard them and reset the processor.
    ///
    /// 2. **Confirm or contradict the current in-flight block.** When the processor's
    ///    in-flight block IS `canonical_block_number` (its flashblocks were already emitted
    ///    — typically via a prior replay — and we have been waiting for the canonical chain
    ///    to weigh in), compare the canonical hash with the latest emitted flashblock's
    ///    tip:
    ///    - Match → clear `awaiting_canonical_confirmation`; the next-block base may now
    ///      take the sequential fast path.
    ///    - Mismatch → reset; subsequent flashblocks that depend on the diverged tip would
    ///      compound the error.
    ///
    /// `is_final` re-emission is **not** retried here: matching geth's
    /// `controller.go:268` guard, once a flashblock-tracer is_final attempt is missed
    /// (either because the next-base's `parent_hash` didn't match the recomputed
    /// hash or just because of race condition), the canonical FIRE BLOCK emitted by the live-block
    /// tracer is considered sufficient — downstream consumers see finality via that canonical line
    /// and don't need a duplicate is_final flashblock partial.
    ///
    /// All other cases (no pending buffer, no awaiting confirmation, mismatched block
    /// numbers) are no-ops.
    pub fn on_canonical_block(&self, canonical_block_number: u64, canonical_block_hash: B256) {
        let mut state = self.state.lock().expect("flashblock state mutex poisoned");

        // Always record the latest canonical so subsequent base flashblocks can validate
        // their `parent_hash` against the canonical chain, regardless of any in-flight
        // pending or awaiting-confirmation state.
        if state.latest_canonical.is_none_or(|(prev_num, _)| canonical_block_number >= prev_num) {
            state.latest_canonical = Some((canonical_block_number, canonical_block_hash));
        }

        // Case 2: a canonical for the block we just (re)played — confirm or contradict tip.
        if !state.pending_state && state.awaiting_canonical_confirmation {
            if state.current_block_number == Some(canonical_block_number) {
                let tip_hash = state
                    .stored_flashblocks
                    .last()
                    .map(|fb| fb.diff.block_hash)
                    .unwrap_or_default();
                if tip_hash == canonical_block_hash {
                    debug!(
                        block = canonical_block_number,
                        canonical_block_hash = %canonical_block_hash,
                        "canonical confirms in-flight tip"
                    );
                    state.awaiting_canonical_confirmation = false;
                } else {
                    warn!(
                        block = canonical_block_number,
                        canonical_block_hash = %canonical_block_hash,
                        flashblock_tip = %tip_hash,
                        "canonical contradicts in-flight tip; resetting state"
                    );
                    state.reset();
                }
            }
            return;
        }

        // Case 1: pending buffer waiting for parent state — replay on match, discard on
        // mismatch.
        if !state.pending_state {
            return;
        }
        let pending_block =
            state.current_block_number.expect("pending_state implies an in-flight block");
        if pending_block.saturating_sub(1) != canonical_block_number {
            return;
        }

        let buffered_parent_hash = state
            .stored_flashblocks
            .first()
            .and_then(|fb| fb.base.as_ref().map(|b| b.parent_hash))
            .unwrap_or_default();
        if buffered_parent_hash != canonical_block_hash {
            warn!(
                pending_block,
                canonical_block_number,
                canonical_block_hash = %canonical_block_hash,
                buffered_parent_hash = %buffered_parent_hash,
                "buffered flashblocks descend from a tip that did not canonicalize; discarding"
            );
            state.reset();
            return;
        }

        let Some(provider) = self.try_bootstrap_provider(canonical_block_number) else {
            debug!(
                pending_block,
                canonical_block_number,
                "canonical block signalled but parent state still not available; staying pending"
            );
            return;
        };

        let stored = state.stored_flashblocks.clone();
        state.pending_state = false;
        drop(state);

        let mut accumulated_db = State::builder()
            .with_database(StateProviderDatabase::new(provider))
            .with_bundle_update()
            .without_state_clear()
            .build();

        for (i, fb) in stored.iter().enumerate() {
            let partial = &stored[..=i];
            let assembled = match BlockAssembler::assemble(partial) {
                Ok(a) => a,
                Err(err) => {
                    error!(
                        pending_block,
                        replay_index = i,
                        error = ?err,
                        "replay block assembly failed; resetting state"
                    );
                    let mut state = self.state.lock().expect("flashblock state mutex poisoned");
                    state.reset();
                    return;
                }
            };
            let new_transactions: Vec<Bytes> = if i == 0 {
                assembled.flashblocks[0].diff.transactions.clone()
            } else {
                fb.diff.transactions.clone()
            };
            if let Err(err) = self.execute_flashblock(
                &assembled,
                fb.index,
                &new_transactions,
                &mut accumulated_db,
            ) {
                error!(
                    pending_block,
                    replay_index = i,
                    error = %err,
                    "replay execution failed; resetting state"
                );
                let mut state = self.state.lock().expect("flashblock state mutex poisoned");
                state.reset();
                return;
            }
        }

        let mut state = self.state.lock().expect("flashblock state mutex poisoned");
        state.accumulated_db = Some(accumulated_db);
        // The just-replayed in-flight block has not yet been confirmed by the canonical
        // chain. Defer the next-block transition until that confirmation arrives.
        state.awaiting_canonical_confirmation = true;
        info!(
            pending_block,
            canonical_block_number,
            replayed = stored.len(),
            "replayed buffered flashblocks after canonical block became available"
        );
    }

    /// Computes the post-execution state root from the accumulated EVM bundle.
    ///
    /// Mirrors geth's `finalizedStateDB.IntermediateRoot(true)` at `processor.go:215`
    /// of the streamingfast flashblocks port: the wire's `diff.state_root` is
    /// typically null/zero for unsealed flashblocks, so the canonical state root is
    /// derived locally from the EVM bundle accumulated across all the block's
    /// flashblocks. Returns `B256::ZERO` (via the state provider) for an absent or
    /// empty bundle.
    fn compute_state_root(
        accumulated_db: Option<&AccumulatedDb>,
    ) -> Result<B256, Box<dyn std::error::Error + Send + Sync>> {
        let db = accumulated_db.ok_or_else(|| {
            Box::<dyn std::error::Error + Send + Sync>::from(
                "no accumulated EVM bundle to derive state_root from",
            )
        })?;
        let provider = &db.database.0;
        let hashed = provider.hashed_post_state(&db.bundle_state);
        provider.state_root(hashed).map_err(|e| Box::new(e).into())
    }

    /// Builds the [`PendingFinalEmission`] for block N's final flashblock when the
    /// recomputed block hash matches `expected_parent_hash` (typically the new base's
    /// `parent_hash`).
    ///
    /// Mirrors geth's `executeAndValidateBlock(true, &expectedBlockHash)` at
    /// `controller.go:302`:
    /// 1. Assemble block N's flashblocks via [`BlockAssembler`].
    /// 2. Compute the post-execution state_root via [`Self::compute_state_root`] (the
    ///    revm equivalent of `finalizedStateDB.IntermediateRoot(true)`).
    /// 3. Override the header's `state_root` with the computed value and seal the
    ///    block via [`Header::hash_slow`].
    /// 4. Compare against `expected_parent_hash`.
    ///
    /// Returns `Ok(emission)` on hash match (caller emits it after dropping the state
    /// lock). Returns `Err(reason)` on assembly failure, state_root computation
    /// failure, or hash mismatch — the caller resets state so the new base is dropped
    /// and the processor waits for a fresh restart, matching geth's
    /// `Skipping = true` recovery path.
    fn build_is_final_emission(
        block_number: u64,
        final_index: u64,
        flashblocks: &[Flashblock],
        accumulated_db: Option<&AccumulatedDb>,
        expected_parent_hash: B256,
    ) -> Result<PendingFinalEmission, String> {
        let assembled = BlockAssembler::assemble(flashblocks)
            .map_err(|err| format!("failed to assemble flashblocks: {err:?}"))?;
        let state_root = Self::compute_state_root(accumulated_db)
            .map_err(|err| format!("failed to compute post-execution state_root: {err}"))?;
        let mut block = assembled.block.clone();
        block.header.state_root = state_root;
        let recomputed_hash = block.header.hash_slow();
        if recomputed_hash != expected_parent_hash {
            return Err(format!(
                "block {block_number} recomputed hash {recomputed_hash} (state_root {state_root}) does not match expected parent_hash {expected_parent_hash}"
            ));
        }
        Ok(PendingFinalEmission {
            sealed_block: SealedBlock::new_unchecked(block, recomputed_hash),
            final_index,
        })
    }

    /// Emits a pre-built [`PendingFinalEmission`] on the flashblock tracer. Must be
    /// called after the [`ProcessorState`] mutex has been released (the tracer mutex
    /// is held for the duration of the FIRE BLOCK emission).
    fn emit_final_if_pending(&self, pending: Option<PendingFinalEmission>) {
        let Some(pending) = pending else { return };
        let block_number = pending.sealed_block.header().number;
        let block_hash = pending.sealed_block.hash();
        let mut tracer = self.tracer.lock().expect("flashblock tracer mutex poisoned");
        let block_tracer = FirehoseBlockTracer::start_flashblock_local::<BasePrimitives>(
            tracer.tracer_mut(),
            &pending.sealed_block,
            None,
            pending.final_index,
            true,
        );
        block_tracer.mark_flashblock();
        info!(
            block = block_number,
            final_index = pending.final_index,
            block_hash = %block_hash,
            "emitted is_final flashblock"
        );
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

        let block_hash = assembled
            .flashblocks
            .last()
            .expect("assembled block has at least one flashblock")
            .diff
            .block_hash;
        let sealed_block: SealedBlock<base_common_consensus::BaseBlock> =
            SealedBlock::new_unchecked(assembled.block.clone(), block_hash);

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
