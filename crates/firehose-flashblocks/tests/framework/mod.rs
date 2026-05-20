//! Test framework for firehose flashblocks integration tests.
//!
//! This module provides:
//! - [`GenesisClient`]: a minimal in-memory reth provider mock backed by a genesis allocation.
//! - [`GenesisStateProvider`]: a thin [`StateProvider`] wrapper around [`StateProviderTest`].
//! - [`flash_base`] / [`flash_delta`]: helpers to build [`Flashblock`] fixtures.
//! - [`ws_server_once`]: a transient WebSocket server that sends a sequence of messages once.
//! - [`ParsedFireBlock`]: structured parsed representation of a `FIRE BLOCK` line.
//! - [`parse_fire_blocks`]: parses all `FIRE BLOCK` lines from raw tracer output.
//! - [`run_flashblock_sequence`]: end-to-end harness — starts server, runs processor, returns output.

use std::{
    net::SocketAddr,
    ops::RangeInclusive,
    sync::Arc,
    time::Duration,
};

use alloy_consensus::Header;
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy_genesis::Genesis;
use alloy_primitives::{
    Address, BlockHash, BlockNumber, Bloom, Bytes, StorageKey, TxHash, TxNumber, B256, U256,
};
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::{
    BaseBlock, BasePrimitives, BaseReceipt, BaseTxEnvelope,
};
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblocksPayloadV1,
    Metadata,
};
use base_execution_chainspec::BaseChainSpec;
use base_firehose_flashblocks::{
    FirehoseFlashblocksProcessor, FlashblocksTracerHandle,
};
use firehose_tracer::{
    InMemoryBuffer,
    config::{ChainClient, ChainConfig, Config},
};
use futures::SinkExt as _;
use reth_chainspec::{ChainInfo, ChainSpecProvider, EthChainSpec};
use reth_db_models::StoredBlockBodyIndices;
use reth_primitives_traits::{Account, RecoveredBlock, SealedHeader};
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
use tokio::net::TcpListener;
use tokio_tungstenite::{
    accept_async,
    tungstenite::{Message, Utf8Bytes},
};
use url::Url;

// ── Mock client ──────────────────────────────────────────────────────────────

/// A minimal in-memory client for use in tests.
///
/// Holds a genesis (used to seed account state and chain spec) and a pre-built genesis header.
/// Only the three methods called by [`FirehoseFlashblocksProcessor`] are implemented; all others
/// return `Ok(None)` / `Ok(Vec::new())` or delegate to [`StateProviderTest`].
#[derive(Clone, Debug)]
pub(crate) struct GenesisClient {
    /// The chain spec derived from the genesis.
    pub(crate) chain_spec: Arc<BaseChainSpec>,
    /// The raw genesis used to seed account state.
    pub(crate) genesis: Genesis,
    /// The genesis block header computed from the genesis.
    pub(crate) genesis_header: Header,
}

impl GenesisClient {
    /// Constructs a [`GenesisClient`] from a genesis allocation.
    pub(crate) fn new(genesis: Genesis) -> Self {
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis.clone()));
        let genesis_header =
            reth_chainspec::make_genesis_header(&genesis, &chain_spec.inner.hardforks);
        Self { chain_spec, genesis, genesis_header }
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

    fn header_by_number(&self, _n: u64) -> ProviderResult<Option<Header>> {
        // The processor uses this to build the EVM env for block N; always return genesis.
        Ok(Some(self.genesis_header.clone()))
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
        _n: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(GenesisStateProvider::new(&self.genesis)))
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

// ── Flashblock builders ──────────────────────────────────────────────────────

/// Constructs a base flashblock (index 0) for the given block number.
///
/// All optional fields use sensible defaults. The base flashblock carries no transactions.
pub(crate) fn flash_base(
    block_number: u64,
    parent_hash: B256,
    timestamp: u64,
) -> Flashblock {
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
        block_hash: B256::ZERO,
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

    Flashblock {
        payload_id: payload.payload_id,
        index: payload.index,
        base: payload.base,
        diff: payload.diff,
        metadata,
    }
}

/// Constructs a delta flashblock (index > 0) for the given block number.
///
/// Carries no new transactions; used to test sequence progression.
pub(crate) fn flash_delta(block_number: u64, index: u64) -> Flashblock {
    let diff = ExecutionPayloadFlashblockDeltaV1 {
        state_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::default(),
        gas_used: 0,
        block_hash: B256::ZERO,
        transactions: Vec::new(),
        withdrawals: Vec::new(),
        withdrawals_root: B256::ZERO,
        blob_gas_used: Some(0),
    };

    let metadata = Metadata { block_number };

    Flashblock {
        payload_id: PayloadId::new([0u8; 8]),
        index,
        base: None,
        diff,
        metadata,
    }
}

// ── WS server ────────────────────────────────────────────────────────────────

/// Spins up a transient WebSocket server that sends each flashblock from `sequence` to the
/// first client that connects, then closes the connection.
///
/// Returns the server's [`SocketAddr`] so the test can pass it to the subscriber.
pub(crate) async fn ws_server_once(sequence: Vec<Flashblock>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        for fb in &sequence {
            let payload = FlashblocksPayloadV1 {
                payload_id: fb.payload_id,
                index: fb.index,
                base: fb.base.clone(),
                diff: fb.diff.clone(),
                metadata: json!({ "block_number": fb.metadata.block_number }),
            };
            let json = serde_json::to_string(&payload).unwrap();
            ws.send(Message::Text(Utf8Bytes::from(json))).await.unwrap();
        }

        // Close gracefully after all messages are sent.
        let _ = ws.send(Message::Close(None)).await;
    });

    addr
}

// ── FIRE BLOCK parsing ───────────────────────────────────────────────────────

/// A parsed representation of a single `FIRE BLOCK` line.
///
/// The FIRE 3.1 protocol format is:
/// ```text
/// FIRE BLOCK <block_num> <flash_block_idx> <block_hash> <prev_num> <prev_hash> <lib_num> <timestamp_unix_nano> <payload_base64>
/// ```
///
/// `flash_block_idx` is 0 for non-flash blocks; for flash blocks it equals the current flash
/// block index, and equals `idx + 1000` when `is_final = true` (the final partial for this block).
/// This convention is defined in `firehose_tracer::printer::compute_printed_flash_block_index`.
#[derive(Debug)]
pub(crate) struct ParsedFireBlock {
    /// Block number as printed on the FIRE BLOCK line.
    pub(crate) block_number: u64,
    /// Raw printed flash block index from the FIRE BLOCK line.
    ///
    /// - `0` for a non-flash (canonical) block.
    /// - `idx` for a partial flashblock where `is_final = false`.
    /// - `idx + 1000` for the final partial flashblock where `is_final = true`.
    pub(crate) printed_flash_idx: u64,
    /// Block hash (hex, no `0x` prefix).
    pub(crate) block_hash: String,
    /// Previous block number.
    pub(crate) prev_block_number: u64,
    /// Previous block hash (hex, no `0x` prefix).
    pub(crate) prev_block_hash: String,
    /// Last irreversible block number.
    pub(crate) lib_num: u64,
    /// Block timestamp in Unix nanoseconds.
    pub(crate) timestamp_ns: u64,
}

impl ParsedFireBlock {
    /// Returns the logical flash block index (stripping the `+1000` final-block sentinel).
    ///
    /// Returns `None` if this is a non-flash (canonical) block (`printed_flash_idx == 0`).
    pub(crate) const fn flash_idx(&self) -> Option<u64> {
        if self.printed_flash_idx == 0 {
            None
        } else if self.printed_flash_idx >= 1000 {
            Some(self.printed_flash_idx - 1000)
        } else {
            Some(self.printed_flash_idx)
        }
    }

    /// Returns `true` when this is the final partial for a canonical block (`is_final = true`).
    ///
    /// Determined by the `+1000` sentinel: `printed_flash_idx >= 1000`.
    pub(crate) const fn is_final(&self) -> bool {
        self.printed_flash_idx >= 1000
    }
}

/// Parses all `FIRE BLOCK` lines from raw tracer output and returns structured representations.
///
/// Lines that do not start with `FIRE BLOCK` or that have malformed fields are silently skipped.
pub(crate) fn parse_fire_blocks(raw: &[u8]) -> Vec<ParsedFireBlock> {
    let text = std::str::from_utf8(raw).unwrap_or("");
    let mut results = Vec::new();

    for line in text.lines() {
        let mut parts = line.split(' ');

        // Match "FIRE BLOCK"
        let (Some(p0), Some(p1)) = (parts.next(), parts.next()) else { continue };
        if p0 != "FIRE" || p1 != "BLOCK" {
            continue;
        }

        // Parse: block_num flash_idx block_hash prev_num prev_hash lib_num timestamp_ns payload
        let Some(block_num_s) = parts.next() else { continue };
        let Some(flash_idx_s) = parts.next() else { continue };
        let Some(block_hash_s) = parts.next() else { continue };
        let Some(prev_num_s) = parts.next() else { continue };
        let Some(prev_hash_s) = parts.next() else { continue };
        let Some(lib_num_s) = parts.next() else { continue };
        let Some(timestamp_s) = parts.next() else { continue };

        let Ok(block_number) = block_num_s.parse::<u64>() else { continue };
        let Ok(printed_flash_idx) = flash_idx_s.parse::<u64>() else { continue };
        let Ok(prev_block_number) = prev_num_s.parse::<u64>() else { continue };
        let Ok(lib_num) = lib_num_s.parse::<u64>() else { continue };
        let Ok(timestamp_ns) = timestamp_s.parse::<u64>() else { continue };

        results.push(ParsedFireBlock {
            block_number,
            printed_flash_idx,
            block_hash: block_hash_s.to_owned(),
            prev_block_number,
            prev_block_hash: prev_hash_s.to_owned(),
            lib_num,
            timestamp_ns,
        });
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

/// Builds a [`FirehoseFlashblocksProcessor`] with a buffer-backed tracer, connects it to a
/// one-shot WebSocket server serving `sequence`, waits briefly for processing, and returns
/// the captured raw tracer output.
pub(crate) async fn run_flashblock_sequence(
    client: GenesisClient,
    sequence: Vec<Flashblock>,
) -> Vec<u8> {
    let addr = ws_server_once(sequence).await;
    let ws_url = Url::parse(&format!("ws://127.0.0.1:{}", addr.port())).unwrap();

    let buffer = InMemoryBuffer::new();
    let writer: Box<dyn std::io::Write + Send> = Box::new(buffer.clone());
    let chain_id = client.chain_spec().chain().id();

    let tracer_handle = FlashblocksTracerHandle::with_writer(
        Config { chain_client: ChainClient::Reth, ..Default::default() },
        ChainConfig::new(chain_id),
        writer,
    );

    let processor = Arc::new(FirehoseFlashblocksProcessor::new(client, tracer_handle));

    // Use the subscriber to drive the processor via the live WS path.
    let mut subscriber =
        base_flashblocks::FlashblocksSubscriber::new(Arc::clone(&processor), ws_url);
    subscriber.start();

    // Allow time for the WS server to send all messages and the processor to handle them.
    tokio::time::sleep(Duration::from_millis(500)).await;

    buffer.get_bytes()
}
