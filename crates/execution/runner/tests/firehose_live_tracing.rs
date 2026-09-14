//! Regression test for Firehose tracing on the engine-tree live-block path.
//!
//! Blocks that a node has to *execute itself* when they arrive via `engine_newPayload` are routed
//! into the Firehose tracer by reth's engine validator (`validate_block_with_state` →
//! `execute_and_trace_block`), which installs the OP Stack hooks Base's EVM config selects through
//! `reth_firehose::FirehoseLiveHooks`. Those hooks have been dropped during upstream merges before,
//! silently disabling live-block tracing (or its OP-specific events) while the historical/stage
//! path kept working — a regression that compiled and passed every existing test.
//!
//! ## Why two nodes
//!
//! A node that *builds* a block (the sequencer flow) inserts it into its tree as already-executed
//! (`InsertExecutedBlock`), so a subsequent `engine_newPayload` for that same block short-circuits
//! and never re-runs `validate_block_with_state` — the traced path is skipped. The live path is
//! only exercised by a node that did **not** build the block: a follower receiving payloads from a
//! sequencer. So this test runs two nodes — a `sequencer` that builds payloads and a `follower`
//! that executes them via `engine_newPayload` — and asserts the follower emits `FIRE BLOCK` lines.
//! If the dispatch into `execute_and_trace_block` is missing, no `FIRE BLOCK` lines are produced and
//! the test fails.
//!
//! ## OP Stack hooks
//!
//! Each block carries the L1-info deposit and a transfer from a pre-funded account. The test checks
//! the two events only the OP hooks produce: the deposit's nonce (deposit envelopes carry none, so
//! without `OpPreTxAdjust` every deposit reports nonce 0) and the `BaseFeeVault` fee credit on the
//! transfer (applied outside the EVM journal, so without `OpPostTxExtras` it is missing).
//!
//! It lives in its own integration-test binary because it installs a process-wide tracer;
//! cargo/nextest run each integration binary in its own process, keeping the global tracer isolated
//! from the rest of the suite. The tracer is global, but only the follower's
//! `validate_block_with_state` execution feeds it — building on the sequencer does not trace.

use std::{sync::Arc, time::Duration};

use alloy_eips::eip7685::Requests;
use alloy_primitives::{Address, B64, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use base_common_consensus::{BaseTxEnvelope, Predeploys};
use base_common_rpc_types::BaseTransactionRequest;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_node_runner::test_utils::{
    BLOCK_BUILD_DELAY_MS, BLOCK_TIME_SECONDS, GAS_LIMIT, L1_BLOCK_INFO_DEPOSIT_TX, LocalNode,
    NODE_STARTUP_DELAY_MS,
};
use base_firehose_tests::BaseFirehoseCapture;
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis};
use firehose_tracer::pb::sf::ethereum::r#type::v2::{
    balance_change::Reason, transaction_trace::Type as TrxType,
};
use eyre::{Result, eyre};
use reth_chainspec::EthChainSpec;
use reth_provider::ChainSpecProvider;
use tokio::time::sleep;

/// Number of blocks to advance. All of them (block 1 included — the genesis block itself is
/// emitted separately at node startup by the Firehose ExEx, not through this path) exercise the
/// live `execute_and_trace_block` path on the follower.
const PRODUCED_BLOCKS: u64 = 3;

/// Base EIP-1559 base-fee params (`eip1559Denominator` / `eip1559Elasticity`) matching the
/// `optimism` section of [`build_test_genesis`].
const EIP1559_DENOMINATOR: u32 = 50;
const EIP1559_ELASTICITY: u32 = 6;
/// Jovian minimum base fee, matching the genesis `base_fee_per_gas` from [`build_test_genesis`].
const MIN_BASE_FEE: u64 = 1_000_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn live_payload_validation_emits_firehose_blocks() -> Result<()> {
    // Install a buffer-backed global Firehose tracer BEFORE any block is validated, so the live
    // path's `is_tracer_initialized()` gate activates and routes execution through
    // `execute_and_trace_block`. The chain id matches the test genesis; the fork timestamps only
    // affect how block contents are mapped, not whether a block is emitted.
    let capture = BaseFirehoseCapture::install(
        DEVNET_CHAIN_ID,
        Some(0), // shanghai / canyon
        Some(0), // cancun / ecotone
        None,    // prague
    );

    // `build_test_genesis` enables Jovian at genesis but leaves `extraData` as a single zero byte.
    // From Holocene on, the EIP-1559 base-fee parameters live in the block's `extraData`, and from
    // Jovian on the encoding is the version-1 form `0x01 || denominator(u32 BE) || elasticity(u32
    // BE) || min_base_fee(u64 BE)`. A fresh follower derives a block's base fee from its parent's
    // `extraData`, so the genesis must carry a well-formed version-1 header — otherwise the builder
    // cannot read the min base fee (base fee collapses to 0) and the follower fails
    // `validate_header_base_fee` ("base fee missing").
    let mut genesis = build_test_genesis();
    let mut extra_data = vec![1u8];
    extra_data.extend_from_slice(&EIP1559_DENOMINATOR.to_be_bytes());
    extra_data.extend_from_slice(&EIP1559_ELASTICITY.to_be_bytes());
    extra_data.extend_from_slice(&MIN_BASE_FEE.to_be_bytes());
    genesis.extra_data = Bytes::from(extra_data);
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));

    // Two nodes on the same genesis: the sequencer builds payloads; the follower executes them via
    // `engine_newPayload` (the path under test). Building does not trace; only the follower's
    // `validate_block_with_state` execution feeds the global tracer.
    let sequencer = LocalNode::new(vec![], chain_spec.clone()).await?;
    let follower = LocalNode::new(vec![], chain_spec.clone()).await?;
    sleep(Duration::from_millis(NODE_STARTUP_DELAY_MS)).await;

    let seq_engine = sequencer.engine_api()?;
    let fol_engine = follower.engine_api()?;
    let spec = sequencer.blockchain_provider().chain_spec();

    // Bootstrap both nodes by pointing their forkchoice at the genesis head.
    let genesis_hash = sequencer
        .provider()?
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre!("no genesis block"))?
        .header
        .hash;
    seq_engine.update_forkchoice(genesis_hash, genesis_hash, None).await?;
    fol_engine.update_forkchoice(genesis_hash, genesis_hash, None).await?;

    for block_index in 0..PRODUCED_BLOCKS {
        // Use the sequencer head as the parent for the next block.
        let parent = sequencer
            .provider()?
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("no head block on sequencer"))?;

        let parent_hash = parent.header.hash;
        let parent_beacon_block_root = parent.header.parent_beacon_block_root.unwrap_or(B256::ZERO);
        let next_timestamp = parent.header.timestamp + BLOCK_TIME_SECONDS;
        let min_base_fee = parent.header.base_fee_per_gas.unwrap_or_default();
        let base_fee_params = spec.base_fee_params_at_timestamp(next_timestamp);
        let eip_1559_params = ((base_fee_params.max_change_denominator as u64) << 32)
            | (base_fee_params.elasticity_multiplier as u64);

        let (transfer, _) = Account::Alice.sign_txn_request(
            BaseTransactionRequest::default()
                .to(Account::Bob.address())
                .value(U256::from(1))
                .nonce(block_index),
        )?;

        let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope>::try_new(
            parent_hash,
            BasePayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp: next_timestamp,
                    parent_beacon_block_root: Some(parent_beacon_block_root),
                    withdrawals: Some(vec![]),
                    slot_number: None,
                    ..Default::default()
                },
                transactions: Some(vec![L1_BLOCK_INFO_DEPOSIT_TX, transfer]),
                gas_limit: Some(GAS_LIMIT),
                no_tx_pool: Some(true),
                min_base_fee: Some(min_base_fee),
                eip_1559_params: Some(B64::from(eip_1559_params)),
            },
            3,
        )?;

        // Sequencer builds the payload (this does NOT trace — it builds, it does not validate).
        let payload_id = seq_engine
            .update_forkchoice(parent_hash, parent_hash, Some(attributes))
            .await?
            .payload_id
            .ok_or_else(|| eyre!("sequencer forkchoice update returned no payload id"))?;

        sleep(Duration::from_millis(BLOCK_BUILD_DELAY_MS)).await;

        let envelope = seq_engine.get_payload_v4(payload_id).await?;
        let execution_payload = envelope.execution_payload;
        let execution_requests: Vec<Bytes> = envelope.execution_requests;
        let execution_requests = if execution_requests.is_empty() {
            Requests::default()
        } else {
            Requests::new(execution_requests)
        };

        // Follower validates the externally-produced payload via `engine_newPayload`. Since the
        // follower did not build it, this drives `validate_block_with_state` →
        // `execute_and_trace_block` — the traced path under test.
        let status = fol_engine
            .new_payload(execution_payload, vec![], parent_beacon_block_root, execution_requests)
            .await?;
        assert!(!status.status.is_invalid(), "follower rejected payload: {status:?}");

        let new_block_hash = status
            .latest_valid_hash
            .ok_or_else(|| eyre!("follower payload status missing latest_valid_hash"))?;

        // Advance both heads to the new block so the next iteration builds/validates on top of it.
        seq_engine.update_forkchoice(parent_hash, new_block_hash, None).await?;
        fol_engine.update_forkchoice(parent_hash, new_block_hash, None).await?;
    }

    let traced = capture.traced_block_numbers();
    assert!(
        !traced.is_empty(),
        "no FIRE BLOCK lines were emitted — the follower's live payload-validation path is not \
         traced.\nCaptured tracer output:\n{}",
        capture.raw_text()
    );

    // Every produced block goes through the live `execute_and_trace_block` path. Require each to
    // have been traced exactly once.
    for number in 1..=PRODUCED_BLOCKS {
        let count = traced.iter().filter(|traced| **traced == number).count();
        assert_eq!(count, 1, "expected one FIRE BLOCK line for live block #{number}, got {traced:?}");
    }

    for block in capture.blocks()? {
        let [deposit, transfer] = block.transaction_traces.as_slice() else {
            panic!(
                "block #{} should hold the L1-info deposit and one transfer, got {} traces",
                block.number,
                block.transaction_traces.len()
            );
        };

        // `OpPreTxAdjust`: the depositor sends one deposit per block, so its pre-execution nonce
        // is the parent block number.
        assert_eq!(deposit.r#type, TrxType::TrxTypeOptimismDeposit as i32);
        assert_eq!(
            deposit.nonce,
            block.number - 1,
            "deposit nonce in block #{} does not come from the depositor account",
            block.number
        );

        // `OpPostTxExtras`: the base fee paid by the transfer is credited to the `BaseFeeVault`.
        let base_fee_vault_credits: Vec<_> = transfer
            .calls
            .iter()
            .flat_map(|call| &call.balance_changes)
            .filter(|change| {
                change.reason == Reason::RewardTransactionFee as i32 &&
                    Address::from_slice(&change.address) == Predeploys::BASE_FEE_VAULT
            })
            .collect();
        assert_eq!(
            base_fee_vault_credits.len(),
            1,
            "transfer in block #{} should credit the BaseFeeVault exactly once",
            block.number
        );
    }

    Ok(())
}
