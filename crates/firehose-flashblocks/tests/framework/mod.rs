//! Test framework for firehose flashblocks integration tests.
//!
//! This module provides:
//! - [`GenesisClient`]: a minimal in-memory reth provider mock backed by a genesis allocation.
//! - [`GenesisStateProvider`]: a thin [`StateProvider`] wrapper around [`StateProviderTest`].
//! - [`flash_base`] / [`flash_delta`] / [`canonical_block`]: helpers to build [`TestEvent`] fixtures.
//! - [`TestEvent`]: discriminated event type for test sequences (flashblock or canonical block).
//! - [`FireEvent`]: structured representation of a single FIRE output line (INIT or BLOCK).
//! - [`parse_fire_events`]: parses all FIRE lines from raw tracer output.
//! - [`run_flashblock_sequence`]: end-to-end harness — drives the processor directly, returns output.

use std::{
    collections::HashSet,
    ops::RangeInclusive,
    sync::{Arc, Mutex},
};

use alloy_consensus::{BlockBody, Header};
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy_genesis::Genesis;
use alloy_primitives::{
    Address, BlockHash, BlockNumber, Bloom, Bytes, StorageKey, TxHash, TxNumber, B256, U256,
    hex, keccak256,
};
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::{BaseBlock, BasePrimitives, BaseReceipt, BaseTxEnvelope};
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblocksPayloadV1,
    Metadata,
};
use base_execution_chainspec::BaseChainSpec;
use base_flashblocks::FlashblocksReceiver;
use base_firehose_flashblocks::{FirehoseFlashblocksProcessor, FlashblocksTracerHandle};
use base64::Engine as _;
use firehose_tracer::{
    InMemoryBuffer,
    config::{ChainClient, ChainConfig, Config},
    pb::Block as EthBlock,
};
use prost::Message as _;
use reth_chainspec::{ChainInfo, ChainSpecProvider, EthChainSpec};
use reth_db_models::StoredBlockBodyIndices;
use reth_firehose::FirehoseBlockTracer;
use reth_primitives_traits::{Account, RecoveredBlock, SealedBlock, SealedHeader};
use reth_provider::{
    AccountReader, BlockBodyIndicesProvider, BlockHashReader, BlockIdReader, BlockNumReader,
    BlockReader, BlockReaderIdExt, BlockSource, BytecodeReader, HashedPostStateProvider,
    HeaderProvider, NodePrimitivesProvider, ReceiptProvider, ReceiptProviderIdExt,
    StateProofProvider, StateProvider, StateProviderBox, StateProviderFactory, StateRootProvider,
    StorageRootProvider, TransactionVariant, TransactionsProvider,
};
use reth_revm::test_utils::StateProviderTest;
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie::{
    AccountProof, HashedPostState, HashedStorage, MultiProof, MultiProofTargets, StorageMultiProof,
    StorageProof, TrieInput, updates::TrieUpdates,
};
use serde_json::json;

// ── TestEvent ─────────────────────────────────────────────────────────────────

/// A discriminated event for test sequences fed into [`run_flashblock_sequence`].
///
/// The runner processes events in order:
/// - [`TestEvent::Flashblock`] is serialised and sent over the WS connection (same as today).
/// - [`TestEvent::CanonicalBlock`] updates [`GenesisClient`]'s internal state to mark block N as
///   available; no WS message is emitted. This unblocks the processor's bootstrap path when it
///   calls `state_by_block_number_or_tag(BlockNumberOrTag::Number(N))`.
#[derive(Clone)]
pub(crate) enum TestEvent {
    /// A flashblock to be sent over the WebSocket connection.
    ///
    /// Boxed to avoid a large-variant size imbalance with [`TestEvent::CanonicalBlock`].
    Flashblock(Box<Flashblock>),
    /// Signals that canonical block N (with the given block hash) is now available from the
    /// chain provider, and triggers an accompanying canonical FIRE BLOCK emission.
    CanonicalBlock {
        /// Block number.
        block_number: u64,
        /// Block hash to seal the emitted canonical FIRE BLOCK with.
        block_hash: B256,
    },
}

impl TestEvent {
    /// Wraps a [`Flashblock`] as a [`TestEvent::Flashblock`].
    pub(crate) fn flashblock(fb: Flashblock) -> Self {
        Self::Flashblock(Box::new(fb))
    }
}

// ── Mock client ──────────────────────────────────────────────────────────────

/// Inner mutable state for [`GenesisClient`], shared via `Arc<Mutex<...>>`.
#[derive(Debug, Default)]
struct GenesisClientInner {
    /// Block numbers that have been made available via a [`TestEvent::CanonicalBlock`] event.
    available_blocks: HashSet<u64>,
}

/// A minimal in-memory client for use in tests.
///
/// Holds a genesis (used to seed account state and chain spec) and a pre-built genesis header.
/// Only the three methods called by [`FirehoseFlashblocksProcessor`] are implemented; all others
/// return `Ok(None)` / `Ok(Vec::new())` or delegate to [`StateProviderTest`].
///
/// [`GenesisClient`] tracks which canonical blocks are "available" so that the processor's
/// bootstrap path (via `state_by_block_number_or_tag`) can be unblocked by a
/// [`TestEvent::CanonicalBlock`] event without sending anything over the WebSocket connection.
#[derive(Clone, Debug)]
pub(crate) struct GenesisClient {
    /// The chain spec derived from the genesis.
    pub(crate) chain_spec: Arc<BaseChainSpec>,
    /// The raw genesis used to seed account state.
    pub(crate) genesis: Genesis,
    /// The genesis block header computed from the genesis.
    pub(crate) genesis_header: Header,
    /// Shared inner state tracking which canonical blocks are available.
    inner: Arc<Mutex<GenesisClientInner>>,
}

impl GenesisClient {
    /// Constructs a [`GenesisClient`] from a genesis allocation.
    pub(crate) fn new(genesis: Genesis) -> Self {
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis.clone()));
        let genesis_header =
            reth_chainspec::make_genesis_header(&genesis, &chain_spec.inner.hardforks);
        Self {
            chain_spec,
            genesis,
            genesis_header,
            inner: Arc::new(Mutex::new(GenesisClientInner::default())),
        }
    }

    /// Marks canonical block `block_number` as available to the provider.
    ///
    /// After this call, `state_by_block_number_or_tag(BlockNumberOrTag::Number(block_number))`
    /// will succeed and return a genesis-seeded state provider for that block number.
    pub(crate) fn mark_canonical_block_available(&self, block_number: u64) {
        self.inner
            .lock()
            .expect("genesis client inner mutex poisoned")
            .available_blocks
            .insert(block_number);
    }

    /// Returns `true` if `block_number` is the genesis block (block 0) or has been explicitly
    /// marked as available via [`mark_canonical_block_available`].
    fn is_block_available(&self, block_number: u64) -> bool {
        if block_number == 0 {
            return true;
        }
        self.inner
            .lock()
            .expect("genesis client inner mutex poisoned")
            .available_blocks
            .contains(&block_number)
    }

    /// Synthesises a [`Header`] for the given block number.
    ///
    /// The genesis header is used as a template. `number` is set to `block_number` and
    /// `timestamp` is advanced by 2 seconds per block from the genesis timestamp. All other
    /// fields are inherited from genesis so that EVM environment construction produces a
    /// consistent result regardless of which parent block the processor looks up.
    pub(crate) fn header_for_block(&self, block_number: u64) -> Header {
        Header {
            number: block_number,
            timestamp: self.genesis_header.timestamp + block_number * 2,
            parent_hash: B256::ZERO,
            ..self.genesis_header.clone()
        }
    }
}

impl ChainSpecProvider for GenesisClient {
    type ChainSpec = BaseChainSpec;

    fn chain_spec(&self) -> Arc<BaseChainSpec> {
        Arc::clone(&self.chain_spec)
    }
}

impl NodePrimitivesProvider for GenesisClient {
    type Primitives = BasePrimitives;
}

impl BlockHashReader for GenesisClient {
    fn block_hash(&self, _n: u64) -> ProviderResult<Option<B256>> {
        Ok(None)
    }

    fn canonical_hashes_range(
        &self,
        _s: BlockNumber,
        _e: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        Ok(Vec::new())
    }
}

impl BlockNumReader for GenesisClient {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        Ok(ChainInfo { best_hash: B256::ZERO, best_number: 0 })
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(0)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(0)
    }

    fn block_number(&self, _h: B256) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
}

impl BlockIdReader for GenesisClient {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }
}

impl HeaderProvider for GenesisClient {
    type Header = Header;

    fn header(&self, _h: BlockHash) -> ProviderResult<Option<Header>> {
        Ok(Some(self.genesis_header.clone()))
    }

    fn header_by_number(&self, n: u64) -> ProviderResult<Option<Header>> {
        // The processor calls this to build the EVM env for block N (looking up the parent
        // header). Return a synthesised header with the correct block number so that
        // `next_evm_env` computes `block_env.number = parent.number + 1` correctly.
        Ok(Some(self.header_for_block(n)))
    }

    fn headers_range(
        &self,
        _r: impl std::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Header>> {
        Ok(vec![self.genesis_header.clone()])
    }

    fn sealed_header(&self, _n: BlockNumber) -> ProviderResult<Option<SealedHeader<Header>>> {
        Ok(None)
    }

    fn sealed_headers_range(
        &self,
        _r: impl std::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Header>>> {
        Ok(Vec::new())
    }

    fn sealed_headers_while(
        &self,
        _r: impl std::ops::RangeBounds<BlockNumber>,
        _p: impl FnMut(&SealedHeader<Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Header>>> {
        Ok(Vec::new())
    }
}

impl BlockBodyIndicesProvider for GenesisClient {
    fn block_body_indices(&self, _n: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        Ok(None)
    }

    fn block_body_indices_range(
        &self,
        _r: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        Ok(Vec::new())
    }
}

impl TransactionsProvider for GenesisClient {
    type Transaction = BaseTxEnvelope;

    fn transaction_id(&self, _h: TxHash) -> ProviderResult<Option<TxNumber>> {
        Ok(None)
    }

    fn transaction_by_id(&self, _id: TxNumber) -> ProviderResult<Option<BaseTxEnvelope>> {
        Ok(None)
    }

    fn transaction_by_id_unhashed(
        &self,
        _id: TxNumber,
    ) -> ProviderResult<Option<BaseTxEnvelope>> {
        Ok(None)
    }

    fn transaction_by_hash(&self, _h: TxHash) -> ProviderResult<Option<BaseTxEnvelope>> {
        Ok(None)
    }

    fn transaction_by_hash_with_meta(
        &self,
        _h: TxHash,
    ) -> ProviderResult<
        Option<(BaseTxEnvelope, alloy_consensus::transaction::TransactionMeta)>,
    > {
        Ok(None)
    }

    fn transactions_by_block(
        &self,
        _b: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<BaseTxEnvelope>>> {
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        _r: impl std::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<BaseTxEnvelope>>> {
        Ok(Vec::new())
    }

    fn transactions_by_tx_range(
        &self,
        _r: impl std::ops::RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<BaseTxEnvelope>> {
        Ok(Vec::new())
    }

    fn senders_by_tx_range(
        &self,
        _r: impl std::ops::RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        Ok(Vec::new())
    }

    fn transaction_sender(&self, _id: TxNumber) -> ProviderResult<Option<Address>> {
        Ok(None)
    }
}

impl ReceiptProvider for GenesisClient {
    type Receipt = BaseReceipt;

    fn receipt(&self, _id: TxNumber) -> ProviderResult<Option<BaseReceipt>> {
        Ok(None)
    }

    fn receipt_by_hash(&self, _h: TxHash) -> ProviderResult<Option<BaseReceipt>> {
        Ok(None)
    }

    fn receipts_by_block(
        &self,
        _b: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<BaseReceipt>>> {
        Ok(None)
    }

    fn receipts_by_tx_range(
        &self,
        _r: impl std::ops::RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<BaseReceipt>> {
        Ok(Vec::new())
    }

    fn receipts_by_block_range(
        &self,
        _r: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<BaseReceipt>>> {
        Ok(Vec::new())
    }
}

impl ReceiptProviderIdExt for GenesisClient {}

impl BlockReader for GenesisClient {
    type Block = BaseBlock;

    fn find_block_by_hash(&self, _h: B256, _s: BlockSource) -> ProviderResult<Option<BaseBlock>> {
        Ok(None)
    }

    fn block(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<BaseBlock>> {
        Ok(None)
    }

    fn pending_block(&self) -> ProviderResult<Option<RecoveredBlock<BaseBlock>>> {
        Ok(None)
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(RecoveredBlock<BaseBlock>, Vec<BaseReceipt>)>> {
        Ok(None)
    }

    fn recovered_block(
        &self,
        _id: BlockHashOrNumber,
        _k: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<BaseBlock>>> {
        Ok(None)
    }

    fn sealed_block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _k: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<BaseBlock>>> {
        Ok(None)
    }

    fn block_range(&self, _r: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<BaseBlock>> {
        Ok(Vec::new())
    }

    fn block_with_senders_range(
        &self,
        _r: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<BaseBlock>>> {
        Ok(Vec::new())
    }

    fn recovered_block_range(
        &self,
        _r: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<BaseBlock>>> {
        Ok(Vec::new())
    }

    fn block_by_transaction_id(&self, _id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
}

impl BlockReaderIdExt for GenesisClient {
    fn block_by_id(&self, _id: BlockId) -> ProviderResult<Option<BaseBlock>> {
        Ok(None)
    }

    fn sealed_header_by_id(
        &self,
        _id: BlockId,
    ) -> ProviderResult<Option<SealedHeader<Header>>> {
        Ok(None)
    }

    fn header_by_id(&self, _id: BlockId) -> ProviderResult<Option<Header>> {
        Ok(Some(self.genesis_header.clone()))
    }
}

// ── GenesisStateProvider ─────────────────────────────────────────────────────

/// A thin wrapper around [`StateProviderTest`] that also implements the full
/// [`StateProvider`] trait surface needed by `reth_revm`'s `StateProviderDatabase`.
pub(crate) struct GenesisStateProvider(StateProviderTest);

impl GenesisStateProvider {
    /// Constructs a [`GenesisStateProvider`] seeded with all accounts from `genesis.alloc`.
    pub(crate) fn new(genesis: &Genesis) -> Self {
        let mut inner = StateProviderTest::default();
        for (addr, acc) in &genesis.alloc {
            let reth_account = Account {
                balance: acc.balance,
                nonce: acc.nonce.unwrap_or_default(),
                bytecode_hash: acc.code.as_ref().map(alloy_primitives::keccak256),
            };
            let bytecode = acc.code.as_ref().map(|c| Bytes::copy_from_slice(c));
            let storage = acc
                .storage
                .as_ref()
                .map(|s| {
                    s.iter()
                        .map(|(k, v)| (StorageKey::from(k.0), U256::from_be_bytes(v.0)))
                        .collect()
                })
                .unwrap_or_default();
            inner.insert_account(*addr, reth_account, bytecode, storage);
        }
        Self(inner)
    }
}

impl AccountReader for GenesisStateProvider {
    fn basic_account(&self, addr: &Address) -> ProviderResult<Option<Account>> {
        self.0.basic_account(addr)
    }
}

impl BlockHashReader for GenesisStateProvider {
    fn block_hash(&self, n: u64) -> ProviderResult<Option<B256>> {
        self.0.block_hash(n)
    }

    fn canonical_hashes_range(
        &self,
        s: BlockNumber,
        e: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(s, e)
    }
}

impl StateRootProvider for GenesisStateProvider {
    fn state_root(&self, _hs: HashedPostState) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn state_root_from_nodes(&self, _i: TrieInput) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn state_root_with_updates(
        &self,
        _hs: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _i: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }
}

impl StorageRootProvider for GenesisStateProvider {
    fn storage_root(
        &self,
        _addr: Address,
        _hs: HashedStorage,
    ) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn storage_proof(
        &self,
        _addr: Address,
        _slot: B256,
        _hs: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn storage_multiproof(
        &self,
        _addr: Address,
        _slots: &[B256],
        _hs: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl StateProofProvider for GenesisStateProvider {
    fn proof(
        &self,
        _i: TrieInput,
        _addr: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn multiproof(&self, _i: TrieInput, _t: MultiProofTargets) -> ProviderResult<MultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn witness(&self, _i: TrieInput, _t: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl HashedPostStateProvider for GenesisStateProvider {
    fn hashed_post_state(&self, bundle: &revm::database::BundleState) -> HashedPostState {
        self.0.hashed_post_state(bundle)
    }
}

impl StateProvider for GenesisStateProvider {
    fn storage(
        &self,
        addr: Address,
        key: StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        self.0.storage(addr, key)
    }

    fn storage_by_hashed_key(
        &self,
        addr: Address,
        hk: StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        self.0.storage_by_hashed_key(addr, hk)
    }
}

impl BytecodeReader for GenesisStateProvider {
    fn bytecode_by_hash(&self, h: &B256) -> ProviderResult<Option<reth_primitives_traits::Bytecode>> {
        self.0.bytecode_by_hash(h)
    }
}

impl StateProviderFactory for GenesisClient {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(GenesisStateProvider::new(&self.genesis)))
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Number(n) if !self.is_block_available(n) => {
                Err(ProviderError::BlockBodyIndicesNotFound(n))
            }
            _ => Ok(Box::new(GenesisStateProvider::new(&self.genesis))),
        }
    }

    fn history_by_block_number(&self, _n: BlockNumber) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(GenesisStateProvider::new(&self.genesis)))
    }

    fn history_by_block_hash(&self, _h: BlockHash) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(GenesisStateProvider::new(&self.genesis)))
    }

    fn state_by_block_hash(&self, _h: BlockHash) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(GenesisStateProvider::new(&self.genesis)))
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(GenesisStateProvider::new(&self.genesis)))
    }

    fn pending_state_by_hash(
        &self,
        _h: B256,
    ) -> ProviderResult<Option<StateProviderBox>> {
        Ok(Some(Box::new(GenesisStateProvider::new(&self.genesis))))
    }

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        Ok(Some(Box::new(GenesisStateProvider::new(&self.genesis))))
    }
}

// ── Hash helper ──────────────────────────────────────────────────────────────

/// Returns a deterministic [`B256`] derived from a short label.
///
/// Used by test builders to assign readable, distinct block hashes to flashblocks and
/// canonical blocks. Tests typically use labels like `"1a"`, `"2a"`, `"3a"` for the
/// canonical fork and `"3b"`, `"3c"` for siblings/alternates.
pub(crate) fn hash(label: &str) -> B256 {
    keccak256(label.as_bytes())
}

// ── Flashblock builders ──────────────────────────────────────────────────────

/// Constructs a base flashblock (index 0) for the given block number, wrapped as a [`TestEvent`].
///
/// `block_hash` is the resulting block's hash (carried on the diff). `parent_hash` is the
/// hash of the parent block referenced by this base. All optional fields use sensible
/// defaults; the base flashblock carries no transactions.
pub(crate) fn flash_base(
    block_number: u64,
    block_hash: B256,
    parent_hash: B256,
    timestamp: u64,
) -> TestEvent {
    let base = ExecutionPayloadBaseV1 {
        parent_beacon_block_root: B256::ZERO,
        parent_hash,
        fee_recipient: Address::ZERO,
        prev_randao: B256::ZERO,
        block_number,
        gas_limit: 30_000_000,
        timestamp,
        extra_data: Bytes::default(),
        base_fee_per_gas: U256::from(7u64),
    };

    let diff = ExecutionPayloadFlashblockDeltaV1 {
        state_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::default(),
        gas_used: 0,
        block_hash,
        transactions: Vec::new(),
        withdrawals: Vec::new(),
        withdrawals_root: B256::ZERO,
        blob_gas_used: Some(0),
    };

    let payload = FlashblocksPayloadV1 {
        payload_id: PayloadId::new([0u8; 8]),
        index: 0,
        base: Some(base),
        diff,
        metadata: json!({ "block_number": block_number }),
    };

    let metadata = Metadata { block_number };

    TestEvent::flashblock(Flashblock {
        payload_id: payload.payload_id,
        index: payload.index,
        base: payload.base,
        diff: payload.diff,
        metadata,
    })
}

/// Constructs a delta flashblock (index > 0) for the given block number, wrapped as a [`TestEvent`].
///
/// `block_hash` is the candidate-tip hash carried on this delta's diff. The parent hash
/// is implicit — it lives on the base flashblock that started the sequence. Carries no
/// new transactions; used to test sequence progression.
pub(crate) fn flash_delta(block_number: u64, block_hash: B256, index: u64) -> TestEvent {
    let diff = ExecutionPayloadFlashblockDeltaV1 {
        state_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::default(),
        gas_used: 0,
        block_hash,
        transactions: Vec::new(),
        withdrawals: Vec::new(),
        withdrawals_root: B256::ZERO,
        blob_gas_used: Some(0),
    };

    let metadata = Metadata { block_number };

    TestEvent::flashblock(Flashblock {
        payload_id: PayloadId::new([0u8; 8]),
        index,
        base: None,
        diff,
        metadata,
    })
}

/// Signals that canonical block `block_number` (with the given `block_hash`) is now
/// available from the provider, and triggers a canonical FIRE BLOCK emission with that
/// hash. The parent hash is implicit — the canonical tracer seals from a synthesised
/// header.
pub(crate) const fn canonical_block(block_number: u64, block_hash: B256) -> TestEvent {
    TestEvent::CanonicalBlock { block_number, block_hash }
}


// ── FireEvent ────────────────────────────────────────────────────────────────

/// A structured representation of a single FIRE output line.
///
/// Parsed from the raw tracer output written to the in-memory buffer during tests.
/// The two FIRE line types relevant to tests are:
///
/// - `FIRE INIT <version> <node_name> <node_version>` — emitted once per tracer instance.
/// - `FIRE BLOCK <block_num> <flash_idx> <block_hash> <prev_num> <prev_hash> <lib_num>
///   <timestamp_unix_nano> <payload_base64>` — one per executed block or flashblock partial.
///
/// `Block` and `FlashBlock` variants include the decoded `sf.ethereum.type.v2.Block` protobuf
/// payload. Use [`assert_fire_events_metadata_eq`] when you only care about protocol metadata
/// fields, or [`assert_fire_events_eq`] for full comparison including the decoded block payload.
///
/// The `flash_idx` encoding on the wire:
/// - `0` → canonical (non-flash) block; maps to `FireEvent::Block`.
/// - `1..=999` → flash partial, `is_final = false`; maps to `FireEvent::FlashBlock`.
/// - `>=1000` → flash partial, `is_final = true` (`idx = printed - 1000`); maps to
///   `FireEvent::FlashBlock` with `is_final: true`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FireEvent {
    /// A `FIRE INIT` line emitted at tracer startup.
    Init {
        /// Protocol version string (e.g. `"3.1"`).
        version: String,
        /// Node/client name (e.g. `"reth-flashblock"`).
        node_name: String,
        /// Node/client version string.
        node_version: String,
    },
    /// A `FIRE BLOCK` line where `printed_flash_idx == 0` (canonical block).
    Block {
        /// Block number.
        block_number: u64,
        /// Block hash parsed from the FIRE BLOCK line.
        block_hash: B256,
        /// Previous block number.
        prev_block_number: u64,
        /// Last irreversible block number.
        lib_num: u64,
        /// Block timestamp in Unix nanoseconds.
        timestamp_ns: u64,
        /// Decoded `sf.ethereum.type.v2.Block` protobuf payload.
        block: EthBlock,
    },
    /// A `FIRE BLOCK` line where `printed_flash_idx > 0` (flashblock partial).
    FlashBlock {
        /// Block number.
        block_number: u64,
        /// Block hash parsed from the FIRE BLOCK line.
        block_hash: B256,
        /// Logical flash block index (with the `+1000` sentinel stripped for `is_final`).
        flash_idx: u64,
        /// Whether this is the final partial for the block (`printed_flash_idx >= 1000`).
        is_final: bool,
        /// Previous block number.
        prev_block_number: u64,
        /// Last irreversible block number.
        lib_num: u64,
        /// Block timestamp in Unix nanoseconds.
        timestamp_ns: u64,
        /// Decoded `sf.ethereum.type.v2.Block` protobuf payload.
        block: EthBlock,
    },
}

impl FireEvent {
    /// Constructs an expected [`FireEvent::Block`] for metadata-only assertions.
    ///
    /// Sets `block` to `EthBlock::default()`. Use with [`assert_fire_events_metadata_eq`]
    /// to compare only protocol metadata fields without inspecting the payload.
    ///
    /// `block_hash` is compared exactly. `lib_num` and `timestamp_ns` default to `0` —
    /// treated as wildcards by the metadata comparison helper.
    pub(crate) fn canonical_block(block_number: u64, block_hash: B256) -> Self {
        Self::Block {
            block_number,
            block_hash,
            prev_block_number: if block_number > 0 { block_number - 1 } else { 0 },
            lib_num: 0,
            timestamp_ns: 0,
            block: EthBlock::default(),
        }
    }

    /// Constructs an expected [`FireEvent::FlashBlock`] for metadata-only assertions.
    ///
    /// Sets `block` to `EthBlock::default()`. Use with [`assert_fire_events_metadata_eq`].
    /// `block_hash` is compared exactly.
    pub(crate) fn flash_block(
        block_number: u64,
        block_hash: B256,
        flash_idx: u64,
        is_final: bool,
    ) -> Self {
        Self::FlashBlock {
            block_number,
            block_hash,
            flash_idx,
            is_final,
            prev_block_number: if block_number > 0 { block_number - 1 } else { 0 },
            lib_num: 0,
            timestamp_ns: 0,
            block: EthBlock::default(),
        }
    }
}

/// Asserts that `actual` events match `expected` ignoring the decoded block payload.
///
/// Uses relaxed matching for `lib_num` and `timestamp_ns` as well: a `0` in `expected`
/// matches any actual value. This is the preferred helper when a test only cares about
/// protocol metadata (block numbers, flash indices, finality flags).
///
/// Produces a clear diff via [`pretty_assertions`] on mismatch.
pub(crate) fn assert_fire_events_metadata_eq(actual: &[FireEvent], expected: &[FireEvent]) {
    let sentinel = FireEvent::Block {
        block_number: 0,
        block_hash: B256::ZERO,
        prev_block_number: 0,
        lib_num: 0,
        timestamp_ns: 0,
        block: EthBlock::default(),
    };
    let normalised: Vec<FireEvent> = actual
        .iter()
        .zip(expected.iter().chain(std::iter::repeat(&sentinel)))
        .map(|(a, e)| normalize_metadata(a, e))
        .collect();

    pretty_assertions::assert_eq!(normalised, expected);
}

/// Asserts that `actual` events exactly match `expected`, including the decoded block payload.
///
/// Uses relaxed matching for `lib_num` and `timestamp_ns` (a `0` in expected matches any
/// actual value) but compares `block` fields directly. Use this when the test needs to
/// verify that payload tracing is correct.
///
/// Produces a clear diff via [`pretty_assertions`] on mismatch.
pub(crate) fn assert_fire_events_eq(actual: &[FireEvent], expected: &[FireEvent]) {
    let sentinel = FireEvent::Block {
        block_number: 0,
        block_hash: B256::ZERO,
        prev_block_number: 0,
        lib_num: 0,
        timestamp_ns: 0,
        block: EthBlock::default(),
    };
    let normalised: Vec<FireEvent> = actual
        .iter()
        .zip(expected.iter().chain(std::iter::repeat(&sentinel)))
        .map(|(a, e)| normalize_full(a, e))
        .collect();

    pretty_assertions::assert_eq!(normalised, expected);
}

/// Normalises `actual` for metadata-only comparison against `expected`.
///
/// - Zeros `lib_num` and `timestamp_ns` when the corresponding field in `expected` is `0`.
/// - Replaces the `block` payload in the normalised copy with `expected`'s block, so the
///   payload is never compared.
fn normalize_metadata(actual: &FireEvent, expected: &FireEvent) -> FireEvent {
    match (actual, expected) {
        (
            FireEvent::Block {
                block_number, block_hash, prev_block_number, lib_num, timestamp_ns, ..
            },
            FireEvent::Block { lib_num: el, timestamp_ns: et, block: eb, .. },
        ) => FireEvent::Block {
            block_number: *block_number,
            block_hash: *block_hash,
            prev_block_number: *prev_block_number,
            lib_num: if *el == 0 { 0 } else { *lib_num },
            timestamp_ns: if *et == 0 { 0 } else { *timestamp_ns },
            block: eb.clone(),
        },
        (
            FireEvent::FlashBlock {
                block_number,
                block_hash,
                flash_idx,
                is_final,
                prev_block_number,
                lib_num,
                timestamp_ns,
                ..
            },
            FireEvent::FlashBlock { lib_num: el, timestamp_ns: et, block: eb, .. },
        ) => FireEvent::FlashBlock {
            block_number: *block_number,
            block_hash: *block_hash,
            flash_idx: *flash_idx,
            is_final: *is_final,
            prev_block_number: *prev_block_number,
            lib_num: if *el == 0 { 0 } else { *lib_num },
            timestamp_ns: if *et == 0 { 0 } else { *timestamp_ns },
            block: eb.clone(),
        },
        _ => actual.clone(),
    }
}

/// Normalises `actual` for full comparison against `expected`.
///
/// Zeros `lib_num` and `timestamp_ns` when the corresponding field in `expected` is `0`,
/// but keeps the actual `block` payload for comparison.
fn normalize_full(actual: &FireEvent, expected: &FireEvent) -> FireEvent {
    match (actual, expected) {
        (
            FireEvent::Block {
                block_number, block_hash, prev_block_number, lib_num, timestamp_ns, block,
            },
            FireEvent::Block { lib_num: el, timestamp_ns: et, .. },
        ) => FireEvent::Block {
            block_number: *block_number,
            block_hash: *block_hash,
            prev_block_number: *prev_block_number,
            lib_num: if *el == 0 { 0 } else { *lib_num },
            timestamp_ns: if *et == 0 { 0 } else { *timestamp_ns },
            block: block.clone(),
        },
        (
            FireEvent::FlashBlock {
                block_number,
                block_hash,
                flash_idx,
                is_final,
                prev_block_number,
                lib_num,
                timestamp_ns,
                block,
            },
            FireEvent::FlashBlock { lib_num: el, timestamp_ns: et, .. },
        ) => FireEvent::FlashBlock {
            block_number: *block_number,
            block_hash: *block_hash,
            flash_idx: *flash_idx,
            is_final: *is_final,
            prev_block_number: *prev_block_number,
            lib_num: if *el == 0 { 0 } else { *lib_num },
            timestamp_ns: if *et == 0 { 0 } else { *timestamp_ns },
            block: block.clone(),
        },
        _ => actual.clone(),
    }
}

// ── FIRE line parsing ─────────────────────────────────────────────────────────

/// Identifies which tracer emitted the FIRE BLOCK line currently being parsed. Set by
/// `# SOURCE FLASH` / `# SOURCE CANON` marker lines in the merged output produced by
/// [`run_flashblock_sequence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceTag {
    Canonical,
    Flash,
}

/// Parses a `0x`-optional hex string into a [`B256`], returning [`B256::ZERO`] on any
/// decode failure (so parsing never panics on malformed input).
fn parse_hex_b256(s: &str) -> B256 {
    let trimmed = s.strip_prefix("0x").unwrap_or(s);
    let mut buf = [0u8; 32];
    match hex::decode_to_slice(trimmed, &mut buf) {
        Ok(()) => B256::from(buf),
        Err(_) => B256::ZERO,
    }
}

/// Decodes a base64-encoded, prost-serialised `sf.ethereum.type.v2.Block` from the
/// `payload_base64` field of a `FIRE BLOCK` line.
///
/// Returns `EthBlock::default()` on any decode error (malformed base64 or invalid proto bytes)
/// rather than panicking, so that parsing never silently drops events.
fn decode_eth_block(payload_base64: &str) -> EthBlock {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(payload_base64) {
        Ok(b) => b,
        Err(_) => return EthBlock::default(),
    };
    EthBlock::decode(bytes.as_slice()).unwrap_or_default()
}

/// Parses all FIRE lines from raw tracer output and returns structured [`FireEvent`] values.
///
/// Recognised line prefixes:
/// - `# SOURCE FLASH` / `# SOURCE CANON` — synthetic marker emitted by
///   [`run_flashblock_sequence`] before each event's tracer output. The parser tracks the
///   current source and uses it to assign each subsequent `FIRE BLOCK` line to the right
///   [`FireEvent`] variant: flashblock-tracer lines (any `flash_idx`) become
///   [`FireEvent::FlashBlock`] (the base flashblock keeps `flash_idx == 0` rather than being
///   reinterpreted as a canonical block); canonical-tracer lines become [`FireEvent::Block`].
/// - `FIRE INIT <version> <node_name> <node_version>`
/// - `FIRE BLOCK <block_num> <flash_idx> <block_hash> <prev_num> <prev_hash> <lib_num>
///   <timestamp_ns> <payload_base64>`
///
/// The `payload_base64` field is decoded from base64 and deserialised via prost into an
/// [`EthBlock`] and stored on the returned [`FireEvent`] variants.
///
/// Lines that do not start with `FIRE` or `# SOURCE`, or that have malformed fields, are
/// silently skipped. If a `FIRE BLOCK` arrives before any source marker, it defaults to
/// canonical (so output from non-runner callers still parses sensibly).
pub(crate) fn parse_fire_events(raw: &[u8]) -> Vec<FireEvent> {
    let text = std::str::from_utf8(raw).unwrap_or("");
    let mut results = Vec::new();
    let mut source = SourceTag::Canonical;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# SOURCE ") {
            source = match rest.trim() {
                "FLASH" => SourceTag::Flash,
                "CANON" => SourceTag::Canonical,
                _ => source,
            };
            continue;
        }

        let mut parts = line.split(' ');

        let (Some(p0), Some(p1)) = (parts.next(), parts.next()) else { continue };
        if p0 != "FIRE" {
            continue;
        }

        match p1 {
            "INIT" => {
                // FIRE INIT <version> <node_name> <node_version>
                let Some(version) = parts.next() else { continue };
                let Some(node_name) = parts.next() else { continue };
                let Some(node_version) = parts.next() else { continue };
                results.push(FireEvent::Init {
                    version: version.to_owned(),
                    node_name: node_name.to_owned(),
                    node_version: node_version.to_owned(),
                });
            }
            "BLOCK" => {
                // FIRE BLOCK <block_num> <flash_idx> <block_hash> <prev_num> <prev_hash>
                //            <lib_num> <timestamp_ns> <payload_base64>
                let Some(block_num_s) = parts.next() else { continue };
                let Some(flash_idx_s) = parts.next() else { continue };
                let Some(block_hash_s) = parts.next() else { continue };
                let Some(prev_num_s) = parts.next() else { continue };
                let Some(_prev_hash_s) = parts.next() else { continue };
                let Some(lib_num_s) = parts.next() else { continue };
                let Some(timestamp_s) = parts.next() else { continue };
                let Some(payload_b64) = parts.next() else { continue };

                let Ok(block_number) = block_num_s.parse::<u64>() else { continue };
                let Ok(printed_flash_idx) = flash_idx_s.parse::<u64>() else { continue };
                let Ok(prev_block_number) = prev_num_s.parse::<u64>() else { continue };
                let Ok(lib_num) = lib_num_s.parse::<u64>() else { continue };
                let Ok(timestamp_ns) = timestamp_s.parse::<u64>() else { continue };
                let block_hash = parse_hex_b256(block_hash_s);

                let block = decode_eth_block(payload_b64);

                let (flash_idx, is_final) = if printed_flash_idx >= 1000 {
                    (printed_flash_idx - 1000, true)
                } else {
                    (printed_flash_idx, false)
                };

                match source {
                    SourceTag::Canonical => {
                        results.push(FireEvent::Block {
                            block_number,
                            block_hash,
                            prev_block_number,
                            lib_num,
                            timestamp_ns,
                            block,
                        });
                    }
                    SourceTag::Flash => {
                        results.push(FireEvent::FlashBlock {
                            block_number,
                            block_hash,
                            flash_idx,
                            is_final,
                            prev_block_number,
                            lib_num,
                            timestamp_ns,
                            block,
                        });
                    }
                }
            }
            _ => {} // skip unknown FIRE sub-commands
        }
    }

    results
}

// ── Test genesis ─────────────────────────────────────────────────────────────

/// Default genesis used by all tests: chain 8453 (Base mainnet chain id), no Isthmus.
///
/// Isthmus is pushed far into the future to avoid pre-execution changes that require
/// contract deployments not present in the empty genesis.
pub(crate) fn test_genesis() -> Genesis {
    serde_json::from_value(json!({
        "config": {
            "chainId": 8453,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "mergeNetsplitBlock": 0,
            "terminalTotalDifficulty": 0,
            "bedrockBlock": 0,
            "regolithTime": 0,
            "canyonTime": 0,
            "ecotoneTime": 0,
            "fjordTime": 0,
            "graniteTime": 0,
            "holoceneTime": 0,
            "isthmusTime": 9999999999u64,
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
        },
        "alloc": {
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
                "balance": "0x3635c9adc5dea00000"
            }
        },
        "baseFeePerGas": "7",
        "difficulty": "0",
        "gasLimit": "30000000",
        "timestamp": "0x67d00000",
        "number": "0"
    }))
    .expect("test genesis JSON must parse")
}

// ── Test runner ──────────────────────────────────────────────────────────────

/// Drives a [`FirehoseFlashblocksProcessor`] with a buffer-backed tracer through `events` in
/// strict order, and returns the captured raw tracer output.
///
/// Events are processed sequentially without any WebSocket indirection:
///
/// - [`TestEvent::Flashblock`] — calls `processor.on_flashblock_received` directly so that
///   FIRE BLOCK lines are emitted by the **flashblock tracer**.
/// - [`TestEvent::CanonicalBlock`] — marks the block available in [`GenesisClient`] *and*
///   emits a canonical (non-flash) FIRE BLOCK line through a dedicated **canonical tracer**.
///   This simulates the live-node behaviour where the global `FirehoseExtension` `ExEx` emits
///   a canonical FIRE BLOCK whenever a block is finalised by the engine, independent of the
///   flashblock tracer.
///
/// In live operation the two tracers are distinct Firehose streams, each with its own
/// `FIRE INIT` header, so `flash_idx=0` is unambiguous downstream — it means "canonical" on
/// the canonical stream and "base flashblock" on the flashblock stream. In tests, both tracers
/// share an output sink, so we tag each event's output with a `# SOURCE FLASH` or
/// `# SOURCE CANON` marker line. [`parse_fire_events`] consumes these markers to assign each
/// emitted FIRE BLOCK to the correct [`FireEvent`] variant:
///
/// - Flashblock-tracer line (any `flash_idx`) → [`FireEvent::FlashBlock`]
///   (including the base flashblock at `flash_idx == 0`).
/// - Canonical-tracer line → [`FireEvent::Block`].
pub(crate) fn run_flashblock_sequence(client: GenesisClient, events: Vec<TestEvent>) -> Vec<u8> {
    let flash_buffer = InMemoryBuffer::new();
    let canonical_buffer = InMemoryBuffer::new();
    let mut output: Vec<u8> = Vec::new();
    let chain_id = client.chain_spec().chain().id();

    // Flashblock tracer — drives `FirehoseFlashblocksProcessor`.
    let flash_writer: Box<dyn std::io::Write + Send> = Box::new(flash_buffer.clone());
    let tracer_handle = FlashblocksTracerHandle::with_writer(
        Config { chain_client: ChainClient::Reth, ..Default::default() },
        ChainConfig::new(chain_id),
        flash_writer,
    );

    // Canonical tracer — emits non-flash FIRE BLOCK lines for `CanonicalBlock` events.
    let canonical_writer: Box<dyn std::io::Write + Send> = Box::new(canonical_buffer.clone());
    let mut canonical_tracer = FlashblocksTracerHandle::with_writer(
        Config { chain_client: ChainClient::Reth, ..Default::default() },
        ChainConfig::new(chain_id),
        canonical_writer,
    );

    let processor = FirehoseFlashblocksProcessor::new(client.clone(), tracer_handle);

    // Track how much of each per-tracer buffer we've already flushed to `output`, so that
    // each event's emissions are tagged with its source marker in the merged stream.
    let mut flash_offset = 0usize;
    let mut canonical_offset = 0usize;

    for event in events {
        match event {
            TestEvent::Flashblock(fb) => {
                processor.on_flashblock_received(*fb);

                let bytes = flash_buffer.get_bytes();
                if bytes.len() > flash_offset {
                    output.extend_from_slice(b"# SOURCE FLASH\n");
                    output.extend_from_slice(&bytes[flash_offset..]);
                    flash_offset = bytes.len();
                }
            }
            TestEvent::CanonicalBlock { block_number, block_hash } => {
                // Make the block available to the provider so that subsequent flashblocks
                // that need to bootstrap from block N can find it.
                client.mark_canonical_block_available(block_number);

                // Emit a canonical FIRE BLOCK to simulate the global ExEx tracer emitting the
                // finalised block. A minimal block with the correct number is sufficient; the
                // test assertions use metadata-only comparisons.
                let header = client.header_for_block(block_number);
                let sealed = SealedBlock::new_unchecked(
                    alloy_consensus::Block {
                        header,
                        body: BlockBody::<BaseTxEnvelope>::default(),
                    },
                    block_hash,
                );
                let tracer = canonical_tracer.tracer_mut();
                let block_tracer =
                    FirehoseBlockTracer::start_local::<BasePrimitives>(tracer, &sealed, None);
                block_tracer.mark_verified();

                let bytes = canonical_buffer.get_bytes();
                if bytes.len() > canonical_offset {
                    output.extend_from_slice(b"# SOURCE CANON\n");
                    output.extend_from_slice(&bytes[canonical_offset..]);
                    canonical_offset = bytes.len();
                }
            }
        }
    }

    output
}
