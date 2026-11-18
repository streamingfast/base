//! Firehose tracing of deposit transaction gas, run through the real Base executor and the
//! Firehose wrapper on an Everest genesis, where the EIP-7825 transaction gas limit cap applies.
//!
//! Deposits are exempt from the cap: a deposit executes with its whole declared gas limit, and
//! only the gas it uses is reported. The traced root call and receipt must show the same.

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Sealed, TxKind, U256, address, bytes};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_firehose_tests::{BlockInvariants, TraceContext, run_block};
use base_test_utils::build_test_genesis_everest;
use firehose_tracer::pb::sf::ethereum::r#type::v2::{self as pbeth, TransactionTrace};

/// Contract whose code is `GAS PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN`: it returns the gas left
/// when it starts executing.
const GAS_ECHO: Address = address!("0x0000000000000000000000000000000000006a50");
const DEPOSITOR: Address = address!("0x0000000000000000000000000000000000de9051");
const BENEFICIARY: Address = address!("0x4200000000000000000000000000000000000011");

/// Intrinsic gas of a call with empty calldata.
const INTRINSIC_GAS: u64 = 21_000;
/// Gas the contract uses: `GAS` (2), three `PUSH1` (9), `MSTORE` with one memory word (6).
const EXECUTION_GAS: u64 = 17;

#[test]
fn deposit_root_call_gets_its_full_gas_limit() {
    for gas_limit in [16_000_000, 16_777_216, 16_777_217, 20_000_000] {
        let trx = run_deposit(gas_limit);

        assert_eq!(pbeth::transaction_trace::Type::TrxTypeOptimismDeposit as i32, trx.r#type);
        assert_eq!(pbeth::TransactionTraceStatus::Succeeded as i32, trx.status);
        assert_eq!(gas_limit, trx.gas_limit, "gas limit {gas_limit}");

        let root = &trx.calls[0];
        assert_eq!(GAS_ECHO.as_slice(), root.address.as_slice());
        assert_eq!(gas_limit - INTRINSIC_GAS, root.gas_limit, "gas limit {gas_limit}");
        assert_eq!(EXECUTION_GAS, root.gas_consumed, "gas limit {gas_limit}");
        // `GAS` itself costs 2 before it pushes the gas left.
        let gas_left = U256::from(gas_limit - INTRINSIC_GAS - 2);
        assert_eq!(gas_left.to_be_bytes::<32>().to_vec(), root.return_data);

        let gas_used = INTRINSIC_GAS + EXECUTION_GAS;
        assert_eq!(gas_used, trx.gas_used, "gas limit {gas_limit}");
        let receipt = trx.receipt.as_ref().expect("receipt");
        assert_eq!(gas_used, receipt.cumulative_gas_used, "gas limit {gas_limit}");
    }
}

fn run_deposit(gas_limit: u64) -> TransactionTrace {
    let genesis = genesis();
    let deposit = TxDeposit {
        source_hash: B256::repeat_byte(0x01),
        from: DEPOSITOR,
        to: TxKind::Call(GAS_ECHO),
        mint: 0,
        value: U256::ZERO,
        gas_limit,
        is_system_transaction: false,
        input: Default::default(),
    };
    let tx = BaseTxEnvelope::Deposit(Sealed::new(deposit));
    let outcome = run_block(&genesis, &context(&genesis), vec![tx]).expect("block runs");
    BlockInvariants::assert(&outcome.block).expect("block invariants hold");
    outcome.block.transaction_traces.into_iter().next().expect("one transaction")
}

fn genesis() -> Genesis {
    let mut genesis = build_test_genesis_everest();
    genesis.alloc.insert(
        GAS_ECHO,
        GenesisAccount { code: Some(bytes!("5a60005260206000f3")), ..Default::default() },
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
