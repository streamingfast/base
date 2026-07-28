//! Firehose tracing regression coverage for the B-20 precompile, driven by the real system stack.
//!
//! ## Why this lives here
//!
//! `base-system-tests` boots a genuine L1 (Docker reth + lighthouse) and a genuine in-process L2
//! (builder + sequencer consensus + batcher + follower client + validator consensus). The follower
//! client executes every block it receives through `engine_newPayload`, which is exactly the path
//! `base_engine_tree`'s payload validator routes into the Firehose tracer. Installing a
//! buffer-backed global tracer before the stack starts therefore captures real `FIRE BLOCK` output
//! for real Base-specific transactions — B-20 precompile calls included — without a separate
//! prestate fixture that has to be hand-maintained.
//!
//! ## Why a dedicated test binary with a single test
//!
//! The Firehose tracer is process-wide and may only be installed once. cargo/nextest give each
//! integration-test binary its own process, so this file holds exactly one test; adding a second
//! one here would panic in `BaseFirehoseCapture::install` or interleave two stacks' blocks into one
//! buffer.
//!
//! ## Layers asserted
//!
//! 1. [`BlockInvariants`] over every traced block — property assertions that never need
//!    regenerating (ordinal uniqueness and nesting, call-tree shape, receipt/call log agreement,
//!    no no-op state changes).
//! 2. A narrow [`BlockProjection`] golden over the B-20 transfer transaction alone, with volatile
//!    fields excluded by construction. Regenerate with `GOLDEN_UPDATE=1`.

mod common;

use std::{path::PathBuf, time::Duration};

use alloy_primitives::{B256, U256};
use alloy_signer_local::PrivateKeySigner;
use base_common_precompiles::{ActivationFeature, B20FactoryStorage, B20Variant, IB20};
use base_firehose_tests::{
    BaseFirehoseCapture, BlockInvariants, BlockProjection, Golden, SymbolTable, VolatilePolicy,
};
use base_system_tests::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, B20PrecompileClient};
use eyre::{Result, WrapErr};

/// Initial supply minted to the admin when the token is created.
const INITIAL_SUPPLY: u64 = 1_000_000_000;
/// Amount moved by the traced `transfer` call.
const TRANSFER_AMOUNT: u64 = 100_000_000;
/// `CREATE2` salt for the traced token; fixed so the token address is reproducible.
const TOKEN_SALT: u8 = 0x42;
/// How long to wait for the follower node to validate (and therefore trace) the target block.
const TRACE_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn b20_transfer_is_traced() -> Result<()> {
    // Must happen before the stack starts producing blocks: the traced execution path is gated on
    // `reth_firehose::is_tracer_initialized()` at payload-validation time.
    let capture = BaseFirehoseCapture::install(common::L2_CHAIN_ID, Some(0), Some(0), None);

    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin private key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Asset.id()).await?;

    let salt = B256::repeat_byte(TOKEN_SALT);
    let params = B20PrecompileClient::token_params(
        "Firehose B20",
        "FHB20",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let receipt = b20
        .send_call_receipt(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT) },
            "B-20 transfer",
        )
        .await?;
    let transfer_block = receipt
        .inner
        .block_number
        .ok_or_else(|| eyre::eyre!("B-20 transfer receipt carries no block number"))?;
    let transfer_hash = receipt.inner.transaction_hash;

    let block = capture.wait_for_block(transfer_block, TRACE_TIMEOUT).await?;

    // Layer 1: property assertions over every block the follower traced so far. The most recently
    // written line may still be in flight, so the tail block is covered by the explicit assert
    // below rather than by this sweep.
    let traced = capture.traced_block_numbers();
    for number in traced.iter().rev().skip(1).rev() {
        let traced_block = capture.block(*number)?;
        BlockInvariants::assert(&traced_block)
            .wrap_err_with(|| format!("invariants failed on traced block #{number}"))?;
    }
    BlockInvariants::assert(&block)?;

    // Layer 2: narrow projection golden over the B-20 transfer transaction only. The rest of the
    // block (the L1-info deposit) carries the L1 head and is therefore not reproducible.
    let symbols = SymbolTable::new()
        .with(admin.address(), "admin")
        .with(recipient, "recipient")
        .with(token, "b20-token")
        .with(B20FactoryStorage::ADDRESS, "b20-factory");

    // A live sequencer moves the base fee, block number and wall clock between runs, so drop what
    // the tracer cannot reproduce (`VolatilePolicy::live_node()`): hashes, timestamps, gas, absolute
    // ordinals; balance/nonce changes are deltaized; storage values are kept verbatim.
    let projection = BlockProjection::new()
        .with_symbols(symbols)
        .with_policy(VolatilePolicy::live_node())
        .transaction(&block, transfer_hash.as_slice())?;
    Golden::is_json_equal(&projection, &golden_path("b20_transfer.json"))?.assert_equal();

    Ok(())
}

/// Resolves `name` inside this crate's golden directory.
fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("goldens").join(name)
}
