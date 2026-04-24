//! Firehose post-tx extras for the Base / OP Stack fee vault predeploys.
//!
//! Revm's `post_execution` phase fires no inspector hooks, so the three `Journal::balance_incr`
//! calls made inside [`base_revm::OpHandler::reward_beneficiary`] (crediting the L1FeeVault,
//! BaseFeeVault, and OperatorFeeVault predeploys) are invisible to the Firehose tracer. This
//! module wires those credits back into the tracer via the
//! [`reth_firehose::PostTxExtras`] hook, mirroring the computation done in the handler.

use alloy_evm::Evm as _;
use alloy_primitives::{Bytes, U256};
use base_alloy_evm::OpEvm;
use base_revm::{
    BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT, OpContext, OpSpecId, OpTxTr,
};
use firehose_tracer::firehose_debug;
use firehose_tracer::pb::sf::ethereum::r#type::v2::balance_change::Reason;
use reth_firehose::PostTxExtras;
use reth_firehose::inspector::FirehoseInspectorApi;
use reth_revm::revm::{
    context_interface::ContextTr,
    handler::PrecompileProvider,
    inspector::Inspector,
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
};

/// Emits the three OP Stack fee vault balance changes that `OpHandler::reward_beneficiary`
/// applies via `Journal::balance_incr` during revm's post_execution phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpPostTxExtras;

impl<DB, I, P> PostTxExtras<OpEvm<DB, I, P>> for OpPostTxExtras
where
    DB: alloy_evm::Database,
    I: Inspector<OpContext<DB>, EthInterpreter> + FirehoseInspectorApi,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    fn emit_post_tx_extras(
        &self,
        evm: &mut OpEvm<DB, I, P>,
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

        let (l1_cost, operator_fee_cost) = {
            let l1 = evm.ctx_mut().chain_mut();
            let l1_cost = l1.calculate_tx_l1_cost(&enveloped, spec);
            let operator_fee_cost = if spec.is_enabled_in(OpSpecId::ISTHMUS) {
                l1.operator_fee_charge(&enveloped, U256::from(gas_used), spec)
            } else {
                U256::ZERO
            };
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

        // Match the handler's emission order: L1 cost → base fee → operator fee.
        // Ordinal ordering within the tx: generic coinbase-tip emission first (inspector), then
        // these three. Skip zero-amount entries to avoid phantom balance changes (Isthmus gate,
        // pre-London basefee, etc.).
        let (db, inspector, _) = evm.components_mut();
        for (vault, amount) in [
            (L1_FEE_RECIPIENT, l1_cost),
            (BASE_FEE_RECIPIENT, base_fee_amount),
            (OPERATOR_FEE_RECIPIENT, operator_fee_cost),
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
