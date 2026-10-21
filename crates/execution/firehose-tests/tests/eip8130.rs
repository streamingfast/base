//! Firehose tracing of EIP-8130 transactions (type `0x79`), run through the real Base executor
//! and the Firehose wrapper on an Everest genesis.

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256, address, aliases::U48, bytes, keccak256};
use alloy_signer::SignerSync;
use alloy_sol_types::{SolValue, sol};
use base_common_consensus::{
    AccountChange, AccountChangeChannel, BaseTransactionSigned, BaseTxEnvelope, Call, ChangeType,
    CreateEntry, Delegation, Eip8130Constants, Eip8130Contracts, Eip8130Signed, InitialActor,
    Predeploys, SignedAccountChanges, SignedChange, TxEip8130,
};
use base_execution_eip8130::{
    AccountChangeApplier, ConfigChangeAuthorizer, Eip8130GasSchedule, IntrinsicGas,
    IntrinsicGasInput,
};
use base_firehose_tests::{BlockInvariants, TraceContext, run_block};
use base_test_utils::{Account, build_test_genesis_everest};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use firehose_tracer::pb::sf::ethereum::r#type::v2::{
    self as pbeth, TransactionTrace, account_abstraction_account_change::Change,
    account_abstraction_config_change::Channel, account_abstraction_keystore_change,
    balance_change::Reason,
};
use p256::ecdsa::{
    Signature as P256Signature, SigningKey as P256SigningKey, signature::hazmat::PrehashSigner,
};
use sha2::{Digest, Sha256};

/// Contract whose code is `PUSH1 0 PUSH1 0 REVERT`.
const REVERTER: Address = address!("0x00000000000000000000000000000000000ad0ad");
const NONCE_MANAGER: Address = address!("0x813000000000000000000000000000000000aa01");
const BENEFICIARY: Address = address!("0x4200000000000000000000000000000000000011");

sol! {
    struct ActorConfigAbi {
        address authenticator;
        uint48 expiry;
        uint16 scope;
    }

    struct WebAuthnAuth {
        bytes32 r;
        bytes32 s;
        uint256 challengeIndex;
        uint256 typeIndex;
        bytes authenticatorData;
        string clientDataJSON;
    }
}

fn genesis() -> Genesis {
    let mut genesis = build_test_genesis_everest();
    genesis.alloc.insert(
        REVERTER,
        GenesisAccount { code: Some(bytes!("60006000fd")), ..Default::default() },
    );
    genesis
}

const fn context(genesis: &Genesis) -> TraceContext {
    TraceContext {
        number: 1,
        timestamp: genesis.timestamp + 2,
        gas_limit: 30_000_000,
        miner: BENEFICIARY,
        base_fee_per_gas: Some(1),
        difficulty: Some(U256::ZERO),
    }
}

fn call(to: Address, value: u64) -> Call {
    Call { to, value: U256::from(value), data: Bytes::new() }
}

const fn unsigned(chain_id: u64, calls: Vec<Vec<Call>>) -> TxEip8130 {
    TxEip8130 {
        chain_id,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: 1,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 300_000,
        account_changes: Vec::new(),
        calls,
        metadata: Bytes::new(),
        payer: None,
    }
}

/// Signs `tx` as Alice, paying for herself unless `payer` is set.
fn sign(tx: TxEip8130, payer: Option<Account>) -> BaseTransactionSigned {
    let alice = Account::Alice;
    let sender_auth: Bytes = alice
        .signer()
        .sign_hash_sync(&tx.sender_signature_hash())
        .expect("sign")
        .as_bytes()
        .to_vec()
        .into();
    let payer_auth: Bytes = payer.map_or_else(Bytes::new, |payer| {
        let signature =
            payer.signer().sign_hash_sync(&tx.payer_signature_hash(alice.address())).expect("sign");
        [Eip8130Constants::K1_AUTHENTICATOR.as_slice(), signature.as_bytes().as_slice()]
            .concat()
            .into()
    });
    Eip8130Signed::new(tx, sender_auth, payer_auth).into()
}

/// An Everest genesis that also activates Zenith, the Keystore, with `funded` holding 1 ether.
fn keystore_genesis(funded: &[Address]) -> Genesis {
    let mut genesis = genesis();
    let base = genesis.config.extra_fields.get_mut("base").expect("base upgrades");
    base["zenith"] = 0.into();
    for address in funded {
        genesis.alloc.insert(
            *address,
            GenesisAccount { balance: U256::from(10).pow(U256::from(18)), ..Default::default() },
        );
    }
    genesis
}

/// The k1 actor id of `address`: the address right-aligned in a word.
fn k1_actor_id(address: Address) -> B256 {
    address.into_word()
}

/// `abi.encode(actorId, ActorConfig, policyData)` authorizing `actor_id` as an unrestricted admin
/// with no expiry and no policy.
fn authorize_payload(actor_id: B256, authenticator: Address) -> Vec<u8> {
    (actor_id, ActorConfigAbi { authenticator, expiry: U48::ZERO, scope: 0 }, Bytes::new())
        .abi_encode_params()
}

/// A batch of `changes` for Alice's account at sequence 0 of `channel`, signed with her key.
fn alice_batch(
    genesis: &Genesis,
    channel: AccountChangeChannel,
    changes: Vec<SignedChange>,
) -> SignedAccountChanges {
    let alice = Account::Alice;
    let mut batch = SignedAccountChanges { channel, sequence: 0, changes, signature: Bytes::new() };
    let digest =
        ConfigChangeAuthorizer::changes_digest(alice.address(), genesis.config.chain_id, &batch);
    let signature = alice.signer().sign_hash_sync(&digest).expect("sign");
    batch.signature = auth_blob(Eip8130Constants::K1_AUTHENTICATOR, &signature.as_bytes());
    batch
}

/// `authenticator || data`.
fn auth_blob(authenticator: Address, data: &[u8]) -> Bytes {
    [authenticator.as_slice(), data].concat().into()
}

fn run(tx: BaseTransactionSigned) -> TransactionTrace {
    let genesis = genesis();
    run_on(&genesis, tx)
}

fn run_on(genesis: &Genesis, tx: BaseTransactionSigned) -> TransactionTrace {
    run_in(genesis, &context(genesis), tx)
}

fn run_in(
    genesis: &Genesis,
    context: &TraceContext,
    tx: BaseTransactionSigned,
) -> TransactionTrace {
    let outcome = run_block(genesis, context, vec![tx]).expect("block runs");
    BlockInvariants::assert(&outcome.block).expect("block invariants hold");
    outcome.block.transaction_traces.into_iter().next().expect("one transaction")
}

fn call_indexes(trx: &TransactionTrace) -> Vec<Vec<u32>> {
    trx.account_abstraction_trx_input
        .as_ref()
        .expect("account_abstraction_trx_input")
        .call_phases
        .iter()
        .map(|phase| phase.calls.iter().map(|call| call.call_index).collect())
        .collect()
}

fn root_balance_changes(trx: &TransactionTrace) -> Vec<(Address, i32)> {
    trx.calls[0]
        .balance_changes
        .iter()
        .map(|change| (Address::from_slice(&change.address), change.reason))
        .collect()
}

fn committed_phases(trx: &TransactionTrace) -> u64 {
    trx.receipt
        .as_ref()
        .and_then(|r| r.account_abstraction_trx_output.as_ref())
        .expect("account_abstraction_trx_output")
        .committed_phases
}

#[test]
fn failed_phase_reverts_its_calls_and_keeps_committed_ones() {
    let genesis = genesis();
    let bob = Account::Bob.address();
    let charlie = Account::Charlie.address();
    let tx = unsigned(
        genesis.config.chain_id,
        vec![
            vec![call(bob, 1)],
            vec![call(charlie, 2), call(REVERTER, 0), call(bob, 3)],
            vec![call(charlie, 4)],
        ],
    );
    let trx = run(sign(tx, None));

    assert_eq!(pbeth::transaction_trace::Type::TrxTypeAccountAbstraction as i32, trx.r#type);
    assert_eq!(pbeth::TransactionTraceStatus::Failed as i32, trx.status);
    assert_eq!(1, committed_phases(&trx));
    assert!(trx.to.is_empty(), "the calls carry the targets");
    assert_eq!(Account::Alice.address().as_slice(), trx.from.as_slice());

    // Calls: #1 root, #2 bob (phase 0), #3 charlie and #4 reverter (phase 1).
    assert_eq!(vec![vec![2], vec![3, 4, 0], vec![0]], call_indexes(&trx));
    let reverted: Vec<bool> = trx.calls.iter().map(|call| call.state_reverted).collect();
    assert_eq!(vec![false, false, true, true], reverted);
    assert!(!trx.calls[0].status_failed, "the root call is never failed");

    let alice = Account::Alice.address();
    let root = root_balance_changes(&trx);
    assert_eq!(root.first(), Some(&(alice, Reason::GasBuy as i32)), "payer charged first");
    assert!(root.contains(&(alice, Reason::GasRefund as i32)), "payer refunded");
    assert!(
        root.contains(&(BENEFICIARY, Reason::RewardTransactionFee as i32)),
        "tip to the beneficiary"
    );
    assert!(
        root.contains(&(Predeploys::BASE_FEE_VAULT, Reason::RewardTransactionFee as i32)),
        "base fee to its vault"
    );
    assert!(
        root.iter().all(|(_, reason)| *reason != Reason::Transfer as i32),
        "no fee movement left as a transfer: {root:?}"
    );
    assert_eq!(1, trx.calls[0].nonce_changes.len(), "account nonce for nonce key 0");
}

#[test]
fn named_payer_and_nonce_channel() {
    let genesis = genesis();
    let bob = Account::Bob.address();
    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Charlie.address(), 1)]]);
    tx.nonce_key = U256::from(7);
    tx.payer = Some(bob);
    let trx = run(sign(tx, Some(Account::Bob)));

    assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    assert_eq!(bob.as_slice(), input.payer.as_slice());
    assert_eq!(vec![7u8], input.nonce_key.as_ref().expect("nonce key").bytes);
    assert_eq!(vec![vec![2]], call_indexes(&trx));

    let root = root_balance_changes(&trx);
    assert_eq!(root.first(), Some(&(bob, Reason::GasBuy as i32)), "the payer is charged");
    assert!(root.contains(&(bob, Reason::GasRefund as i32)), "the payer is refunded");
    assert!(
        !root.iter().any(|(address, _)| *address == Account::Alice.address()),
        "the sender pays nothing: {root:?}"
    );

    let root_call = &trx.calls[0];
    assert!(root_call.nonce_changes.is_empty(), "a nonce channel leaves the account nonce");
    assert!(
        root_call.storage_changes.iter().any(|change| change.address == NONCE_MANAGER.to_vec()),
        "the channel sequence is a nonce manager storage write"
    );
}

#[test]
fn delegation_is_a_code_change_on_the_root_call() {
    let genesis = genesis();
    let target = address!("0x00000000000000000000000000000000000de1e9");
    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.account_changes = vec![AccountChange::Delegation(Delegation { target })];
    let trx = run(sign(tx, None));

    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    assert_eq!(1, input.account_changes.len());

    let alice = Account::Alice.address();
    let code_change = trx.calls[0]
        .code_changes
        .iter()
        .find(|change| change.address == alice.to_vec())
        .expect("the sender's delegation code is recorded on the root call");
    assert_eq!([&[0xef, 0x01, 0x00][..], target.as_slice()].concat(), code_change.new_code);
}

#[test]
fn keystore_create_records_the_entry_and_the_new_account_code() {
    let alice = Account::Alice;
    let initial_actors =
        vec![InitialActor::owner(k1_actor_id(alice.address()), Eip8130Constants::K1_AUTHENTICATOR)];
    let entry = CreateEntry {
        user_salt: B256::repeat_byte(0x5a),
        code: bytes!("6000"),
        initial_actors: initial_actors.clone(),
    };
    let account =
        AccountChangeApplier::compute_address(entry.user_salt, &entry.code, &initial_actors)
            .expect("address derivation");
    let genesis = keystore_genesis(&[account]);

    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.sender = Some(account);
    tx.account_changes = vec![AccountChange::Create(entry.clone())];
    let signature = alice.signer().sign_hash_sync(&tx.sender_signature_hash()).expect("sign");
    let sender_auth = auth_blob(Eip8130Constants::K1_AUTHENTICATOR, &signature.as_bytes());
    let trx = run_on(&genesis, Eip8130Signed::new(tx, sender_auth, Bytes::new()).into());

    assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
    assert_eq!(account.as_slice(), trx.from.as_slice());
    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    assert!(input.sender_auth.is_empty(), "a k1 signature is in v, r and s");
    assert_eq!(signature.r().to_be_bytes::<32>().to_vec(), trx.r);

    let Some(Change::Create(create)) = &input.account_changes[0].change else {
        panic!("expected a create entry, got {:?}", input.account_changes);
    };
    assert_eq!(entry.user_salt.to_vec(), create.user_salt);
    assert_eq!(entry.code.to_vec(), create.code);
    assert_eq!(1, create.initial_actors.len());
    assert_eq!(k1_actor_id(alice.address()).to_vec(), create.initial_actors[0].actor_id);
    assert_eq!(Eip8130Constants::K1_AUTHENTICATOR.to_vec(), create.initial_actors[0].authenticator);

    let root = &trx.calls[0];
    let code_change = root
        .code_changes
        .iter()
        .find(|change| change.address == account.to_vec())
        .expect("the created account's code is recorded on the root call");
    assert_eq!(entry.code.to_vec(), code_change.new_code);
    assert!(
        root.storage_changes
            .iter()
            .any(|change| change.address == Eip8130Contracts::ACCOUNT_CONFIG.to_vec()),
        "the initial actors are Keystore storage writes"
    );
    assert!(
        root.logs.iter().any(|log| log.address == Eip8130Contracts::ACCOUNT_CONFIG.to_vec()),
        "the Keystore logs are on the root call"
    );
}

#[test]
fn keystore_config_change_authorizes_the_p256_actor_that_signs() {
    let genesis = keystore_genesis(&[]);
    let alice = Account::Alice;
    let p256_key = P256SigningKey::from_slice(&[0x77; 32]).expect("p256 key");
    let point = p256_key.verifying_key().to_encoded_point(false);
    let p256_actor_id = keccak256(&point.as_bytes()[1..]);

    let payload = authorize_payload(p256_actor_id, Eip8130Contracts::P256_AUTHENTICATOR);
    let batch = alice_batch(
        &genesis,
        AccountChangeChannel::Multichain,
        vec![SignedChange {
            change_type: ChangeType::AuthorizeActor,
            payload: payload.clone().into(),
        }],
    );

    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.sender = Some(alice.address());
    tx.account_changes = vec![AccountChange::ConfigChange(batch.clone())];
    let hash = tx.sender_signature_hash();
    let signature: P256Signature = p256_key.sign_prehash(hash.as_slice()).expect("sign");
    let signature = signature.normalize_s().unwrap_or(signature);
    let data = [&signature.to_bytes()[..], &point.as_bytes()[1..], &[0]].concat();
    let sender_auth = auth_blob(Eip8130Contracts::P256_AUTHENTICATOR, &data);
    let trx = run_on(&genesis, Eip8130Signed::new(tx, sender_auth.clone(), Bytes::new()).into());

    assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    assert_eq!(sender_auth.to_vec(), input.sender_auth);
    assert!(trx.v.is_empty() && trx.r.is_empty() && trx.s.is_empty(), "no k1 signature");

    let Some(Change::ConfigChange(change)) = &input.account_changes[0].change else {
        panic!("expected a config change entry, got {:?}", input.account_changes);
    };
    assert_eq!(Channel::Multichain as i32, change.channel);
    assert_eq!(0, change.sequence);
    assert_eq!(batch.signature.to_vec(), change.auth);
    assert_eq!(1, change.changes.len());
    assert_eq!(
        account_abstraction_keystore_change::Type::AuthorizeActor as i32,
        change.changes[0].r#type
    );
    assert_eq!(payload, change.changes[0].payload);

    assert!(
        trx.calls[0]
            .logs
            .iter()
            .any(|log| log.address == Eip8130Contracts::ACCOUNT_CONFIG.to_vec()),
        "the ActorAuthorized log is on the root call"
    );
}

#[test]
fn open_payer_signature_that_does_not_recover_rejects_the_block() {
    let genesis = genesis();
    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.payer = Some(Eip8130Constants::OPEN_PAYER);
    let sender_auth: Bytes = Account::Alice
        .signer()
        .sign_hash_sync(&tx.sender_signature_hash())
        .expect("sign")
        .as_bytes()
        .to_vec()
        .into();
    // `v` must be 27 or 28, so this payer signature never recovers.
    let payer_auth = Bytes::from(vec![0x11; 65]);
    let tx = Eip8130Signed::new(tx, sender_auth, payer_auth).into();

    let result = run_block(&genesis, &context(&genesis), vec![tx]);
    assert!(result.is_err(), "the block is rejected instead of traced");
}

#[test]
fn payer_that_is_the_beneficiary_gets_its_refund_then_its_tip() {
    let genesis = genesis();
    let alice = Account::Alice.address();
    let mut context = context(&genesis);
    context.miner = alice;
    let tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    let trx = run_in(&genesis, &context, sign(tx, None));

    let root_reasons: Vec<i32> = trx.calls[0]
        .balance_changes
        .iter()
        .filter(|change| change.address == alice.to_vec())
        .map(|change| change.reason)
        .collect();
    assert_eq!(
        vec![Reason::GasBuy as i32, Reason::GasRefund as i32, Reason::RewardTransactionFee as i32],
        root_reasons,
        "the payer is charged, refunded, then tipped"
    );

    // Across every call, including the 1 wei the payer sends in its call, each change of the
    // payer starts where the previous one ended.
    let mut changes: Vec<_> = trx
        .calls
        .iter()
        .flat_map(|call| &call.balance_changes)
        .filter(|change| change.address == alice.to_vec())
        .collect();
    changes.sort_by_key(|change| change.ordinal);
    for pair in changes.windows(2) {
        assert_eq!(
            big(&pair[0].new_value),
            big(&pair[1].old_value),
            "each change starts where the previous one ended"
        );
    }
}

#[test]
fn nonce_free_transaction_records_its_replay_entry_and_no_nonce_change() {
    let genesis = genesis();
    let context = context(&genesis);
    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.nonce_key = Eip8130Constants::NONCE_KEY_MAX;
    tx.valid_before = context.timestamp + 10;
    let trx = run_in(&genesis, &context, sign(tx, None));

    assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    assert_eq!(vec![0xff; 32], input.nonce_key.as_ref().expect("nonce key").bytes);
    let root = &trx.calls[0];
    assert!(root.nonce_changes.is_empty(), "nonce-free mode leaves every nonce alone");
    assert!(
        root.storage_changes.iter().any(|change| change.address == NONCE_MANAGER.to_vec()),
        "the replay entry is a nonce manager storage write"
    );
}

#[test]
fn failure_in_the_first_phase_commits_nothing() {
    let genesis = genesis();
    let bob = Account::Bob.address();
    let tx = unsigned(genesis.config.chain_id, vec![vec![call(REVERTER, 0)], vec![call(bob, 1)]]);
    let trx = run(sign(tx, None));

    assert_eq!(pbeth::TransactionTraceStatus::Failed as i32, trx.status);
    assert_eq!(0, committed_phases(&trx));
    assert_eq!(vec![vec![2], vec![0]], call_indexes(&trx));
    let reverted: Vec<bool> = trx.calls.iter().map(|call| call.state_reverted).collect();
    assert_eq!(vec![false, true], reverted, "only the reverted call is reverted");
    assert_eq!(
        root_balance_changes(&trx).first(),
        Some(&(Account::Alice.address(), Reason::GasBuy as i32)),
        "the fees are still charged"
    );
}

#[test]
fn call_refused_for_lack_of_new_account_gas_is_left_unlinked() {
    let genesis = genesis();
    let alice = Account::Alice;
    let fresh = address!("0x00000000000000000000000000000000000fe5e1");
    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(fresh, 1)]]);

    // One gas short of the new-account charge once the sender-intrinsic gas is paid, so the
    // protocol refuses the call before it runs.
    let shortfall = Eip8130GasSchedule::NEW_ACCOUNT_COST - 1;
    let mut gas_limit = shortfall;
    for _ in 0..8 {
        tx.gas_limit = gas_limit;
        let signed = sign(tx.clone(), None);
        let BaseTxEnvelope::Eip8130(envelope) = &signed else { panic!("an EIP-8130 envelope") };
        let intrinsic = IntrinsicGas::compute(
            envelope,
            &signed.encoded_2718(),
            &IntrinsicGasInput::new(alice.address(), true),
        )
        .expect("intrinsic gas");
        let next = intrinsic.sender_intrinsic() + shortfall;
        if next == gas_limit {
            break;
        }
        gas_limit = next;
    }
    tx.gas_limit = gas_limit;
    let trx = run(sign(tx, None));

    assert_eq!(pbeth::TransactionTraceStatus::Failed as i32, trx.status);
    assert_eq!(0, committed_phases(&trx));
    assert_eq!(vec![vec![0]], call_indexes(&trx), "the refused call never ran");
    assert_eq!(1, trx.calls.len(), "only the root call");
}

fn big(value: &Option<pbeth::BigInt>) -> U256 {
    value.as_ref().map_or(U256::ZERO, |value| U256::from_be_slice(&value.bytes))
}

#[test]
fn keystore_local_batch_authorizes_then_revokes_an_actor() {
    let genesis = keystore_genesis(&[]);
    let alice = Account::Alice;
    let actor_id = B256::repeat_byte(0x5c);
    let revoke_payload = actor_id.abi_encode();
    let batch = alice_batch(
        &genesis,
        AccountChangeChannel::Local,
        vec![
            SignedChange {
                change_type: ChangeType::AuthorizeActor,
                payload: authorize_payload(actor_id, Eip8130Contracts::P256_AUTHENTICATOR).into(),
            },
            SignedChange {
                change_type: ChangeType::RevokeActor,
                payload: revoke_payload.clone().into(),
            },
        ],
    );

    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.sender = Some(alice.address());
    tx.account_changes = vec![AccountChange::ConfigChange(batch)];
    let signature = alice.signer().sign_hash_sync(&tx.sender_signature_hash()).expect("sign");
    let sender_auth = auth_blob(Eip8130Constants::K1_AUTHENTICATOR, &signature.as_bytes());
    let trx = run_on(&genesis, Eip8130Signed::new(tx, sender_auth, Bytes::new()).into());

    assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    let Some(Change::ConfigChange(change)) = &input.account_changes[0].change else {
        panic!("expected a config change entry, got {:?}", input.account_changes);
    };
    assert_eq!(Channel::Local as i32, change.channel);
    let types: Vec<i32> = change.changes.iter().map(|change| change.r#type).collect();
    assert_eq!(
        vec![
            account_abstraction_keystore_change::Type::AuthorizeActor as i32,
            account_abstraction_keystore_change::Type::RevokeActor as i32,
        ],
        types
    );
    assert_eq!(revoke_payload, change.changes[1].payload);

    let keystore_logs = trx.calls[0]
        .logs
        .iter()
        .filter(|log| log.address == Eip8130Contracts::ACCOUNT_CONFIG.to_vec())
        .count();
    assert_eq!(2, keystore_logs, "the ActorAuthorized and ActorRevoked logs are on the root call");
}

#[test]
fn keystore_webauthn_sender_auth_is_recorded_as_signed() {
    let genesis = keystore_genesis(&[]);
    let alice = Account::Alice;
    let key = P256SigningKey::from_slice(&[0x78; 32]).expect("p256 key");
    let point = key.verifying_key().to_encoded_point(false);
    let (x, y) =
        (B256::from_slice(&point.as_bytes()[1..33]), B256::from_slice(&point.as_bytes()[33..]));
    let actor_id = keccak256([x.as_slice(), y.as_slice()].concat());
    let batch = alice_batch(
        &genesis,
        AccountChangeChannel::Multichain,
        vec![SignedChange {
            change_type: ChangeType::AuthorizeActor,
            payload: authorize_payload(actor_id, Eip8130Contracts::WEBAUTHN_AUTHENTICATOR).into(),
        }],
    );

    let mut tx = unsigned(genesis.config.chain_id, vec![vec![call(Account::Bob.address(), 1)]]);
    tx.sender = Some(alice.address());
    tx.account_changes = vec![AccountChange::ConfigChange(batch)];

    // An OpenZeppelin-layout WebAuthn assertion over the sender signature hash.
    let challenge = URL_SAFE_NO_PAD.encode(tx.sender_signature_hash().as_slice());
    let client_json = format!(
        "{{\"type\":\"webauthn.get\",\"challenge\":\"{challenge}\",\"origin\":\"https://base.org\"}}"
    );
    let mut authenticator_data = vec![0xaa; 32];
    authenticator_data.push(0x01); // user present
    authenticator_data.extend_from_slice(&[0, 0, 0, 1]);
    let signed_digest = Sha256::new()
        .chain_update(&authenticator_data)
        .chain_update(Sha256::digest(client_json.as_bytes()))
        .finalize();
    let signature: P256Signature = key.sign_prehash(&signed_digest).expect("sign");
    let signature = signature.normalize_s().unwrap_or(signature);
    let assertion = WebAuthnAuth {
        r: B256::from_slice(&signature.to_bytes()[..32]),
        s: B256::from_slice(&signature.to_bytes()[32..]),
        challengeIndex: U256::from(client_json.find("\"challenge\":\"").expect("challenge")),
        typeIndex: U256::from(client_json.find("\"type\":\"webauthn.get\"").expect("type")),
        authenticatorData: authenticator_data.into(),
        clientDataJSON: client_json,
    };
    let data = (assertion, x, y).abi_encode_params();
    let sender_auth = auth_blob(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR, &data);
    let trx = run_on(&genesis, Eip8130Signed::new(tx, sender_auth.clone(), Bytes::new()).into());

    assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
    let input = trx.account_abstraction_trx_input.as_ref().expect("account_abstraction_trx_input");
    assert_eq!(sender_auth.to_vec(), input.sender_auth);
    assert!(trx.v.is_empty() && trx.r.is_empty() && trx.s.is_empty(), "no k1 signature");
}
