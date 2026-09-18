//! Firehose tracing regression coverage for the precompile surfaces Cobalt introduces, driven by
//! the real system stack.
//!
//! Cobalt switches the B-20 asset, stablecoin and policy-registry precompiles to their V2 wire
//! surface and installs the `TxContext` and `NonceManager` precompiles. None of that adds a
//! transaction type or changes the block model, but it adds new precompile events (`Seized`,
//! `UIMultiplierUpdated`, `UIMultiplierUpdateCancelled`, `CompositePolicyUpdated`) and new
//! storage paths. Precompile logs are emitted straight into the revm journal rather than through a
//! `LOG` opcode, so this test checks that the tracer attributes every one of them to its call and
//! that the traced receipts match what the node's RPC returns.
//!
//! See `firehose_b20.rs` for why this runs against the Docker-backed stack and why the binary holds
//! a single test (the Firehose tracer is a process-wide singleton).
//!
//! ## Layers asserted
//!
//! 1. [`BlockInvariants`] over every traced block.
//! 2. For each Cobalt-only transaction: the traced receipt logs equal the RPC receipt logs, and the
//!    expected Cobalt event is among them.
//! 3. A narrow [`BlockProjection`] golden over the seize transaction. Regenerate with
//!    `GOLDEN_UPDATE=1`.

#[path = "common/balance.rs"]
mod balance;
#[path = "common/cobalt.rs"]
mod cobalt;
mod common;

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_precompiles::{
    ActivationFeature, B20FactoryStorage, B20PolicyType, B20TokenRole, B20Variant, IB20, IB20Asset,
    INonceManager, IPolicyRegistry, NonceManagerStorage, PolicyRegistryStorage,
};
use base_common_rpc_types::BaseTransactionReceipt;
use base_firehose_tests::{
    BaseFirehoseCapture, BlockInvariants, BlockProjection, FirehoseCapture, Golden, SymbolTable,
    VolatilePolicy,
};
use base_system_tests::{
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, B20PrecompileClient, SystemTestStackBuilder,
};
use eyre::{Result, WrapErr, ensure};

/// Initial supply minted to the admin when the token is created.
const INITIAL_SUPPLY: u64 = 1_000_000_000;
/// Amount moved to the holder so it has a balance to seize.
const HOLDER_AMOUNT: u64 = 100_000_000;
/// Amount seized from the holder.
const SEIZE_AMOUNT: u64 = 40_000_000;
/// `CREATE2` salt for the traced token; distinct from the salts used by other system tests.
const TOKEN_SALT: u8 = 0x43;
/// Scheduled UI multiplier (1.5 in WAD).
const UI_MULTIPLIER: u128 = 1_500_000_000_000_000_000;
/// How far in the future the UI multiplier update is scheduled.
const UI_MULTIPLIER_DELAY: Duration = Duration::from_secs(86_400);
/// Memo attached to the seize.
const SEIZE_MEMO: B256 = B256::repeat_byte(0x5e);
/// How long to wait for the follower node to validate (and therefore trace) a block.
const TRACE_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn cobalt_b20_surfaces_are_traced() -> Result<()> {
    // Must happen before the stack starts producing blocks: the traced execution path is gated on
    // `reth_firehose::is_tracer_initialized()` at payload-validation time.
    let capture = BaseFirehoseCapture::install(common::L2_CHAIN_ID, Some(0), Some(0), None);

    let (_system, provider) = cobalt::start_cobalt_stack(SystemTestStackBuilder::new()).await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin private key")?;
    let receiver = ANVIL_ACCOUNT_6.address;
    let holder = ANVIL_ACCOUNT_7.address;
    balance::wait_for_balance(&provider, admin.address()).await?;

    let b20 = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(balance::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Asset.id()).await?;
    b20.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;

    // Seize policies: an empty allowlist as SEIZE_EXEMPT (nobody is exempt, so the holder is
    // seizable) and, as SEIZE_RECEIVER, a UNION composite of an allowlist holding the receiver and
    // the empty allowlist (a composite needs at least two children).
    let exempt_policy =
        create_policy(&b20, admin.address(), IPolicyRegistry::PolicyType::ALLOWLIST).await?;
    let receiver_allowlist =
        create_policy(&b20, admin.address(), IPolicyRegistry::PolicyType::ALLOWLIST).await?;
    b20.send_call(
        PolicyRegistryStorage::ADDRESS,
        IPolicyRegistry::updateAllowlistCall {
            policyId: receiver_allowlist,
            allowed: true,
            accounts: vec![receiver],
        },
        "updateAllowlist add receiver",
    )
    .await?;
    let composite_call = IPolicyRegistry::createCompositePolicyCall {
        admin: admin.address(),
        policyType: IPolicyRegistry::PolicyType::UNION,
        childPolicyIds: vec![receiver_allowlist, exempt_policy],
    };
    let receiver_policy = IPolicyRegistry::createCompositePolicyCall::abi_decode_returns(
        b20.call(PolicyRegistryStorage::ADDRESS, composite_call.clone()).await?.as_ref(),
    )
    .wrap_err("Failed to decode createCompositePolicy return")?;
    let composite_receipt = b20
        .send_call_receipt(PolicyRegistryStorage::ADDRESS, composite_call, "createCompositePolicy")
        .await?;

    let params = B20PrecompileClient::token_params(
        "Firehose Cobalt B20",
        "FHCB20",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::Asset, params, B256::repeat_byte(TOKEN_SALT)).await?;
    b20.wait_for_token_code(token, balance::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL)
        .await?;

    let operator_role = IB20Asset::OPERATOR_ROLECall::abi_decode_returns(
        b20.call(token, IB20Asset::OPERATOR_ROLECall {}).await?.as_ref(),
    )
    .wrap_err("Failed to decode OPERATOR_ROLE")?;
    for (role, label) in
        [(B20TokenRole::Seize.id(), "grantRole SEIZE"), (operator_role, "grantRole OPERATOR")]
    {
        b20.send_call(token, IB20::grantRoleCall { role, account: admin.address() }, label).await?;
    }
    for (scope, policy_id, label) in [
        (B20PolicyType::SeizeExempt.id(), exempt_policy, "updatePolicy SEIZE_EXEMPT"),
        (B20PolicyType::SeizeReceiver.id(), receiver_policy, "updatePolicy SEIZE_RECEIVER"),
    ] {
        b20.send_call(
            token,
            IB20::updatePolicyCall { policyScope: scope, newPolicyId: policy_id },
            label,
        )
        .await?;
    }
    b20.transfer(token, holder, U256::from(HOLDER_AMOUNT)).await?;

    let seize_receipt = b20
        .send_call_receipt(
            token,
            IB20::seizeWithMemoCall {
                from: holder,
                to: receiver,
                amount: U256::from(SEIZE_AMOUNT),
                memo: SEIZE_MEMO,
            },
            "seizeWithMemo",
        )
        .await?;
    ensure!(
        b20.balance_of(token, receiver).await? == U256::from(SEIZE_AMOUNT),
        "seize did not credit the receiver"
    );

    let effective_at = SystemTime::now().duration_since(UNIX_EPOCH)? + UI_MULTIPLIER_DELAY;
    let ui_update_receipt = b20
        .send_call_receipt(
            token,
            IB20Asset::updateUIMultiplierCall {
                newMultiplier: U256::from(UI_MULTIPLIER),
                effectiveAt: U256::from(effective_at.as_secs()),
            },
            "updateUIMultiplier",
        )
        .await?;
    let ui_cancel_receipt = b20
        .send_call_receipt(
            token,
            IB20Asset::cancelUIMultiplierUpdateCall {},
            "cancelUIMultiplierUpdate",
        )
        .await?;

    // A plain transaction into the Cobalt NonceManager precompile, which must trace as an ordinary
    // successful call with no state change.
    let nonce_receipt = b20
        .send_call_receipt(
            NonceManagerStorage::ADDRESS,
            INonceManager::getNonceCall { account: admin.address(), nonceKey: U256::from(1) },
            "NonceManager getNonce",
        )
        .await?;

    let checks = [
        (&composite_receipt, Some(IPolicyRegistry::CompositePolicyUpdated::SIGNATURE_HASH)),
        (&seize_receipt, Some(IB20::Seized::SIGNATURE_HASH)),
        (&ui_update_receipt, Some(IB20Asset::UIMultiplierUpdated::SIGNATURE_HASH)),
        (&ui_cancel_receipt, Some(IB20Asset::UIMultiplierUpdateCancelled::SIGNATURE_HASH)),
        (&nonce_receipt, None),
    ];
    let mut last_block = 0;
    for (receipt, expected_event) in checks {
        last_block =
            last_block.max(assert_traced_logs_match(&capture, receipt, expected_event).await?);
    }

    // Every block traced so far, up to and including the last one checked above.
    for number in capture.traced_block_numbers().into_iter().filter(|n| *n <= last_block) {
        let traced_block = capture.block(number)?;
        BlockInvariants::assert(&traced_block)
            .wrap_err_with(|| format!("invariants failed on traced block #{number}"))?;
    }

    let seize_block = capture
        .block(receipt_block_number(&seize_receipt)?)
        .wrap_err("seize block was not traced")?;
    let symbols = SymbolTable::new()
        .with(admin.address(), "admin")
        .with(holder, "holder")
        .with(receiver, "receiver")
        .with(token, "b20-token")
        .with(B20FactoryStorage::ADDRESS, "b20-factory")
        .with(PolicyRegistryStorage::ADDRESS, "policy-registry");
    let projection = BlockProjection::new()
        .with_symbols(symbols)
        .with_policy(VolatilePolicy::live_node())
        .transaction(&seize_block, seize_receipt.inner.transaction_hash.as_slice())?;
    Golden::is_json_equal(&projection, &golden_path("cobalt_b20_seize.json"))?.assert_equal();

    Ok(())
}

/// Creates a policy in the registry and returns its id.
///
/// The id is read with an `eth_call` first; the stack has a single sender, so the registry counter
/// cannot move between the simulation and the transaction.
async fn create_policy(
    client: &B20PrecompileClient<'_>,
    admin: Address,
    policy_type: IPolicyRegistry::PolicyType,
) -> Result<u64> {
    let call = IPolicyRegistry::createPolicyCall { admin, policyType: policy_type };
    let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
    let policy_id = IPolicyRegistry::createPolicyCall::abi_decode_returns(output.as_ref())
        .wrap_err("Failed to decode createPolicy return")?;
    client.send_call(PolicyRegistryStorage::ADDRESS, call, "createPolicy").await?;
    Ok(policy_id)
}

/// Waits for the receipt's block to be traced, then checks that the traced transaction succeeded,
/// that its receipt logs equal the RPC receipt logs, and that `expected_event` is among them.
/// Returns the block number.
async fn assert_traced_logs_match(
    capture: &FirehoseCapture,
    receipt: &BaseTransactionReceipt,
    expected_event: Option<B256>,
) -> Result<u64> {
    let number = receipt_block_number(receipt)?;
    let hash = receipt.inner.transaction_hash;
    let block = capture.wait_for_block(number, TRACE_TIMEOUT).await?;
    let trace = block
        .transaction_traces
        .iter()
        .find(|trace| trace.hash == hash.as_slice())
        .ok_or_else(|| eyre::eyre!("transaction {hash} missing from traced block #{number}"))?;
    let traced_logs = &trace
        .receipt
        .as_ref()
        .ok_or_else(|| eyre::eyre!("transaction {hash} has no traced receipt"))?
        .logs;

    let rpc_logs = receipt.inner.logs();
    ensure!(
        traced_logs.len() == rpc_logs.len(),
        "transaction {hash}: {} traced logs, {} RPC logs",
        traced_logs.len(),
        rpc_logs.len()
    );
    for (index, (traced, rpc)) in traced_logs.iter().zip(rpc_logs).enumerate() {
        let rpc_topics: Vec<&[u8]> = rpc.data().topics().iter().map(|t| t.as_slice()).collect();
        let traced_topics: Vec<&[u8]> = traced.topics.iter().map(Vec::as_slice).collect();
        ensure!(
            traced.address == rpc.address().as_slice()
                && traced_topics == rpc_topics
                && traced.data == rpc.data().data.as_ref(),
            "transaction {hash}: traced log {index} differs from the RPC receipt log"
        );
    }
    if let Some(event) = expected_event {
        ensure!(
            traced_logs
                .iter()
                .any(|log| log.topics.first().map(Vec::as_slice) == Some(event.as_slice())),
            "transaction {hash}: expected event {event} not in traced logs"
        );
    }
    Ok(number)
}

/// Returns the block number a receipt was included in.
fn receipt_block_number(receipt: &BaseTransactionReceipt) -> Result<u64> {
    receipt.inner.block_number.ok_or_else(|| {
        eyre::eyre!("receipt {} carries no block number", receipt.inner.transaction_hash)
    })
}

/// Resolves `name` inside this crate's golden directory.
fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("goldens").join(name)
}
