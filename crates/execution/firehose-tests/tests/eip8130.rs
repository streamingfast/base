//! Firehose tracing of EIP-8130 transactions (type `0x79`), run through the real Base executor
//! and the Firehose wrapper on an Everest genesis.

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256, address, aliases::U48, bytes, keccak256};
use alloy_signer::SignerSync;
use alloy_sol_types::{SolValue, sol};
use base_common_consensus::{
    AccountChange, AccountChangeChannel, BaseTransactionSigned, Call, ChangeType, CreateEntry,
    Delegation, Eip8130Constants, Eip8130Contracts, Eip8130Signed, InitialActor, Predeploys,
    SignedAccountChanges, SignedChange, TxEip8130,
};
use base_execution_eip8130::{AccountChangeApplier, ConfigChangeAuthorizer};
use base_firehose_tests::{BlockInvariants, TraceContext, run_block};
use base_test_utils::{Account, build_test_genesis_everest};
use firehose_tracer::pb::sf::ethereum::r#type::v2::{
    self as pbeth, TransactionTrace, account_abstraction_account_change::Change,
    account_abstraction_config_change::Channel, account_abstraction_keystore_change,
    balance_change::Reason,
};
use p256::ecdsa::{
    Signature as P256Signature, SigningKey as P256SigningKey, signature::hazmat::PrehashSigner,
};

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

/// `authenticator || data`.
fn auth_blob(authenticator: Address, data: &[u8]) -> Bytes {
    [authenticator.as_slice(), data].concat().into()
}

fn run(tx: BaseTransactionSigned) -> TransactionTrace {
    run_on(&genesis(), tx)
}

fn run_on(genesis: &Genesis, tx: BaseTransactionSigned) -> TransactionTrace {
    let outcome = run_block(genesis, &context(genesis), vec![tx]).expect("block runs");
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

    let payload = (
        p256_actor_id,
        ActorConfigAbi {
            authenticator: Eip8130Contracts::P256_AUTHENTICATOR,
            expiry: U48::ZERO,
            scope: 0,
        },
        Bytes::new(),
    )
        .abi_encode_params();
    let mut batch = SignedAccountChanges {
        channel: AccountChangeChannel::Multichain,
        sequence: 0,
        changes: vec![SignedChange {
            change_type: ChangeType::AuthorizeActor,
            payload: payload.clone().into(),
        }],
        signature: Bytes::new(),
    };
    let digest =
        ConfigChangeAuthorizer::changes_digest(alice.address(), genesis.config.chain_id, &batch);
    let batch_signature = alice.signer().sign_hash_sync(&digest).expect("sign");
    batch.signature = auth_blob(Eip8130Constants::K1_AUTHENTICATOR, &batch_signature.as_bytes());

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
