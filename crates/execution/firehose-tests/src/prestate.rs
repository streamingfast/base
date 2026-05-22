//! Prestate-driven Firehose block runner for Base / OP Stack chains.
//!
//! Layout of a test case folder:
//!
//! ```text
//! tests/cases/<case>/
//!   prestate.json               # genesis + context + EIP-2718-encoded signed tx
//!   block.<num>.binpb           # expected Firehose Block (binary protobuf, golden)
//! ```

use std::{path::Path, sync::Arc};

use alloy_consensus::{
    Header,
    constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH},
    proofs::calculate_transaction_root,
};
use alloy_eips::{eip2718::Decodable2718, eip4895::Withdrawals};
use alloy_primitives::B256;
use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_firehose::{OpPostTxExtras, OpPreTxAdjust};
use eyre::Context;
use reth_firehose::{FirehoseBlockTracer, run_wrapped_block};
use reth_firehose_tests::{
    RunOutcome, TraceContext, decode_hex, parse_fire_block_for, seed_cache_db,
};
use reth_primitives_traits::{Block as _, RecoveredBlock};
use reth_revm::State;
use revm::database::{CacheDB, EmptyDB};

/// Run the prestate-driven Firehose harness against `case_folder` and return the captured Block.
///
/// `case_folder` must contain `prestate.json` (Base/OP-style genesis + block context + EIP-2718
/// encoded signed transaction). The captured `Block` is the protobuf parsed from the single
/// `FIRE BLOCK` line emitted for the executed block; assert against a `.binpb` golden via
/// [`assert_block_equals_golden`].
pub fn run_prestate(case_folder: &Path) -> eyre::Result<RunOutcome> {
    use alloy_consensus::Block;
    use base_common_consensus::BaseBlockBody;
    use reth_firehose_tests::Prestate;

    let prestate_path = case_folder.join("prestate.json");
    let prestate: Prestate = serde_json::from_slice(&std::fs::read(&prestate_path)?)
        .with_context(|| format!("reading prestate.json at {}", prestate_path.display()))?;

    let chain_spec = Arc::new(BaseChainSpec::from_genesis(prestate.genesis.clone()));
    let parent_hash = chain_spec.inner.genesis_hash();

    let tx_bytes = decode_hex(&prestate.input).context("decoding prestate.input hex")?;
    let signed_tx = BaseTransactionSigned::decode_2718(&mut tx_bytes.as_slice())
        .context("EIP-2718-decoding prestate.input as a signed transaction")?;

    let transactions = vec![signed_tx];
    let header = build_op_header(&prestate.context, parent_hash, &transactions);

    // OP Stack post-Canyon blocks always carry a withdrawals list (may be empty).
    // An absent slot would change the header keccak vs Geth's representation.
    let block = Block {
        header,
        body: BaseBlockBody {
            transactions,
            ommers: vec![],
            withdrawals: Some(Withdrawals::default()),
        },
    };

    let recovered: RecoveredBlock<Block<BaseTransactionSigned>> =
        block.try_into_recovered().map_err(|_| eyre::eyre!("recovering tx senders"))?;

    let mut db = CacheDB::new(EmptyDB::default());
    seed_cache_db(&mut db, &prestate.genesis)?;
    let mut state = State::builder().with_database(db).with_bundle_update().build();

    let evm_config = BaseEvmConfig::base(chain_spec);

    let (mut tracer, buffer) = firehose_tracer::Tracer::with_buffer(
        firehose_tracer::config::Config::default(),
        firehose_tracer::config::ChainConfig::from_genesis(&prestate.genesis),
        "base-firehose-tests",
        env!("CARGO_PKG_VERSION"),
    );

    let block_number = recovered.header().number;
    let block_hash = recovered.hash();
    let finalized = Some(firehose_tracer::types::FinalizedBlockRef {
        number: block_number,
        hash: Some(block_hash),
    });

    let mut block_tracer = FirehoseBlockTracer::start_local::<BasePrimitives>(
        &mut tracer,
        recovered.sealed_block(),
        finalized,
    );

    let exec_result = run_wrapped_block::<_, _, _, _, _>(
        &evm_config,
        &mut state,
        &recovered,
        &mut block_tracer,
        OpPreTxAdjust,
        OpPostTxExtras,
    );

    match exec_result {
        Ok(_) => block_tracer.mark_verified(),
        Err(e) => {
            let msg = e.to_string();
            block_tracer.mark_failed(&std::io::Error::other(msg.clone()));
            return Err(eyre::eyre!("block execution failed: {msg}"));
        }
    }

    drop(tracer);

    let raw = buffer.get_bytes();
    let block = parse_fire_block_for(&raw, block_number)?;
    Ok(RunOutcome { block, raw })
}

/// Build an OP Stack block header from the JSON test context.
fn build_op_header(
    ctx: &TraceContext,
    parent_hash: B256,
    transactions: &[BaseTransactionSigned],
) -> Header {
    Header {
        parent_hash,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        state_root: B256::ZERO,
        transactions_root: calculate_transaction_root(transactions),
        receipts_root: EMPTY_ROOT_HASH,
        // OP Stack blocks post-Canyon always carry withdrawals_root.
        withdrawals_root: Some(EMPTY_ROOT_HASH),
        number: ctx.number,
        timestamp: ctx.timestamp,
        gas_limit: ctx.gas_limit,
        gas_used: 0,
        beneficiary: ctx.miner,
        base_fee_per_gas: ctx.base_fee_per_gas.map(|v| v as u64),
        difficulty: ctx.difficulty.unwrap_or_default(),
        parent_beacon_block_root: Some(B256::ZERO),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        requests_hash: None,
        ..Default::default()
    }
}

pub use reth_firehose_tests::assert_block_equals_golden;
