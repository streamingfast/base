//! Regression test for Firehose tracing on the engine-tree live-block path.
//!
//! Base's live-path Firehose hooks live in `base_engine_tree`'s payload validator
//! (`validate_block_with_state` → `execute_and_trace_block`) and are wired into the node through
//! [`base_node_runner::BaseNode`]'s `BaseEngineValidatorBuilder` add-on. Blocks that a node has to
//! *execute itself* when they arrive via `engine_newPayload` are routed into the Firehose tracer
//! there. Those hooks have been dropped during upstream merges before, silently disabling
//! live-block tracing while the historical/stage path kept working — a regression that compiled and
//! passed every existing test.
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
//! It lives in its own integration-test binary because it installs a process-wide tracer;
//! cargo/nextest run each integration binary in its own process, keeping the global tracer isolated
//! from the rest of the suite. The tracer is global, but only the follower's
//! `validate_block_with_state` execution feeds it — building on the sequencer does not trace.

use std::{sync::Arc, time::Duration};

use alloy_eips::eip7685::Requests;
use alloy_primitives::{B64, B256, Bytes};
use alloy_provider::Provider;
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_node_runner::test_utils::{
    BLOCK_BUILD_DELAY_MS, BLOCK_TIME_SECONDS, GAS_LIMIT, L1_BLOCK_INFO_DEPOSIT_TX, LocalNode,
    NODE_STARTUP_DELAY_MS,
};
use base_test_utils::{DEVNET_CHAIN_ID, build_test_genesis};
use eyre::{Result, eyre};
use reth_chainspec::EthChainSpec;
use reth_provider::ChainSpecProvider;
use tokio::time::sleep;

/// Number of blocks to advance. Block 1 is the Firehose genesis marker (emitted via
/// `on_genesis_block`); blocks 2.. exercise the live `execute_and_trace_block` path on the follower.
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
    let buffer = reth_firehose::init_tracer_with_buffer(
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

    for _ in 0..PRODUCED_BLOCKS {
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
                transactions: Some(vec![L1_BLOCK_INFO_DEPOSIT_TX]),
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

    // Collect the block numbers from every captured `FIRE BLOCK <num> ...` line.
    let raw = buffer.get_bytes();
    let text = String::from_utf8(raw).expect("captured tracer output is UTF-8");
    let traced: Vec<u64> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            if parts.next()? != "FIRE" || parts.next()? != "BLOCK" {
                return None;
            }
            parts.next()?.parse::<u64>().ok()
        })
        .collect();

    assert!(
        !traced.is_empty(),
        "no FIRE BLOCK lines were emitted — the follower's live payload-validation path is not \
         traced.\nCaptured tracer output:\n{text}"
    );

    // Blocks 2..=PRODUCED_BLOCKS go through the live `execute_and_trace_block` path (block 1 is the
    // genesis marker, emitted separately via `on_genesis_block`). Require each to have been traced.
    for number in 2..=PRODUCED_BLOCKS {
        assert!(
            traced.contains(&number),
            "expected a FIRE BLOCK line for live block #{number}, got traced blocks {traced:?}"
        );
    }

    Ok(())
}
