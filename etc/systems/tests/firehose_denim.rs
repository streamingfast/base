//! Firehose tracing regression coverage for the Denim activation block, driven by the real system
//! stack.
//!
//! The first Denim block installs the BaseTime predeploy before its transactions run: it sets the
//! implementation account's code and points the BaseTime proxy's EIP-1967 implementation slot at
//! it. Both writes are committed straight to the state DB, outside the EVM, so the inspector never
//! sees them. This test checks that the traced activation block still carries them: the code as a
//! block-level code change, and the slot inside a system call to the proxy.
//!
//! See `firehose_b20.rs` for why this runs against the Docker-backed stack and why the binary holds
//! a single test (the Firehose tracer is a process-wide singleton).

mod common;

use std::time::Duration;

use alloy_primitives::{B256, KECCAK256_EMPTY};
use base_common_consensus::Predeploys;
use base_common_evm::BaseTime;
use base_firehose_tests::{BaseFirehoseCapture, BlockInvariants};
use base_system_tests::SystemTestStackBuilder;
use eyre::{OptionExt, Result, WrapErr, ensure};

/// Leaves enough pre-Denim runway for the stack to start, as `builder_cutover.rs` does.
const DENIM_ACTIVATION_BLOCK: u64 = 25;
/// How long to wait for the follower node to validate (and therefore trace) a block.
const TRACE_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread")]
async fn denim_activation_block_traces_base_time_install() -> Result<()> {
    // Must happen before the stack starts producing blocks: the traced execution path is gated on
    // `reth_firehose::is_tracer_initialized()` at payload-validation time.
    let capture = BaseFirehoseCapture::install(common::L2_CHAIN_ID, Some(0), Some(0), None);

    let _system = SystemTestStackBuilder::new()
        .with_l1_chain_id(common::L1_CHAIN_ID)
        .with_l2_chain_id(common::L2_CHAIN_ID)
        .with_base_cobalt_activation_block(0)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK)
        .with_payload_builder_cutover()
        .build()
        .await?;

    capture.wait_for_block(DENIM_ACTIVATION_BLOCK + 1, TRACE_TIMEOUT).await?;
    for number in
        capture.traced_block_numbers().into_iter().filter(|n| *n <= DENIM_ACTIVATION_BLOCK + 1)
    {
        BlockInvariants::assert(&capture.block(number)?)
            .wrap_err_with(|| format!("invariants failed on traced block #{number}"))?;
    }

    let implementation = BaseTime::IMPLEMENTATION_ADDRESS;
    let implementation_slot = B256::from(BaseTime::IMPLEMENTATION_SLOT);
    let implementation_word = implementation.into_word();

    let activation =
        capture.block(DENIM_ACTIVATION_BLOCK).wrap_err("activation block was not traced")?;
    ensure!(
        activation
            .transaction_traces
            .get(1)
            .is_some_and(|trx| trx.to == Predeploys::BASE_TIME.as_slice()),
        "activation block has no BaseTime deposit at index 1, so Denim is not active"
    );

    let code_change = activation
        .code_changes
        .iter()
        .find(|change| change.address == implementation.as_slice())
        .ok_or_eyre("activation block has no code change for the BaseTime implementation")?;
    ensure!(code_change.old_hash == KECCAK256_EMPTY.as_slice(), "implementation had code before");
    ensure!(
        code_change.new_hash == BaseTime::IMPLEMENTATION_CODE_HASH.as_slice(),
        "implementation code hash is {}",
        alloy_primitives::hex::encode(&code_change.new_hash)
    );

    let slot_change = activation
        .system_calls
        .iter()
        .filter(|call| call.address == Predeploys::BASE_TIME.as_slice())
        .flat_map(|call| &call.storage_changes)
        .find(|change| change.key == implementation_slot.as_slice())
        .ok_or_eyre(
            "activation block has no system call writing the BaseTime implementation slot",
        )?;
    ensure!(slot_change.old_value == B256::ZERO.as_slice(), "implementation slot was set before");
    ensure!(
        slot_change.new_value == implementation_word.as_slice(),
        "implementation slot now holds {}",
        alloy_primitives::hex::encode(&slot_change.new_value)
    );

    let previous = capture
        .block(DENIM_ACTIVATION_BLOCK - 1)
        .wrap_err("block before activation was not traced")?;
    ensure!(
        !previous.code_changes.iter().any(|change| change.address == implementation.as_slice()),
        "block before activation has a BaseTime implementation code change"
    );
    ensure!(
        !previous.system_calls.iter().any(|call| call.address == Predeploys::BASE_TIME.as_slice()),
        "block before activation has a system call to BaseTime"
    );

    let next = capture.block(DENIM_ACTIVATION_BLOCK + 1)?;
    ensure!(
        !next.system_calls.iter().any(|call| call.address == Predeploys::BASE_TIME.as_slice()),
        "block after activation repeats the BaseTime install"
    );

    Ok(())
}
