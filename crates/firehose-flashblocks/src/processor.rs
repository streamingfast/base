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
use revm_database::states::bundle_state::BundleRetention;
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

/// Minimum flashblock index at which the processor starts a background speculative
/// state-root precompute after `execute_flashblock`.
///
/// Op-Base flashblocks arrive at a steady ~200 ms cadence and the block typically seals
/// around idx ≈ 10–11. Once we are past this threshold, the next message we receive is
/// likely either the next-block base (peek-driven is_final fires) or an empty final
/// marker. In both common cases the bundle state at the moment is_final is computed
/// matches the bundle state at the moment the spec was launched, so the cached
/// state_root can be used directly — saving the ~100-150 ms trie traversal off the
/// is_final emission latency.
const SPECULATIVE_STATE_ROOT_MIN_INDEX: u64 = 10;

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

/// Classifies a freshly-received flashblock against the WS subscriber's 1-element peek window,
/// producing `(squash, is_final_expected_hash)` for [`FirehoseFlashblocksProcessor::process`].
///
/// Lives outside the processor (and is free of the `Client` type parameter) so the ingress
/// layer can classify at the moment the peek reference is still live — before the owned
/// flashblock is handed to the serialized command queue — without depending on the processor's
/// provider bounds.
#[derive(Debug)]
pub struct FlashblockPeekClassifier;

impl FlashblockPeekClassifier {
    /// Classifies the peeked next message and produces `(squash, is_final_expected_hash)`.
    ///
    /// Exactly one of the two values is "active" (or neither, if the peek is empty or
    /// unrelated):
    ///
    /// - **Squash** — `(true, None)`: the current flashblock is a delta (`index > 0`) and
    ///   the peek shows another message for the same block (a higher-index delta or a
    ///   same-block restart base). The current flashblock's data is accumulated into
    ///   `stored_flashblocks`, but EVM execution and FIRE BLOCK emission are deferred to
    ///   the next non-squashed flashblock. Geth's strict version at `controller.go:394-396`
    ///   only squashes on a same-block restart base; this implementation extends to also
    ///   squash same-block higher-index deltas.
    ///
    /// - **is_final** — `(false, Some(expected_parent_hash))`: the peek shows the base of
    ///   the immediately next block (`peek.block_number == current.block_number + 1` and
    ///   `peek.index == 0`). The current flashblock will execute through the EVM, and just
    ///   before the FIRE BLOCK is flushed the processor will recompute the canonical block
    ///   hash from the post-execution state and override it on the tracer via
    ///   `set_final_flash_block`. The wire emission becomes a single FIRE BLOCK stamped
    ///   `idx + 1000` and sealed with the recomputed hash — matching geth's peek path at
    ///   `controller.go:398-405`.
    ///
    /// - **None** — `(false, None)`: peek is absent or unrelated; the current flashblock
    ///   executes and emits as a non-final partial.
    pub fn classify(current: &Flashblock, peek: Option<&Flashblock>) -> (bool, Option<B256>) {
        let Some(peek) = peek else { return (false, None) };
        let cur_block = current.metadata.block_number;
        let peek_block = peek.metadata.block_number;
        if peek_block == cur_block && current.index > 0 {
            return (true, None);
        }
        if peek_block == cur_block + 1
            && peek.index == 0
            && let Some(base) = peek.base.as_ref()
        {
            return (false, Some(base.parent_hash));
        }
        (false, None)
    }
}

/// Result of [`FirehoseFlashblocksProcessor::execute_flashblock`].
///
/// Both flags are needed by callers to drive post-execution state mutations:
/// - `emitted_is_final` gates the `final_part_sent` write that suppresses the
///   `FirstOfNextBlock` fallback re-emission.
/// - `bundle_changed` gates the `bundle_revision` bump that invalidates a stale
///   speculative state-root cache. The flag is computed inside
///   `execute_flashblock` because that's where the EVM execution decision lives,
///   but the actual state mutation has to happen at the caller (otherwise the
///   `on_canonical_block` merged-replay path — which holds the `state` mutex
///   for the entire loop — would deadlock against the inner re-lock).
#[derive(Debug, Clone, Copy)]
struct ExecuteFlashblockOutcome {
    /// `true` when the FIRE BLOCK was emitted as the is_final partial via the
    /// peek-driven path (and the caller should set `final_part_sent`).
    emitted_is_final: bool,
    /// `true` when pre-execution changes or any transaction actually ran during
    /// this call, mutating the underlying revm `BundleState`.
    bundle_changed: bool,
}

/// A background speculative state-root precompute.
///
/// Spawned at the end of `execute_flashblock` once the flashblock index crosses
/// [`SPECULATIVE_STATE_ROOT_MIN_INDEX`]. The task clones the post-merge `BundleState`
/// and runs `provider.hashed_post_state` + `provider.state_root` off the synchronous
/// processor path, writing the resulting hash into `result` when finished.
///
/// At is_final time, the processor compares `revision` against the current
/// `bundle_revision`. A match means no bundle-changing work happened between spec
/// launch and is_final, so the cached state_root is exactly the value the
/// synchronous path would have computed. A mismatch falls back to a fresh
/// [`FirehoseFlashblocksProcessor::compute_state_root`] call.
///
/// Aborts the in-flight task on drop so a stale spec doesn't keep running after
/// the processor has moved on.
struct SpeculativeStateRoot {
    /// Block number the spec is for. Used to detect a spec leaking across block boundaries.
    block_number: u64,
    /// Flashblock index that triggered the spec launch. Recorded for diagnostics.
    flashblock_index: u64,
    /// Snapshot of `ProcessorState::bundle_revision` at the moment the spec was launched.
    /// A match against the current revision at is_final time means the bundle is unchanged
    /// and the cached state_root is valid.
    revision: u64,
    /// Filled by the background task on completion.
    result: Arc<Mutex<Option<B256>>>,
    /// Handle to the spawned task — aborted on drop.
    handle: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for SpeculativeStateRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeculativeStateRoot")
            .field("block_number", &self.block_number)
            .field("flashblock_index", &self.flashblock_index)
            .field("revision", &self.revision)
            .field("result", &self.result.lock().ok().and_then(|guard| *guard))
            .finish_non_exhaustive()
    }
}

impl Drop for SpeculativeStateRoot {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

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
    /// Highest flashblock index that has actually been **executed** (and hence emitted as a
    /// FIRE BLOCK) for `current_block_number`. May trail [`latest_flashblock_index`] by one
    /// or more positions when intermediate same-block deltas were *squashed* via the peek
    /// path — those deltas were accumulated into `stored_flashblocks` but their EVM
    /// execution and FIRE BLOCK emission were deferred until a non-squashed delta arrives
    /// and runs through all the held-back transactions in one pass. `None` whenever no
    /// flashblock has been executed yet for the current block (e.g. immediately after a
    /// `start_block`).
    last_executed_index: Option<u64>,
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
    /// `true` once the is_final FIRE BLOCK for the current block has been emitted.
    /// Set by the peek-driven path inside [`execute_flashblock`] after a successful
    /// [`firehose_tracer::Tracer::set_final_flash_block`] override. Read on the
    /// `FirstOfNextBlock` transition to suppress the fallback re-emission so we don't
    /// emit two is_final FIRE BLOCKs for the same block. Mirrors geth's
    /// `c.state.FinalPartSent` at `controller.go:284-300`.
    final_part_sent: bool,
    /// Monotonic counter bumped every time `accumulated_db.merge_transitions()` actually
    /// applies pending changes (pre-execution system calls or transactions). Reads of
    /// this counter let [`SpeculativeStateRoot`] tell whether the bundle state it
    /// captured is still the live bundle state: a match means the cached state_root is
    /// safe to reuse; a difference means a follow-up flashblock has mutated the bundle
    /// and the spec is invalid.
    bundle_revision: u64,
    /// In-flight or completed speculative state-root precompute for the current block.
    /// Replaced (and the prior task aborted via [`Drop`]) every time a new flashblock
    /// past [`SPECULATIVE_STATE_ROOT_MIN_INDEX`] is executed.
    speculative_state_root: Option<SpeculativeStateRoot>,
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

/// Failure modes for [`FirehoseFlashblocksProcessor::build_is_final_emission`].
///
/// The `HashMismatch` variant carries diagnostic fields that get surfaced in the
/// caller's `warn!` so a prod log alone can pin down whether the disagreement is
/// in state_root, in some other header field, or in delivery (missing tail
/// flashblocks).
#[derive(Debug)]
enum BuildIsFinalError {
    /// `BlockAssembler::assemble` returned an error.
    AssembleFailed(String),
    /// `compute_state_root` failed (no accumulated_db, or the provider's
    /// state_root computation errored).
    StateRootFailed(String),
    /// Recomputed block hash did not equal `expected_parent_hash`.
    HashMismatch {
        /// Hash produced by hashing the assembled header with the locally-computed
        /// post-execution state_root overridden in.
        recomputed_hash: B256,
        /// Locally-computed post-execution state_root.
        computed_state_root: B256,
        /// What we were comparing against (e.g. the new base's `parent_hash` or
        /// the canonical hash from `on_canonical_block`).
        expected_parent_hash: B256,
        /// Hash of the assembled header WITHOUT our state_root override — i.e.
        /// with the sequencer's wire `diff.state_root` (typically ZERO) left in
        /// place. Equality of this against `expected_parent_hash` would mean the
        /// wire's state_root was actually canonical and our compute diverged;
        /// equality against `recomputed_hash` would mean the wire's state_root
        /// already matched our compute and the disagreement is in another header
        /// field.
        assembled_header_hash_with_wire_state_root: B256,
        /// Number of flashblocks we accumulated for the block.
        flashblock_count: usize,
        /// Cumulative count of transactions across all accumulated flashblocks.
        /// Comparing against `txs=` on the canonical chain commit log surfaces
        /// the "missing tail flashblock" case where the sequencer kept sending
        /// after our last accepted index but the WS subscriber's mailbox dropped
        /// them (or they arrived after canonical and were skipped as stale).
        total_transactions: usize,
    },
}

impl std::fmt::Display for BuildIsFinalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AssembleFailed(reason) => write!(f, "failed to assemble flashblocks: {reason}"),
            Self::StateRootFailed(reason) => {
                write!(f, "failed to compute post-execution state_root: {reason}")
            }
            Self::HashMismatch {
                recomputed_hash,
                computed_state_root,
                expected_parent_hash,
                assembled_header_hash_with_wire_state_root,
                flashblock_count,
                total_transactions,
            } => write!(
                f,
                "recomputed_hash={recomputed_hash} state_root={computed_state_root} \
                 expected_parent_hash={expected_parent_hash} \
                 wire_state_root_header_hash={assembled_header_hash_with_wire_state_root} \
                 flashblocks={flashblock_count} total_transactions={total_transactions}"
            ),
        }
    }
}

impl ProcessorState {
    fn new() -> Self {
        Self {
            current_block_number: None,
            latest_flashblock_index: None,
            last_executed_index: None,
            stored_flashblocks: Vec::new(),
            accumulated_db: None,
            pending_state: false,
            awaiting_canonical_confirmation: false,
            latest_canonical: None,
            final_part_sent: false,
            bundle_revision: 0,
            speculative_state_root: None,
        }
    }

    /// Clear all per-sequence state, forcing the next flashblock to start a fresh sequence
    /// (only a base flashblock at index 0 will be accepted; non-zero indices are dropped).
    /// Leaves `latest_canonical` untouched: knowledge of the canonical chain survives
    /// processor resets so that subsequent flashblocks are still validated against it.
    fn reset(&mut self) {
        self.current_block_number = None;
        self.latest_flashblock_index = None;
        self.last_executed_index = None;
        self.stored_flashblocks.clear();
        self.accumulated_db = None;
        self.pending_state = false;
        self.awaiting_canonical_confirmation = false;
        self.final_part_sent = false;
        // Any reset implies the bundle state we'd been speculating against is gone;
        // bump revision and drop the spec (its Drop aborts the spawned task).
        self.bundle_revision = self.bundle_revision.wrapping_add(1);
        self.speculative_state_root = None;
    }

    /// Begin (or restart) a sequence on a fresh base flashblock at index 0. The accumulated
    /// DB is left untouched — the caller decides whether to keep the carried-forward state
    /// (sequential fast path) or to drop it (block gap / startup) before calling this.
    fn start_block(&mut self, flashblock: Flashblock) {
        debug_assert_eq!(flashblock.index, 0, "start_block requires a base flashblock");
        self.current_block_number = Some(flashblock.metadata.block_number);
        self.latest_flashblock_index = Some(0);
        self.last_executed_index = None;
        self.stored_flashblocks = vec![flashblock];
        // Starting a fresh sequence invalidates any prior pending accumulation.
        self.pending_state = false;
        self.awaiting_canonical_confirmation = false;
        // The new block hasn't been finalized yet.
        self.final_part_sent = false;
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

    /// Returns the highest flashblock index that has actually been executed for the
    /// current in-flight block. Test-only inspection point: integration tests use
    /// this to verify that the canonical-driven replay path correctly stamps
    /// `last_executed_index` so a subsequent delta arriving through `process_inner`
    /// doesn't re-execute already-applied transactions.
    #[doc(hidden)]
    pub fn last_executed_index_for_test(&self) -> Option<u64> {
        self.state.lock().expect("flashblock state mutex poisoned").last_executed_index
    }

    /// Returns `(current_block_number, pending_state, stored_flashblock_count)`.
    /// Test-only: lets a regression test distinguish "buffered as pending" from
    /// "state reset" — the two paths produce the same empty wire output but
    /// differ in the processor's internal accumulation.
    #[doc(hidden)]
    pub fn pending_state_for_test(&self) -> (Option<u64>, bool, usize) {
        let state = self.state.lock().expect("flashblock state mutex poisoned");
        (state.current_block_number, state.pending_state, state.stored_flashblocks.len())
    }

    /// Returns `Some((block_number, flashblock_index, revision, completed))` describing the
    /// most recent speculative state-root precompute launched by the processor, or `None`
    /// if no speculation is currently tracked. `completed` is `true` once the background
    /// task has written its result. Test-only.
    #[doc(hidden)]
    pub fn speculative_state_root_status_for_test(&self) -> Option<(u64, u64, u64, bool)> {
        let state = self.state.lock().expect("flashblock state mutex poisoned");
        let spec = state.speculative_state_root.as_ref()?;
        let completed = spec.result.lock().expect("spec result mutex poisoned").is_some();
        Some((spec.block_number, spec.flashblock_index, spec.revision, completed))
    }

    /// Process a single flashblock event. Errors are logged and swallowed: the processor clears
    /// its in-flight state and accumulated DB so the next base flashblock restarts tracking.
    ///
    /// # Serialization invariant
    ///
    /// This must only ever be driven from a single thread, interleaved with no other call to
    /// `process` or [`Self::on_canonical_block`]. In production that is guaranteed by routing
    /// every event through the single-consumer
    /// [`FirehoseFlashblocksDispatcher`](crate::FirehoseFlashblocksDispatcher) command queue.
    /// `process_inner` deliberately releases the `state` mutex across EVM execution, so calling
    /// this concurrently from multiple producers reintroduces the data race it was built to
    /// prevent (a canonical signal mutating/resetting the in-flight state mid-execution, yielding
    /// wrong state roots and duplicate is_final emissions). Do not call it directly outside that
    /// serialized path.
    ///
    /// `squash` carries the verdict of [`FlashblockPeekClassifier::classify`]: when `true`, the validator
    /// still accepts the message into `stored_flashblocks` (so its transactions are not
    /// lost) but EVM execution and FIRE BLOCK emission are deferred to the next
    /// non-squashed flashblock — which will gather and execute all the held-back
    /// transactions in a single pass.
    ///
    /// `is_final_expected_hash` is `Some(expected_parent_hash)` when the peeked next message
    /// is the base of the immediately next block — i.e. the current flashblock IS the final
    /// partial for its block. The execute path will then call
    /// [`firehose_tracer::Tracer::set_final_flash_block`] with the locally-recomputed hash
    /// and state_root before the FIRE BLOCK is flushed, matching geth's
    /// `Firehose.SetFinalFlashBlock` + `OnBlockEnd` pattern.
    pub fn process(
        &self,
        flashblock: Flashblock,
        squash: bool,
        is_final_expected_hash: Option<B256>,
    ) {
        if let Err(err) = self.process_inner(flashblock, squash, is_final_expected_hash) {
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

    fn process_inner(
        &self,
        flashblock: Flashblock,
        squash: bool,
        is_final_expected_hash: Option<B256>,
    ) -> Result<(), Error> {
        let block_number = flashblock.metadata.block_number;
        let index = flashblock.index;
        debug!(
            block = block_number,
            index,
            squash,
            is_final_expected = ?is_final_expected_hash,
            "process_inner entry"
        );

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

        // Parent-hash sanity check against the canonical chain: any base (index 0)
        // whose `parent_hash` disagrees with a previously-recorded canonical hash for
        // its parent block is dropped. This catches sequencer-reported forks once
        // canonical has weighed in on the parent.
        //
        // No in-flight-tip check is performed here. A previous version compared
        // `flashblock.base.parent_hash` against `stored_flashblocks.last().diff.block_hash`,
        // but the sequencer-supplied `diff.block_hash` is computed with
        // `state_root = ZERO` for unsealed flashblocks while the new base's
        // `parent_hash` is the real canonical hash (computed with the post-execution
        // state root) — so they always differ in normal operation and the check
        // would reject every valid next-block base whose canonical commit hadn't
        // yet propagated to our `latest_canonical`. The rigorous equivalent runs
        // inside the `FirstOfNextBlock` branch via `build_is_final_emission`, which
        // recomputes the previous block's hash via `Header::hash_slow(computed_state_root)`
        // and compares against the new base's `parent_hash` — matching geth's
        // `executeAndValidateBlock(true, expectedHash)` flow.
        //
        // Non-base flashblocks (deltas) inherit the verdict via the in-flight block —
        // they target an already-validated sequence.
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
                        let mut awaiting = state.awaiting_canonical_confirmation;
                        let stored_parent_hash =
                            flashblock.base.as_ref().map(|b| b.parent_hash).unwrap_or_default();
                        // Mirror geth's `controller.go:300` flow: re-execute (here:
                        // recompute) the previous block's last flashblock with
                        // `isLastFlashBlock=true` and validate the resulting hash against
                        // the new base's `parent_hash`. Fallback only — skipped when the
                        // peek-driven path already emitted the is_final partial for the
                        // previous block (matching geth's `!c.state.FinalPartSent` guard
                        // at `controller.go:284`).
                        //
                        // On mismatch we don't drop the new base. Instead we reset the
                        // in-flight state (so the previous block's accumulated EVM bundle
                        // — which we just proved diverged from canonical — is discarded)
                        // and fall through to `start_block(flashblock)` so the new base
                        // is recorded. `accumulated_db` is now `None`, so the execute
                        // path below either bootstraps a fresh state from the parent
                        // (when canonical for the parent is already known) or marks the
                        // sequence pending until the canonical notification arrives —
                        // either way the new block proceeds. Geth's `Skipping=true`
                        // semantics would have dropped the new base outright, breaking
                        // the wire stream until a fresh base for an even later block
                        // arrived; recovering via re-bootstrap is the only practical
                        // option for us because the recompute mismatch may itself be a
                        // false positive (delivery losing tail flashblocks, header-field
                        // divergence between our `BlockAssembler` and reth's canonical
                        // sealing, etc. — see the diagnostic fields surfaced below).
                        if !state.final_part_sent && !state.stored_flashblocks.is_empty() {
                            // Same optimization as `on_canonical_block`: the live
                            // revision still matches what the speculative precompute
                            // captured (no flashblock has executed for the previous
                            // block since the spec was launched), so a hit avoids
                            // the synchronous trie traversal.
                            let cached_state_root = Self::cached_state_root_for(
                                &state,
                                latest_block,
                                state.bundle_revision,
                            );
                            if cached_state_root.is_some() {
                                debug!(
                                    block = latest_block,
                                    revision = state.bundle_revision,
                                    "FirstOfNextBlock-fallback: using speculatively-precomputed state_root"
                                );
                            }
                            match Self::build_is_final_emission(
                                latest_block,
                                latest_idx,
                                &state.stored_flashblocks,
                                state.accumulated_db.as_ref(),
                                stored_parent_hash,
                                cached_state_root,
                            ) {
                                Ok(emission) => {
                                    pending_final_emission = Some(emission);
                                }
                                Err(BuildIsFinalError::HashMismatch {
                                    recomputed_hash,
                                    computed_state_root,
                                    expected_parent_hash,
                                    assembled_header_hash_with_wire_state_root,
                                    flashblock_count,
                                    total_transactions,
                                }) => {
                                    warn!(
                                        block = latest_block,
                                        latest_flashblock_index = latest_idx,
                                        recomputed_hash = %recomputed_hash,
                                        computed_state_root = %computed_state_root,
                                        expected_parent_hash = %expected_parent_hash,
                                        wire_state_root_header_hash = %assembled_header_hash_with_wire_state_root,
                                        flashblock_count,
                                        total_transactions,
                                        "is_final hash mismatch on next-base transition; \
                                         resetting in-flight state and re-bootstrapping for the new block \
                                         (no is_final FIRE BLOCK emitted for the previous block)"
                                    );
                                    state.reset();
                                    // After reset, awaiting flag is cleared on state and
                                    // we shouldn't take the awaiting-defer branch below.
                                    awaiting = false;
                                    // Fall through to `start_block(flashblock)`: the new
                                    // base is recorded and the execute path below will
                                    // re-bootstrap from canonical (or buffer pending) for
                                    // the new block.
                                }
                                Err(err) => {
                                    warn!(
                                        block = latest_block,
                                        latest_flashblock_index = latest_idx,
                                        reason = %err,
                                        "could not build is_final emission for previous block; \
                                         resetting in-flight state and re-bootstrapping for the new block"
                                    );
                                    state.reset();
                                    awaiting = false;
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

        // Base flashblocks (wire idx 0) are never emitted standalone — the printed
        // flash_idx would be 0, which is reserved for canonical FIRE BLOCKs in the
        // firehose protocol. A single consumer reading the merged stream can't tell
        // `flash_idx=0` from a canonical block, so we squash bases the same way we
        // squash same-block deltas: stash in `stored_flashblocks`, defer execution
        // until the next non-squashed event picks it up via the `last_executed_index`
        // gather. The first delta then emits at flash_idx >= 1 (carrying base+delta
        // txs); the peek-driven / canonical-driven is_final paths emit at
        // `final_index + 1000` (>= 1001, no collision).
        //
        // The exception: when peek identified this base as the immediately-final
        // partial (`is_final_expected_hash = Some(_)`), the execute path stamps the
        // FIRE BLOCK with the `+1000` is_final marker (printed value >= 1000), so
        // there's no canonical collision and we run execute_flashblock normally.
        let squash = squash || (index == 0 && is_final_expected_hash.is_none());

        // Squash: the caller's peek showed another same-block message already queued, so
        // the EVM execution + FIRE BLOCK emission for *this* flashblock is deferred.
        // `stored_flashblocks` keeps the data; the next non-squashed flashblock will pick
        // it up via the `last_executed_index` gather logic below.
        if squash {
            debug!(
                block = block_number,
                index,
                "squashing flashblock execution; accumulated for next non-squashed flashblock"
            );
            // Still flush any pending is_final FIRE BLOCK queued by the
            // FirstOfNextBlock transition (squashing the new base must not drop
            // the previous block's finality emission). Release the state lock
            // first — emit_final_if_pending takes the tracer lock and must not
            // be called while holding state.
            drop(state);
            self.emit_final_if_pending(pending_final_emission.take());
            return Ok(());
        }

        let stored_flashblocks = state.stored_flashblocks.clone();
        let last_executed_index = state.last_executed_index;
        let mut accumulated_db = state.accumulated_db.take();

        drop(state); // release the lock on the state

        // If the FirstOfNextBlock transition just confirmed the previous block's
        // recomputed hash, re-emit its final flashblock with `is_final = true`
        // before executing the new block. Order in the wire stream stays:
        // …, last(N) partial, is_final(N), first(N+1).
        self.emit_final_if_pending(pending_final_emission.take());

        let assembled = BlockAssembler::assemble(&stored_flashblocks)
            .map_err(|e| Error::BlockAssembly(Box::new(e)))?;

        // Gather transactions to execute. When prior same-block flashblocks were squashed
        // (their EVM execution deferred), their transactions land in `stored_flashblocks`
        // but past `last_executed_index`; we replay them here so the deferred work runs
        // through the EVM as part of this delta's execution.
        let new_transactions: Vec<Bytes> = stored_flashblocks
            .iter()
            .filter(|fb| match last_executed_index {
                None => true,
                Some(last) => fb.index > last,
            })
            .flat_map(|fb| fb.diff.transactions.clone())
            .collect();
        // Pre-execution changes (EIP-4788 etc.) apply once per block, on the first EVM
        // run for that block. With squashing, "first run" isn't necessarily `index == 0`:
        // the base may have been squashed and the first execution might happen on a later
        // delta. Track this via `last_executed_index == None`.
        let is_first_execution_for_block = last_executed_index.is_none();

        let parent_block = block_number.saturating_sub(1);

        if accumulated_db.is_none() {
            match self.try_bootstrap_provider(parent_block) {
                Some(provider) => {
                    accumulated_db = Some(
                        State::builder()
                            .with_database(StateProviderDatabase::new(provider))
                            .with_bundle_update()
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

        // The parent's canonical *header* must also be available — `execute_flashblock`
        // reads it via `client.header_by_number(parent_block)` to construct the EVM env.
        // State availability and header availability can diverge briefly when a
        // flashblock for block N+1 arrives between the parent's payload-insert and the
        // canonical-chain commit: the in-memory state may be queryable while
        // `header_by_number` (which reads the canonical chain) still returns None.
        // Treat that the same as a bootstrap failure — buffer the sequence so the
        // canonical-block notification replays it once the header lands. Otherwise the
        // error would bubble out of `process_inner`, the outer `process` would reset
        // state, and subsequent flashblocks for the same block would be dropped as
        // "no in-flight sequence" — losing the whole block on the flashblock stream.
        match self.client.header_by_number(parent_block) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let mut state = self.state.lock().expect("flashblock state mutex poisoned");
                state.pending_state = true;
                // Drop the freshly-bootstrapped accumulated_db; the replay path
                // re-bootstraps from the canonical-block notification.
                state.accumulated_db = None;
                warn!(
                    block = block_number,
                    parent_block,
                    "parent header not available; buffering flashblock for replay on canonical signal"
                );
                return Ok(());
            }
        }

        let mut db = accumulated_db.expect("accumulated_db was populated just above");
        let ExecuteFlashblockOutcome { emitted_is_final, bundle_changed } = self
            .execute_flashblock(
                &assembled,
                index,
                &new_transactions,
                is_first_execution_for_block,
                is_final_expected_hash,
                &mut db,
            )?;

        let mut state_guard = self.state.lock().expect("flashblock state mutex poisoned");
        state_guard.accumulated_db = Some(db);
        state_guard.last_executed_index = Some(index);
        if bundle_changed {
            // Invalidate any speculative state-root whose `revision` was captured
            // before this flashblock's merge.
            state_guard.bundle_revision = state_guard.bundle_revision.wrapping_add(1);
        }
        if emitted_is_final {
            // Suppress the FirstOfNextBlock fallback re-emission when the new-block base
            // actually arrives — the is_final FIRE BLOCK has already been emitted inline.
            state_guard.final_part_sent = true;
        }
        // High-index flashblocks are likely the last partial (or one away from it).
        // Kick off a background state-root compute so that when the next-block base
        // (or canonical notification) arrives and triggers is_final, the slow trie
        // traversal has already happened off the critical path.
        if !emitted_is_final && index >= SPECULATIVE_STATE_ROOT_MIN_INDEX {
            self.launch_speculative_state_root(&mut state_guard, block_number, index);
        }
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

    /// Launches a background `tokio::spawn_blocking` task that runs
    /// `provider.hashed_post_state` + `provider.state_root` on a clone of the current
    /// `BundleState`. Records the bundle revision at launch so the is_final path can
    /// validate that the cached result still corresponds to the live bundle.
    ///
    /// Called from `process_inner` after a flashblock at idx ≥
    /// [`SPECULATIVE_STATE_ROOT_MIN_INDEX`] has been executed. The prior spec (if any)
    /// is aborted via its `Drop` impl when overwritten.
    ///
    /// Skips silently when no tokio runtime is available (e.g. in synchronous tests):
    /// the synchronous `compute_state_root` path inside `execute_flashblock` handles
    /// that case unchanged.
    fn launch_speculative_state_root(
        &self,
        state: &mut ProcessorState,
        block_number: u64,
        index: u64,
    ) {
        let Some(db) = state.accumulated_db.as_ref() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            debug!(
                block_number,
                index, "no tokio runtime in scope; skipping speculative state-root precompute"
            );
            return;
        };

        let bundle = db.bundle_state.clone();
        let revision = state.bundle_revision;
        let result: Arc<Mutex<Option<B256>>> = Arc::new(Mutex::new(None));
        let result_for_task = Arc::clone(&result);
        let client = self.client.clone();
        let parent_block = block_number.saturating_sub(1);

        let handle = rt.spawn_blocking(move || {
            let provider =
                match client.state_by_block_number_or_tag(BlockNumberOrTag::Number(parent_block)) {
                    Ok(p) => p,
                    Err(err) => {
                        debug!(
                            parent_block,
                            error = %err,
                            "speculative state-root: parent state unavailable"
                        );
                        return;
                    }
                };
            let hashed = provider.hashed_post_state(&bundle);
            match provider.state_root(hashed) {
                Ok(root) => {
                    if let Ok(mut slot) = result_for_task.lock() {
                        *slot = Some(root);
                    }
                }
                Err(err) => {
                    debug!(
                        parent_block,
                        error = %err,
                        "speculative state-root: state_root computation failed"
                    );
                }
            }
        });

        // Replacing the prior spec (if any) drops it; its `Drop` impl aborts the task.
        state.speculative_state_root = Some(SpeculativeStateRoot {
            block_number,
            flashblock_index: index,
            revision,
            result,
            handle,
        });
        debug!(block_number, index, revision, "speculative state-root precompute launched");
    }

    /// Returns the speculatively-precomputed state_root if one exists and is still
    /// valid for the live bundle (matching block + matching revision + task has
    /// completed). Otherwise returns `None`, signalling that the caller should fall
    /// back to a synchronous [`Self::compute_state_root`].
    fn try_speculative_state_root(
        &self,
        block_number: u64,
        expected_revision: u64,
    ) -> Option<B256> {
        let state = self.state.lock().ok()?;
        Self::cached_state_root_for(&state, block_number, expected_revision)
    }

    /// Same as [`Self::try_speculative_state_root`] but reads from a `ProcessorState`
    /// guard the caller already holds — used by [`Self::on_canonical_block`] and the
    /// `FirstOfNextBlock` fallback in [`Self::process_inner`], both of which lock
    /// `self.state` for the duration of the is_final-emission decision and would
    /// deadlock against re-locking inside [`Self::try_speculative_state_root`].
    fn cached_state_root_for(
        state: &ProcessorState,
        block_number: u64,
        expected_revision: u64,
    ) -> Option<B256> {
        let spec = state.speculative_state_root.as_ref()?;
        if spec.block_number != block_number || spec.revision != expected_revision {
            return None;
        }
        let slot = spec.result.lock().ok()?;
        *slot
    }

    /// Signals that the canonical chain just committed block `canonical_block_number` with
    /// hash `canonical_block_hash`. Drives two distinct flows:
    ///
    /// 1. **Confirm (and is_final-emit) the in-flight block.** When the processor is
    ///    in-flight on `canonical_block_number` (its flashblocks have been executed,
    ///    either via the real-time path or via a prior canonical-driven replay) and
    ///    is_final hasn't already been emitted for it via the peek-driven
    ///    `FirstOfNextBlock` path, re-run the same recompute-then-validate that
    ///    [`Self::build_is_final_emission`] performs in the next-base path:
    ///    - Match → emit the is_final FIRE BLOCK for the block's last flashblock,
    ///      set `final_part_sent` so a later `FirstOfNextBlock` fallback doesn't
    ///      double-emit, and clear any `awaiting_canonical_confirmation` flag.
    ///    - Mismatch → reset; subsequent flashblocks that depended on the diverged
    ///      tip would compound the error.
    ///
    ///    This unifies what used to be two separate handlers (the "awaiting after
    ///    replay" check that compared the wire's `diff.block_hash` against the
    ///    canonical hash, and a missing "confirm the real-time in-flight block"
    ///    path). Both ordering scenarios — `FirstOfNextBlock` arrives before the
    ///    canonical notification, or the canonical notification arrives first —
    ///    now produce the same single is_final emission.
    ///
    /// 2. **Replay or discard a pending buffer.** If the processor has buffered
    ///    flashblocks for block `canonical_block_number + 1` (whose parent state
    ///    was unavailable when they arrived), compare the buffered base's
    ///    `parent_hash` with `canonical_block_hash`:
    ///    - Match → bootstrap the parent state and replay every buffered flashblock,
    ///      emitting one FIRE BLOCK line per entry. After a successful replay the
    ///      in-flight block is itself unconfirmed, so
    ///      [`awaiting_canonical_confirmation`] is set; a later canonical(N+1) hits
    ///      flow 1 above and emits is_final.
    ///    - Mismatch → the buffered flashblocks descend from a tip that the
    ///      canonical chain did not accept; discard them and reset the processor.
    ///
    /// All other cases (no pending buffer, no in-flight match, mismatched block
    /// numbers) just record `latest_canonical` and return.
    pub fn on_canonical_block(&self, canonical_block_number: u64, canonical_block_hash: B256) {
        let mut state = self.state.lock().expect("flashblock state mutex poisoned");

        // Always record the latest canonical so subsequent base flashblocks can validate
        // their `parent_hash` against the canonical chain, regardless of any in-flight
        // pending or awaiting-confirmation state.
        if state.latest_canonical.is_none_or(|(prev_num, _)| canonical_block_number >= prev_num) {
            state.latest_canonical = Some((canonical_block_number, canonical_block_hash));
        }

        // Flow 1: canonical confirms the in-flight block — try is_final emission.
        // Covers both the "awaiting after replay" sub-case (which used to compare
        // the wire's diff.block_hash and was broken when sequencers send null
        // state_root) and the real-time-in-flight sub-case where the next-block
        // base hasn't arrived yet but the canonical notification has.
        if !state.pending_state && state.current_block_number == Some(canonical_block_number) {
            if state.final_part_sent {
                // is_final already emitted via the peek-driven `FirstOfNextBlock`
                // path; this canonical notification is purely informational. Clear
                // any awaiting flag and bail.
                debug!(
                    block = canonical_block_number,
                    canonical_block_hash = %canonical_block_hash,
                    "canonical confirms an already-finalized in-flight block"
                );
                state.awaiting_canonical_confirmation = false;
                return;
            }
            let latest_idx = state.latest_flashblock_index.unwrap_or(0);
            // No bundle-changing work has happened since the last flashblock
            // execution, so the live revision is the one the speculative
            // precompute (if any) was launched against. A hit skips the slow
            // synchronous state_root and is the key to beating reth's full-block
            // FIRE BLOCK out the door.
            let cached_state_root =
                Self::cached_state_root_for(&state, canonical_block_number, state.bundle_revision);
            if cached_state_root.is_some() {
                debug!(
                    block = canonical_block_number,
                    revision = state.bundle_revision,
                    "canonical-driven is_final: using speculatively-precomputed state_root"
                );
            }

            match Self::build_is_final_emission(
                canonical_block_number,
                latest_idx,
                &state.stored_flashblocks,
                state.accumulated_db.as_ref(),
                canonical_block_hash,
                cached_state_root,
            ) {
                Ok(emission) => {
                    // emit_final_if_pending locks self.tracer only; it does not
                    // touch self.state — so holding the state mutex here is safe.
                    self.emit_final_if_pending(Some(emission));
                    state.final_part_sent = true;
                    state.awaiting_canonical_confirmation = false;
                    info!(
                        block = canonical_block_number,
                        canonical_block_hash = %canonical_block_hash,
                        "canonical-driven is_final emitted"
                    );
                }
                Err(BuildIsFinalError::HashMismatch {
                    recomputed_hash,
                    computed_state_root,
                    expected_parent_hash,
                    assembled_header_hash_with_wire_state_root,
                    flashblock_count,
                    total_transactions,
                }) => {
                    warn!(
                        block = canonical_block_number,
                        latest_flashblock_index = latest_idx,
                        recomputed_hash = %recomputed_hash,
                        computed_state_root = %computed_state_root,
                        canonical_block_hash = %expected_parent_hash,
                        wire_state_root_header_hash = %assembled_header_hash_with_wire_state_root,
                        flashblock_count,
                        total_transactions,
                        "canonical contradicts recomputed in-flight tip; resetting state"
                    );
                    state.reset();
                }
                Err(err) => {
                    warn!(
                        block = canonical_block_number,
                        canonical_block_hash = %canonical_block_hash,
                        reason = %err,
                        "could not build is_final emission for in-flight block on canonical \
                         notification; resetting state"
                    );
                    state.reset();
                }
            }
            return;
        }

        // Flow 2: pending buffer waiting for parent state — replay on match, discard
        // on mismatch.
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

        // Edge case: when only the base is buffered, running merged-replay
        // would emit a FIRE BLOCK at flash_idx=0 — which collides with canonical
        // FIRE BLOCKs on the shared firehose stream. There is no
        // "execute through the tracer, take snapshot, but suppress wire output"
        // affordance (mark_failed discards the snapshot), so the cleanest fix
        // is to skip the merged-replay entirely. Clear the pending flag so the
        // next event isn't deferred; the next delta arriving via process_inner
        // will bootstrap a fresh EVM state from the now-canonical parent,
        // gather base+delta txs, and emit at flash_idx >= 1. If no delta
        // arrives and the next-block-base / canonical(N) signal comes directly,
        // the canonical-tracer FIRE BLOCK for block N still reaches downstream
        // consumers — the flashblock-stream just drops the "base-only" partial
        // for this block, which carries no user transactions anyway.
        if state.stored_flashblocks.last().is_some_and(|fb| fb.index == 0) {
            state.pending_state = false;
            // accumulated_db stays None so the next delta re-bootstraps cleanly.
            // last_executed_index stays None so the next delta's execution
            // gathers the base's txs (and fires pre-execution changes).
            info!(
                pending_block,
                canonical_block_number,
                "skipping merged-replay of base-only buffer to avoid flash_idx=0 \
                 collision with canonical FIRE BLOCKs; next delta or canonical \
                 signal will surface the data"
            );
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

        // Hold the state mutex for the ENTIRE replay loop. The previous version
        // dropped state before the loop and relied on a `pending_state = true`
        // boolean as a "soft" defer signal for concurrent WS arrivals — but that
        // boolean is also touched by `start_block` (via `FirstOfNextBlock` /
        // `InvalidNewBlockIndex{0}`) and `reset` (via `NonSequentialGap`), so the
        // soft guard could be cleared or mutated mid-replay by a concurrent
        // `process_inner` call, opening the same race window the boolean was meant
        // to close. Holding the lock is the only correct primitive: WS-subscriber
        // calls to `process_inner` block on `self.state.lock()` for the lifetime
        // of the loop, and `execute_flashblock` deliberately does not touch
        // `self.state` (only `self.tracer`), so there is no deadlock risk.
        //
        // Cost: the WS consumer task waits on state.lock() during replay. The
        // upstream mpsc channel (capacity 100) absorbs incoming flashblocks
        // without dropping; once the loop ends and state is consistent again,
        // the WS task drains its backlog through the normal squash + multi-delta
        // tx-gather path.
        let stored = state.stored_flashblocks.clone();

        let mut accumulated_db = State::builder()
            .with_database(StateProviderDatabase::new(provider))
            .with_bundle_update()
            .build();

        // Merge all buffered flashblocks into a single execute+emit. Downstream
        // firehose consumers only need the state-correct final partial; emitting
        // one FIRE BLOCK per buffered flashblock would re-traverse the same EVM
        // work and stream redundant intermediate partials. Mirrors the
        // peek-driven squash collapse done at the WS-receive path, applied here
        // to the entire accumulated buffer.
        let assembled = match BlockAssembler::assemble(&stored) {
            Ok(a) => a,
            Err(err) => {
                error!(
                    pending_block,
                    flashblock_count = stored.len(),
                    error = ?err,
                    "replay block assembly failed; resetting state"
                );
                state.reset();
                return;
            }
        };
        let all_transactions: Vec<Bytes> =
            stored.iter().flat_map(|fb| fb.diff.transactions.clone()).collect();
        let merged_index =
            stored.last().expect("pending_state implies non-empty stored_flashblocks").index;
        if let Err(err) = self.execute_flashblock(
            &assembled,
            merged_index,
            &all_transactions,
            true,
            // Replay path never claims is_final — block N's flashblocks were
            // buffered before the canonical chain was known and we don't yet know
            // its final hash. The next-block transition (or its own peek-driven
            // pass) will handle is_final later.
            None,
            &mut accumulated_db,
        ) {
            error!(
                pending_block,
                flashblock_count = stored.len(),
                merged_index,
                error = %err,
                "merged replay execution failed; resetting state"
            );
            state.reset();
            return;
        }

        // We still hold the state lock here — no concurrent `process_inner` call
        // could have run during the loop, so the writes below land on a consistent
        // ProcessorState snapshot.
        state.pending_state = false;
        state.accumulated_db = Some(accumulated_db);
        // The merged replay created a fresh bundle from scratch — any prior
        // speculative state-root no longer corresponds to live state. Bump the
        // revision and drop the spec; the next high-index flashblock will
        // re-launch one.
        state.bundle_revision = state.bundle_revision.wrapping_add(1);
        state.speculative_state_root = None;
        // Mark every replayed flashblock as already-executed. Without this, the next
        // delta arriving through `process_inner` would see `last_executed_index = None`
        // and the multi-delta tx-gather filter would include every previously-replayed
        // delta's transactions in the next EVM run — re-applying them and tripping
        // "nonce too low" on the second pass.
        state.last_executed_index = stored.last().map(|fb| fb.index);
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
    /// `parent_hash`, or a canonical-block notification's hash).
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
    /// The emission uses `final_index = latest_idx + 1` rather than `latest_idx`.
    /// This sidesteps the tracer's "flash block index must be strictly greater than
    /// previous" snapshot check: when the regular `execute_flashblock` already ran
    /// for `latest_idx` it stamped `snapshot.flash_index = latest_idx`, so a
    /// follow-up `start_flashblock_local(latest_idx, is_final=true)` would panic.
    /// Mirrors geth's `c.state.CurrentIndex++` at `controller.go:298` before the
    /// fallback `executeAndValidateBlock(true, …)` call. The resulting wire
    /// emission is `(latest_idx + 1) + 1000` instead of the peek-driven path's
    /// `latest_idx + 1000`; both indicate the same final partial via the `>=1000`
    /// marker, and `restore_flash_block_snapshot` on the tracer side picks up the
    /// cumulative trace from the previous iteration's snapshot so the FIRE BLOCK
    /// still carries `0..=latest_idx` of transaction traces.
    ///
    /// Returns `Ok(emission)` on hash match (caller emits it after the recompute).
    /// Returns `Err(reason)` on assembly failure, state_root computation failure,
    /// or hash mismatch — the caller resets state so the new base is dropped and
    /// the processor waits for a fresh restart, matching geth's `Skipping = true`
    /// recovery path.
    fn build_is_final_emission(
        _block_number: u64,
        latest_idx: u64,
        flashblocks: &[Flashblock],
        accumulated_db: Option<&AccumulatedDb>,
        expected_parent_hash: B256,
        precomputed_state_root: Option<B256>,
    ) -> Result<PendingFinalEmission, BuildIsFinalError> {
        let assembled = BlockAssembler::assemble(flashblocks)
            .map_err(|err| BuildIsFinalError::AssembleFailed(format!("{err:?}")))?;
        // Hash the assembled header AS-RECEIVED (using the sequencer's wire
        // `diff.state_root`, which is typically ZERO). When the recompute below
        // mismatches `expected_parent_hash`, comparing this against
        // `expected_parent_hash` and `recomputed_hash` quickly localises the bug:
        //   - equals `expected_parent_hash` → sequencer's wire state_root *was*
        //     canonical and our local state_root computation diverged.
        //   - equals `recomputed_hash` → state_root override was a no-op (wire
        //     state_root already matched our compute) and the disagreement is in
        //     another header field (receipts_root, gas_used, withdrawals_root, …).
        //   - matches neither → either header construction divergence or a wire
        //     header field disagrees with canonical.
        let assembled_header_hash_with_wire_state_root = assembled.block.header.hash_slow();
        let total_transactions: usize =
            flashblocks.iter().map(|fb| fb.diff.transactions.len()).sum();
        // When the caller supplies a precomputed state_root (typically from the
        // speculative cache that fires at index ≥ 10), skip the synchronous
        // 100-150 ms trie traversal — this is the entire point of the cache,
        // and it's what lets the canonical-driven is_final FIRE BLOCK win the
        // race against reth's full-block FIRE BLOCK emitted from `mark_verified`
        // after its own state-root validation.
        let state_root = match precomputed_state_root {
            Some(root) => root,
            None => Self::compute_state_root(accumulated_db)
                .map_err(|err| BuildIsFinalError::StateRootFailed(format!("{err}")))?,
        };
        let mut block = assembled.block.clone();
        block.header.state_root = state_root;
        let recomputed_hash = block.header.hash_slow();
        if recomputed_hash != expected_parent_hash {
            return Err(BuildIsFinalError::HashMismatch {
                recomputed_hash,
                computed_state_root: state_root,
                expected_parent_hash,
                assembled_header_hash_with_wire_state_root,
                flashblock_count: flashblocks.len(),
                total_transactions,
            });
        }
        Ok(PendingFinalEmission {
            sealed_block: SealedBlock::new_unchecked(block, recomputed_hash),
            // Step past the snapshot's `flash_index = latest_idx` so the tracer's
            // strictly-greater check passes.
            final_index: latest_idx + 1,
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
            "FirstOfNextBlock-fallback is_final emitted"
        );
    }

    /// Executes the new transactions of a flashblock and emits a FIRE BLOCK on the
    /// flashblock tracer. Returns `Ok(true)` when the FIRE BLOCK was emitted as the
    /// is_final partial for its block via the peek-driven path (and the caller should
    /// suppress the FirstOfNextBlock fallback by flipping `final_part_sent`);
    /// `Ok(false)` for a normal non-final partial emission.
    ///
    /// When `is_final_expected_hash` is `Some(expected)`, after `executor.finish()`:
    /// 1. The post-execution state root is derived from the EVM bundle.
    /// 2. The assembled header is re-sealed with that state_root via `hash_slow`.
    /// 3. If the recomputed hash equals `expected`, the in-progress block on the tracer
    ///    is mutated through [`firehose_tracer::Tracer::set_final_flash_block`] so the
    ///    next `on_block_end` flush carries the recomputed hash and the `+1000` printed
    ///    flash index (single FIRE BLOCK, matching geth's
    ///    `Firehose.SetFinalFlashBlock` + `OnBlockEnd` pattern).
    /// 4. If the recomputed hash mismatches the expected value, the FIRE BLOCK is
    ///    dropped via `mark_failed` and an [`Error::Execution`] is returned — the
    ///    caller resets state, mirroring geth's `Skipping=true` recovery path.
    fn execute_flashblock(
        &self,
        assembled: &AssembledBlock,
        index: u64,
        new_transactions: &[Bytes],
        is_first_execution_for_block: bool,
        is_final_expected_hash: Option<B256>,
        accumulated_db: &mut AccumulatedDb,
    ) -> Result<ExecuteFlashblockOutcome, Error> {
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
        // Reborrow `accumulated_db` so the outer `&mut` is preserved past the EVM
        // run — we need it again after `executor.finish()` to compute the
        // post-execution state_root for the peek-driven is_final override.
        let evm = evm_config.evm_with_env_and_inspector(&mut *accumulated_db, evm_env, inspector);
        let inner = BaseBlockExecutor::new(evm, ctx, self.client.chain_spec(), receipt_builder);

        let withdrawals = assembled
            .block
            .body
            .withdrawals
            .clone()
            .map(|ws| alloy_eips::eip4895::Withdrawals::new(ws.0));
        let mut executor =
            FirehoseWrappedExecutor::with_hooks(inner, withdrawals, OpPreTxAdjust, OpPostTxExtras);

        if is_first_execution_for_block {
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

        // Promote the per-tx cache transitions accumulated by the EVM into
        // `bundle_state`. Without this, `compute_state_root` reads an empty bundle
        // and `StateProvider::state_root(HashedPostState)` returns the underlying
        // provider's pre-execution state_root (i.e. the PARENT block's state_root)
        // — every is_final recompute then disagrees with canonical. Mirrors the
        // existing pattern in `crates/execution/flashblocks/src/processor.rs:488`.
        accumulated_db.merge_transitions(BundleRetention::Reverts);

        // The bundle changed iff pre-execution or any transaction actually ran:
        // when both are absent, `merge_transitions` is a no-op and a prior
        // speculative state-root (captured at the same revision) is still valid.
        // Callers consume this flag (in process_inner / on_canonical_block) to
        // decide whether to bump `bundle_revision` — the bump cannot happen here
        // because on_canonical_block's merged-replay path already holds the
        // `state` mutex for the whole loop, so re-locking from inside this
        // function would deadlock.
        let bundle_changed = is_first_execution_for_block || !new_transactions.is_empty();

        let mut emitted_is_final = false;
        if let Some(expected_parent_hash) = is_final_expected_hash {
            // Peek-driven is_final path: derive the post-execution state_root, recompute
            // the canonical block hash, and override the tracer's in-progress block so
            // the upcoming `on_block_end` flush emits a single FIRE BLOCK already
            // marked is_final and sealed with the recomputed hash.
            //
            // Try the speculative cache first. The revision at the moment is_final
            // fires is `state.bundle_revision + (bundle_changed ? 1 : 0)` — the +1
            // happens conceptually here (the caller will commit it on success) and
            // we use that projected value as the cache key. A hit means the trie
            // traversal already happened off the synchronous flashblock path during
            // the previous ~200 ms inter-flashblock gap, saving 100-150 ms.
            //
            // Reaching this branch requires `is_final_expected_hash = Some(_)`, which
            // is only set by `process_inner` (peek-driven). The `on_canonical_block`
            // merged-replay path always passes `None`, so re-locking the `state`
            // mutex inside this branch is safe — that caller doesn't hold it.
            let projected_revision = {
                let state = self.state.lock().expect("flashblock state mutex poisoned");
                state.bundle_revision + if bundle_changed { 1 } else { 0 }
            };
            let state_root_result =
                match self.try_speculative_state_root(block_number, projected_revision) {
                    Some(root) => {
                        debug!(
                            block_number,
                            index,
                            revision = projected_revision,
                            state_root = %root,
                            "is_final: using speculatively-precomputed state_root"
                        );
                        Ok(root)
                    }
                    None => Self::compute_state_root(Some(&*accumulated_db)),
                };
            match state_root_result {
                Ok(state_root) => {
                    let mut recomputed_header = assembled.block.header.clone();
                    recomputed_header.state_root = state_root;
                    let recomputed_hash = recomputed_header.hash_slow();
                    if recomputed_hash == expected_parent_hash {
                        block_tracer
                            .tracer_mut()
                            .set_final_flash_block(recomputed_hash, state_root);
                        emitted_is_final = true;
                        debug!(
                            block = block_number,
                            index,
                            recomputed_hash = %recomputed_hash,
                            state_root = %state_root,
                            "peek-driven is_final emitted inline"
                        );
                    } else {
                        let err = std::io::Error::other(format!(
                            "block {block_number} recomputed hash {recomputed_hash} (state_root {state_root}) does not match expected parent_hash {expected_parent_hash}"
                        ));
                        warn!(
                            block = block_number,
                            index,
                            recomputed_hash = %recomputed_hash,
                            expected = %expected_parent_hash,
                            "peek-driven is_final: hash mismatch; dropping FIRE BLOCK and resetting state"
                        );
                        block_tracer.mark_failed(&err);
                        return Err(Error::Execution(Box::new(err)));
                    }
                }
                Err(err) => {
                    let io_err =
                        std::io::Error::other(format!("state_root computation failed: {err}"));
                    warn!(
                        block = block_number,
                        index,
                        error = %err,
                        "peek-driven is_final: state_root computation failed; dropping FIRE BLOCK and resetting state"
                    );
                    block_tracer.mark_failed(&io_err);
                    return Err(Error::Execution(Box::new(io_err)));
                }
            }
        }

        // When this isn't the final partial for the block, snapshot the in-progress
        // block so the NEXT same-block flashblock iteration can restore these tx
        // traces, balance/code changes, system calls, and ordinals into its own
        // (initially empty) block buffer at `on_block_start`. Without this each
        // FIRE BLOCK at index K would carry only the contributions new to that
        // iteration, instead of the cumulative trace from indices `0..=K` that
        // downstream firehose consumers expect.
        //
        // Mirrors geth's `firehoseTracer.SnapshotFlashBlockForNextIteration()` call
        // at `eth/tracers/firehose.go:180` (inside `Process`, before the early
        // return on `!isLastFlashBlock`). Safe to call unconditionally on the
        // non-final path: a stale snapshot left over when no more same-block
        // flashblocks arrive is discarded by the tracer's
        // `handle_flash_block_start` block-number check the next time a
        // flashblock for a *different* block arrives.
        if !emitted_is_final {
            block_tracer.tracer_mut().snapshot_flash_block_for_next_iteration();
        }

        block_tracer.mark_flashblock();
        drop(tracer);

        info!(
            block = block_number,
            index,
            tx_count = new_tx_count,
            is_final = emitted_is_final,
            "emitted flashblock partial FIRE event"
        );

        Ok(ExecuteFlashblockOutcome { emitted_is_final, bundle_changed })
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
        // Direct (no-peek) entry point. Without look-ahead we can't classify the
        // current message as squashable nor as the final partial — every flashblock
        // executes individually and the FirstOfNextBlock fallback handles is_final
        // re-emission when the next-block base eventually arrives.
        self.process(flashblock, false, None);
    }

    fn on_flashblock_received_with_peek(&self, flashblock: Flashblock, peek: Option<&Flashblock>) {
        let (squash, is_final_expected_hash) = FlashblockPeekClassifier::classify(&flashblock, peek);
        self.process(flashblock, squash, is_final_expected_hash);
    }
}
