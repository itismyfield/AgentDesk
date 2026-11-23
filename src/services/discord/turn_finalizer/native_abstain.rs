//! A durable input barrier permits ordinary ownership cleanup only after current source Idle.
use super::*;
use crate::services::discord::{inflight, receipt_settle};
use futures::FutureExt;

#[derive(Clone)]
pub(in crate::services::discord::turn_finalizer) struct Work {
    input: NativePrepared,
}
impl Work {
    pub(super) fn capture(mut input: NativePrepared, shared: &SharedData) -> Result<Self, String> {
        input.ticket = input.bound_ticket(&input.history)?;
        let saved = input
            .history
            .native_settlement(&input.namespace, &input.ticket)?;
        saved
            .barrier(&input.namespace)?
            .ok_or("abstain barrier absent")?;
        if saved.outcome.is_some() || input.key.generation != shared.restart.current_generation {
            return Err("abstain input superseded".into());
        }
        if input.actor.is_some() && input.row_witness.is_none() {
            let row = inflight::load_inflight_state_read_only_result(
                &ProviderKind::Claude,
                input.key.channel_id.get(),
            )?;
            if let Some(row) = row {
                let source = input
                    .ticket
                    .source
                    .as_ref()
                    .ok_or("abstain source absent")?;
                if NativeCleanup::matches_row(&row, input.key, source) {
                    input.row_witness = Some(row);
                }
            }
        }
        Ok(Self { input })
    }
    async fn note(&self, finish: Option<bool>, shared: &Arc<SharedData>) -> Result<bool, String> {
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeAbstain {
                input: Box::new(self.input.clone()),
                finish,
                shared: shared.clone(),
                ack,
            })
            .map_err(|_| "abstain actor unavailable")?;
        rx.await.map_err(|_| "abstain actor acknowledgement lost")?
    }
    pub(super) async fn drive(&self, shared: &Arc<SharedData>) -> Result<bool, String> {
        let input = &self.input;
        let saved = input
            .history
            .native_settlement(&input.namespace, &input.ticket)?;
        saved
            .barrier(&input.namespace)?
            .ok_or("abstain barrier absent")?;
        if saved.outcome.is_some() {
            return Ok(true);
        }
        if input.actor.is_some() {
            shared
                .mailbox(input.key.channel_id)
                .remove_control_custody(input_id(&input.namespace, &input.ticket)?)
                .await;
        }
        if input
            .history
            .native_settlement(&input.namespace, &input.ticket)?
            .outcome
            .is_some()
        {
            return Ok(true);
        }
        if !self.note(None, shared).await? {
            return Ok(true);
        }
        if let Some(actor) = &input.actor {
            let slot = shared.mailbox(input.key.channel_id).snapshot().await;
            if slot
                .cancel_token
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, actor))
            {
                let row = input
                    .row_witness
                    .as_ref()
                    .ok_or("abstain row witness missing")?;
                let mut snapshot =
                    super::super::super::cleanup::SyntheticClaimSnapshot::from_row(row);
                snapshot.recovery_actor = Some(Arc::downgrade(actor));
                let witness = super::super::super::episode::RuntimeWitness {
                    native_prepared_before: Some(input.started_before),
                    probed_tmux: None,
                    claim_snapshot: Some(&snapshot),
                };
                let work = super::super::super::finalize::do_finalize_with_probe(
                    input.key,
                    ProviderKind::Claude,
                    &TerminalEvent::Complete,
                    FinalizeContext::host_terminated(),
                    witness,
                    shared,
                    (None, false),
                );
                if let Err(payload) = std::panic::AssertUnwindSafe(work).catch_unwind().await {
                    return Err(format!(
                        "abstain finalize panic: {}",
                        super::super::super::panic_payload_summary(payload.as_ref())
                    ));
                }
            }
        }
        let wake = self.retry_row(shared).await?;
        if input
            .history
            .native_settlement(&input.namespace, &input.ticket)?
            .outcome
            .is_some()
        {
            return Ok(true);
        }
        self.note(Some(wake), shared).await
    }
    // A released token must never be claimed or debited again just to retire its remaining row.
    async fn retry_row(&self, shared: &Arc<SharedData>) -> Result<bool, String> {
        let input = &self.input;
        if input.actor.is_none() {
            return Ok(false);
        }
        let slot = shared.mailbox(input.key.channel_id).snapshot().await;
        if slot
            .cancel_token
            .as_ref()
            .zip(input.actor.as_ref())
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
        {
            return Err("abstain lease still owned".into());
        }
        let current = inflight::load_inflight_state_read_only_result(
            &ProviderKind::Claude,
            input.key.channel_id.get(),
        )?;
        let Some(row) = current else {
            return Ok(slot.cancel_token.is_none());
        };
        let source = input
            .ticket
            .source
            .as_ref()
            .ok_or("abstain source absent")?;
        if !NativeCleanup::matches_row(&row, input.key, source) {
            return Ok(false);
        }
        let captured = input
            .row_witness
            .as_ref()
            .ok_or("abstain row appeared after approval")?;
        if slot.cancel_token.is_some()
            || row.started_at != captured.started_at
            || row.turn_start_offset != captured.turn_start_offset
        {
            return Err("abstain row authority changed".into());
        }
        if !receipt_settle::record_or_stamp(
            shared.pg_pool.as_ref(),
            &row,
            crate::db::turns::TerminalOutcome::Empty,
        )
        .await
        {
            return Err("abstain terminal receipt pending".into());
        }
        let (result, detail) = inflight::GuardedClearOutcome::for_native_snapshot_with_detail(
            &ProviderKind::Claude,
            &row,
            false,
        );
        if !matches!(
            (result, detail.reason),
            (inflight::GuardedClearOutcome::Cleared, _)
                | (inflight::GuardedClearOutcome::Missing, "not_found")
        ) {
            return Err("abstain row retry deferred".into());
        }
        Ok(true)
    }
}

pub(in crate::services::discord::turn_finalizer) fn handle(
    input: NativePrepared,
    finish: Option<bool>,
    shared: &SharedData,
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
) -> Result<bool, String> {
    let entry = ledger
        .get_mut(&input.key.exact_key())
        .ok_or("abstain entry missing")?;
    let id = input_id(&input.namespace, &input.ticket)?;
    let registered = entry
        .native_inputs
        .get_mut(&id)
        .ok_or("abstain input missing")?;
    if !registered.same_control_scope(&input) {
        return Err("abstain actor changed".into());
    }
    if let (Some(a), Some(b)) = (&registered.row_witness, &input.row_witness)
        && (a.started_at != b.started_at || a.turn_start_offset != b.turn_start_offset)
    {
        return Err("abstain row custody changed".into());
    }
    if registered.row_witness.is_none() {
        registered.row_witness = input.row_witness.clone();
    }
    if input.actor.is_none() {
        if finish.is_some() {
            entry.native_inputs.remove(&id);
        }
        return Ok(true);
    }
    if entry.completion_admission.native_input == Some(true) {
        return Ok(false);
    }
    if entry
        .native_inputs
        .iter()
        .any(|(other, p)| other != &id && p.actor.is_some())
    {
        return Err("abstain cannot consume another primary".into());
    }
    match finish {
        None if entry.phase == Phase::Pending || entry.phase == Phase::Finalizing => {
            entry.phase = Phase::Finalizing;
        }
        Some(wake) if entry.phase == Phase::Finalizing => {
            entry.completion_admission.native_input = Some(true);
            entry.completion_admission.note_mailbox_released();
            entry.phase = Phase::Finalized;
            entry.finalized_at = Some(Instant::now());
            if wake {
                super::super::super::completion_admission::publish_claimed_queue_eligible(
                    shared, entry,
                );
            } else {
                entry.completion_admission.queue_eligible_published = true;
            }
        }
        _ => return Err("abstain phase superseded".into()),
    }
    Ok(true)
}
