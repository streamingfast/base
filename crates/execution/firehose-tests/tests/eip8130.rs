//! Firehose tracing of EIP-8130 transactions (type `0x79`), run through the real Base executor
//! and the Firehose wrapper on an Everest genesis.

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, Bytes, U256, address, bytes};
use alloy_signer::SignerSync;
use base_common_consensus::{
    AccountChange, BaseTransactionSigned, Call, Delegation, Eip8130Constants, Eip8130Signed,
    Predeploys, TxEip8130,
};
use base_firehose_tests::{BlockInvariants, TraceContext, run_block};
use base_test_utils::{Account, build_test_genesis_everest};
use firehose_tracer::pb::sf::ethereum::r#type::v2::{
    self as pbeth, TransactionTrace, balance_change::Reason,
};

/// Contract whose code is `PUSH1 0 PUSH1 0 REVERT`.
const REVERTER: Address = address!("0x00000000000000000000000000000000000ad0ad");
const NONCE_MANAGER: Address = address!("0x813000000000000000000000000000000000aa01");
const BENEFICIARY: Address = address!("0x4200000000000000000000000000000000000011");

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

fn run(tx: BaseTransactionSigned) -> TransactionTrace {
    let genesis = genesis();
    let outcome = run_block(&genesis, &context(&genesis), vec![tx]).expect("block runs");
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
