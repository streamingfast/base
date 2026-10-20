//! Firehose chain-specific hooks for Base / OP Stack.
//!
//! Two hooks live here, both installed on a [`reth_firehose::FirehoseWrappedExecutor`]
//! (directly on the live path, indirectly via [`crate::OpFirehoseEvmConfig`] on the pipeline
//! path):
//!
//! * [`OpPostTxExtras`] — re-emits the three fee-vault balance changes that
//!   [`base_common_evm::BaseHandler::reward_beneficiary`] applies via `Journal::balance_incr`
//!   during revm's `post_execution` phase. Revm fires no inspector hooks in that phase, so without
//!   this the `L1FeeVault` / `BaseFeeVault` / `OperatorFeeVault` credits would be invisible to the
//!   tracer.
//!
//! * [`OpPreTxAdjust`] — patches the per-tx [`firehose_tracer::types::TxEvent`] before it
//!   reaches the tracer. OP deposit transaction envelopes carry no nonce field
//!   ([`alloy_op_consensus::TxDeposit::nonce`] returns a literal `0`); the effective nonce is
//!   the sender account's pre-execution nonce, which this hook reads from the DB and writes
//!   into the event.

use alloy_evm::Evm as _;
use alloy_primitives::{Address, B256, Bytes, U256};
use base_common_consensus::{AccountChange, Predeploys};
use base_common_evm::{
    BaseContext, BaseEvm, BaseSpecId, BaseTransaction, BaseTxTr, BaseUpgrade,
    DEPOSIT_TRANSACTION_TYPE, Eip8130PhaseStatuses,
};
use firehose_tracer::{
    firehose_debug,
    pb::sf::ethereum::r#type::v2::{
        AccountAbstractionAccountChange, AccountAbstractionPhase, AccountAbstractionPhaseCall,
        AccountAbstractionTrxInput, AccountAbstractionTrxOutput, account_abstraction_account_change,
        balance_change::Reason,
    },
    types::{TxEvent, TxType},
    utils::u256_to_protobuf,
};
use reth_firehose::{
    PostTxExtras, PreTxAdjust,
    inspector::{FirehoseInspectorApi, RootJournalReasons},
};
use reth_revm::revm::{
    context::TxEnv,
    context_interface::{Block as _, ContextTr},
    handler::PrecompileProvider,
    inspector::Inspector,
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
};

/// Emits the three OP Stack fee vault balance changes that `BaseHandler::reward_beneficiary`
/// applies via `Journal::balance_incr` during revm's `post_execution` phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpPostTxExtras;

impl<DB, I, P> PostTxExtras<BaseEvm<DB, I, P>> for OpPostTxExtras
where
    DB: alloy_evm::Database,
    I: Inspector<BaseContext<DB>, EthInterpreter> + FirehoseInspectorApi,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    fn emit_post_tx_extras(&self, evm: &mut BaseEvm<DB, I, P>, gas_used: u64, base_fee: u64) {
        firehose_debug!(
            "OpPostTxExtras::emit_post_tx_extras called (gas_used={}, base_fee={})",
            gas_used,
            base_fee
        );
        // An EIP-8130 transaction credits its fee recipients on the journal inside its root call
        // (see `OpPreTxAdjust`), so only its committed phase count is left to record. The
        // receipt builder has not consumed the phase statuses yet: it runs at commit.
        if evm.ctx().tx().is_eip8130() {
            let (_, inspector, _) = evm.components_mut();
            inspector.tracer_mut().set_account_abstraction_trx_output(AccountAbstractionTrxOutput {
                committed_phases: Eip8130PhaseStatuses::committed_phases(),
            });
            return;
        }

        // Deposit txs return early in `BaseHandler::reward_beneficiary` without crediting any
        // vault — skip them here so we do not emit phantom entries.
        let (enveloped, spec): (Bytes, BaseSpecId) = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            if tx.is_deposit() {
                firehose_debug!("OpPostTxExtras: skipping deposit tx");
                return;
            }
            // Non-deposit txs are guaranteed to carry envelope bytes (validated upstream in
            // `BaseHandler::validate_env`). If we hit the None branch it's an internal bug —
            // emit nothing rather than panic: the tracer stays consistent with the handler,
            // which would have errored out before reaching `reward_beneficiary`.
            let Some(bytes) = tx.enveloped_tx().cloned() else {
                firehose_debug!("OpPostTxExtras: non-deposit tx has no enveloped bytes — skipping");
                return;
            };
            (bytes, *ctx.cfg().spec())
        };

        firehose_debug!("OpPostTxExtras: enveloped_len={}, spec={:?}", enveloped.len(), spec);

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
            let operator_fee_cost = if spec.is_enabled_in(BaseUpgrade::Isthmus) {
                l1.operator_fee_charge(&enveloped, U256::from(gas_used), spec)
            } else {
                U256::ZERO
            };
            l1.clear_tx_l1_cost();
            (l1_cost, operator_fee_cost)
        };
        let base_fee_amount = U256::from(base_fee as u128).saturating_mul(U256::from(gas_used));

        firehose_debug!(
            "OpPostTxExtras: l1_cost={}, base_fee_amount={}, operator_fee_cost={}",
            l1_cost,
            base_fee_amount,
            operator_fee_cost
        );

        // Emission order matches the geth-instrumented op-node: BaseFeeVault (0x...19) →
        // L1FeeVault (0x...1A) → OperatorFeeVault (0x...1B). Note this differs from
        // `BaseHandler::reward_beneficiary`'s journal-balance_incr order (L1 → base → operator)
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
            inspector.tracer_mut().on_balance_change(vault, old, new, Reason::RewardTransactionFee);
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
    I: Inspector<BaseContext<DB>, EthInterpreter> + FirehoseInspectorApi,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    fn adjust_tx_event(
        &self,
        evm: &mut BaseEvm<DB, I, P>,
        tx_env: &BaseTransaction<TxEnv>,
        tx_event: &mut TxEvent,
        sender: Address,
    ) {
        if matches!(tx_event.tx_type, TxType::AccountAbstraction) {
            Self::adjust_eip8130_tx_event(evm, tx_env, tx_event, sender);
            return;
        }

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
        // `BaseHandler::validate_against_state_and_deduct_caller` writes
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

impl OpPreTxAdjust {
    /// Fills the fields of an EIP-8130 transaction (type `0x79`) the generic mapper cannot read
    /// from the envelope, and records its fees from the journal.
    ///
    /// The executor charges the payer, increments the nonce and applies the account changes on
    /// the journal before the root call, and refunds the payer and credits the beneficiary and
    /// the fee vaults after the last call, all outside the calls below the root call. The
    /// inspector records them inside the root call with the reasons set here.
    fn adjust_eip8130_tx_event<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        tx_env: &BaseTransaction<TxEnv>,
        tx_event: &mut TxEvent,
        sender: Address,
    ) where
        DB: alloy_evm::Database,
        I: Inspector<BaseContext<DB>, EthInterpreter> + FirehoseInspectorApi,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
    {
        let Some(parts) = tx_env.eip8130_parts() else {
            panic!(
                "Firehose cannot trace transaction {}: an EIP-8130 transaction reached the EVM \
                 without its envelope",
                tx_event.hash
            );
        };
        let signed = &parts.signed;
        let tx = signed.tx();
        let beneficiary = evm.ctx().block().beneficiary();

        let payer = signed.resolved_payer(sender).unwrap_or_else(|_| {
            panic!(
                "Firehose cannot trace transaction {}: its open payer signature does not recover",
                tx_event.hash
            )
        });

        let account_changes = tx
            .account_changes
            .iter()
            .map(|change| match change {
                AccountChange::Delegation(delegation) => AccountAbstractionAccountChange {
                    change: Some(account_abstraction_account_change::Change::DelegationTarget(
                        if delegation.target.is_zero() {
                            Vec::new()
                        } else {
                            delegation.target.to_vec()
                        },
                    )),
                },
                // Keystore entries are gated by Zenith, which only genesis can activate.
                AccountChange::Create(_) | AccountChange::ConfigChange(_) => panic!(
                    "Firehose cannot trace transaction {}: the block model has no Keystore \
                     account change, so this node cannot emit this block",
                    tx_event.hash
                ),
            })
            .collect();

        let phases = tx
            .calls
            .iter()
            .map(|calls| AccountAbstractionPhase {
                calls: calls
                    .iter()
                    .map(|call| AccountAbstractionPhaseCall {
                        to: call.to.to_vec(),
                        value: u256_to_protobuf(call.value),
                        data: call.data.to_vec(),
                        call_index: 0,
                    })
                    .collect(),
            })
            .collect();

        tx_event.to = None;
        // `sender_auth` ends with the sender's `r || s || v` secp256k1 signature, after the
        // 20-byte authenticator selector when the sender is named.
        let sender_auth = signed.sender_auth();
        if let Some(signature) = sender_auth.get(sender_auth.len().saturating_sub(65)..)
            && signature.len() == 65
        {
            tx_event.r = B256::from_slice(&signature[..32]);
            tx_event.s = B256::from_slice(&signature[32..64]);
            // Typed transactions record the y-parity, trimmed like a big integer.
            let y_parity = signature[64].saturating_sub(27);
            tx_event.v = Some(if y_parity == 0 {
                Bytes::new()
            } else {
                Bytes::copy_from_slice(&[y_parity])
            });
        }
        tx_event.account_abstraction_trx_input = Some(AccountAbstractionTrxInput {
            nonce_key: u256_to_protobuf(tx.nonce_key),
            payer: payer.to_vec(),
            valid_after: tx.valid_after,
            valid_before: tx.valid_before,
            metadata: tx.metadata.to_vec(),
            account_changes,
            phases,
        });

        let (_, inspector, _) = evm.components_mut();
        inspector.set_root_journal_reasons(RootJournalReasons {
            decreases: vec![(payer, Reason::GasBuy)],
            increases: vec![
                (payer, Reason::GasRefund),
                (beneficiary, Reason::RewardTransactionFee),
                (Predeploys::BASE_FEE_VAULT, Reason::RewardTransactionFee),
                (Predeploys::L1_FEE_VAULT, Reason::RewardTransactionFee),
                (Predeploys::OPERATOR_FEE_VAULT, Reason::RewardTransactionFee),
            ],
        });
    }
}
