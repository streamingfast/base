//! Firehose chain-specific hooks for Base / OP Stack.
//!
//! Two hooks live here, both installed on a [`reth_firehose::FirehoseWrappedExecutor`]
//! (directly on the live path, indirectly via [`crate::OpFirehoseEvmConfig`] on the pipeline
//! path):
//!
//! * [`OpPostTxExtras`] — re-emits the three fee-vault balance changes that
//!   [`base_common_evm::OpHandler::reward_beneficiary`] applies via `Journal::balance_incr`
//!   during revm's post_execution phase. Revm fires no inspector hooks in that phase, so without
//!   this the L1FeeVault / BaseFeeVault / OperatorFeeVault credits would be invisible to the
//!   tracer.
//!
//! * [`OpPreTxAdjust`] — patches the per-tx [`firehose_tracer::types::TxEvent`] before it
//!   reaches the tracer. OP deposit transaction envelopes carry no nonce field
//!   ([`alloy_op_consensus::TxDeposit::nonce`] returns a literal `0`); the effective nonce is
//!   the sender account's pre-execution nonce, which this hook reads from the DB and writes
//!   into the event.

use alloy_evm::Evm as _;
use alloy_primitives::{Address, Bytes, U256};
use base_common_consensus::Predeploys;
use base_common_evm::{BaseEvm, DEPOSIT_TRANSACTION_TYPE, OpSpecId, OpTxTr};
use firehose_tracer::firehose_debug;
use firehose_tracer::pb::sf::ethereum::r#type::v2::balance_change::Reason;
use firehose_tracer::types::{TxEvent, TxType};
use reth_firehose::inspector::FirehoseInspectorApi;
use reth_firehose::{PostTxExtras, PreTxAdjust};
use reth_revm::revm::{
    context_interface::ContextTr,
    handler::PrecompileProvider,
    inspector::Inspector,
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
};

use base_common_evm::OpContext;

/// Emits the three OP Stack fee vault balance changes that `OpHandler::reward_beneficiary`
/// applies via `Journal::balance_incr` during revm's post_execution phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpPostTxExtras;

impl<DB, I, P> PostTxExtras<BaseEvm<DB, I, P>> for OpPostTxExtras
where
    DB: alloy_evm::Database,
    I: Inspector<OpContext<DB>, EthInterpreter> + FirehoseInspectorApi,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    fn emit_post_tx_extras(
        &self,
        evm: &mut BaseEvm<DB, I, P>,
        gas_used: u64,
        base_fee: u64,
    ) {
        firehose_debug!(
            "OpPostTxExtras::emit_post_tx_extras called (gas_used={}, base_fee={})",
            gas_used,
            base_fee
        );
        // Deposit txs return early in `OpHandler::reward_beneficiary` without crediting any
        // vault — skip them here so we do not emit phantom entries.
        let (enveloped, spec): (Bytes, OpSpecId) = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            if tx.is_deposit() {
                firehose_debug!("OpPostTxExtras: skipping deposit tx");
                return;
            }
            // Non-deposit txs are guaranteed to carry envelope bytes (validated upstream in
            // `OpHandler::validate_env`). If we hit the None branch it's an internal bug —
            // emit nothing rather than panic: the tracer stays consistent with the handler,
            // which would have errored out before reaching `reward_beneficiary`.
            let Some(bytes) = tx.enveloped_tx().cloned() else {
                firehose_debug!(
                    "OpPostTxExtras: non-deposit tx has no enveloped bytes — skipping"
                );
                return;
            };
            (bytes, *ctx.cfg().spec())
        };

        firehose_debug!(
            "OpPostTxExtras: enveloped_len={}, spec={:?}",
            enveloped.len(),
            spec
        );

        // CRITICAL: `calculate_tx_l1_cost` caches its result on `L1BlockInfo.tx_l1_cost`
        // (see `l1block.rs:297`). The OP handler relies on that cache being `None` at the
        // start of each tx — `Handler::execution_result` clears it after `reward_beneficiary`
        // runs (see `handler.rs:324`) so the next tx's `validate_against_state_and_deduct_caller`
        // computes the L1 cost from *its own* envelope. Our hook runs *after* that clear, so
        // calling `calculate_tx_l1_cost` here re-caches *this* tx's value. Without an explicit
        // re-clear below, tx N+1's `tx_cost_with_tx` would short-circuit at the cache check
        // and charge the caller tx N's L1 cost — the post-state diverges, state root mismatch.
        let (l1_cost, operator_fee_cost) = {
            let l1 = evm.ctx_mut().chain_mut();
            let l1_cost = l1.calculate_tx_l1_cost(&enveloped, spec);
            let operator_fee_cost = if spec.is_enabled_in(OpSpecId::ISTHMUS) {
                l1.operator_fee_charge(&enveloped, U256::from(gas_used), spec)
            } else {
                U256::ZERO
            };
            l1.clear_tx_l1_cost();
            (l1_cost, operator_fee_cost)
        };
        let base_fee_amount =
            U256::from(base_fee as u128).saturating_mul(U256::from(gas_used));

        firehose_debug!(
            "OpPostTxExtras: l1_cost={}, base_fee_amount={}, operator_fee_cost={}",
            l1_cost,
            base_fee_amount,
            operator_fee_cost
        );

        // Emission order matches the geth-instrumented op-node: BaseFeeVault (0x...19) →
        // L1FeeVault (0x...1A) → OperatorFeeVault (0x...1B). Note this differs from
        // `OpHandler::reward_beneficiary`'s journal-balance_incr order (L1 → base → operator)
        // — the handler order doesn't matter for state since increments commute, but the
        // firehose trace contract pins ordinals to the geth instrumentation, so we emit in
        // address-ascending order here. Generic coinbase-tip emission already fired (inspector)
        // before these. Skip zero-amount entries to avoid phantom balance changes (Isthmus
        // gate, pre-London basefee, etc.).
        let (db, inspector, _) = evm.components_mut();
        for (vault, amount) in [
            (Predeploys::BASE_FEE_VAULT, base_fee_amount),
            (Predeploys::L1_FEE_VAULT, l1_cost),
            (Predeploys::OPERATOR_FEE_VAULT, operator_fee_cost),
        ] {
            if amount.is_zero() {
                firehose_debug!(
                    "OpPostTxExtras: skipping zero-amount credit for vault={:?}",
                    vault
                );
                continue;
            }
            let old = db.basic(vault).ok().flatten().map(|i| i.balance).unwrap_or(U256::ZERO);
            let new = old.saturating_add(amount);
            firehose_debug!(
                "OpPostTxExtras: emitting vault={:?} old={} new={} delta={}",
                vault,
                old,
                new,
                amount
            );
            inspector
                .tracer_mut()
                .on_balance_change(vault, old, new, Reason::RewardTransactionFee);
        }
    }
}

/// Patches the per-tx [`TxEvent`] for OP Stack deposit transactions.
///
/// Deposit envelopes ([`alloy_op_consensus::TxDeposit`]) carry no nonce field: their
/// `Transaction::nonce()` impl returns a literal `0`. The effective nonce is the sender
/// account's current on-chain nonce (post-Regolith the state transition increments it as part
/// of the deposit). This hook reads the pre-execution nonce from the DB and writes it into the
/// event so the trace matches what geth's instrumented node emits.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpPreTxAdjust;

impl<DB, I, P> PreTxAdjust<BaseEvm<DB, I, P>> for OpPreTxAdjust
where
    DB: alloy_evm::Database,
    I: Inspector<OpContext<DB>, EthInterpreter> + FirehoseInspectorApi,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    fn adjust_tx_event(
        &self,
        evm: &mut BaseEvm<DB, I, P>,
        tx_event: &mut TxEvent,
        sender: Address,
    ) {
        // Only OP deposit txs need any fixup — every other envelope carries a real nonce and
        // is correctly classified as `GasBuy` by the generic depth-0 inspector path.
        if !matches!(tx_event.tx_type, TxType::OptimismDeposit) {
            return;
        }
        debug_assert_eq!(TxType::OptimismDeposit as u8, DEPOSIT_TRANSACTION_TYPE);
        let (db, inspector, _) = evm.components_mut();
        let nonce = db.basic(sender).ok().flatten().map(|i| i.nonce).unwrap_or(0);
        firehose_debug!(
            "OpPreTxAdjust: deposit tx sender={:?} envelope_nonce={} db_nonce={}",
            sender,
            tx_event.nonce,
            nonce
        );
        tx_event.nonce = nonce;

        // Reclassify the depth-0 sender balance change. For deposit txs,
        // `OpHandler::validate_against_state_and_deduct_caller` writes
        // `balance + mint − effective_balance_spending` (op-revm/handler.rs:104-142). For
        // typical deposits (zero gas cost, zero spending) the change is purely the `mint`
        // credit — the geth-instrumented op-node tags this as `IncreaseMint` (reason 18),
        // not `GasBuy` (reason 7). The generic inspector defaults to `GasBuy`; this override
        // takes over for the next depth-0 emission only (single-shot, consumed inside
        // `enter_frame_pre_hook`). Non-deposit OP txs hit the early return above and keep
        // the default `GasBuy` reason.
        inspector.set_root_balance_reason(Reason::IncreaseMint);
    }
}
