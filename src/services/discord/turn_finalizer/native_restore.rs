//! Boot custody proves cleanup authority without recreating a live actor or a monotonic cutoff.
use super::*;
use crate::services::discord::inflight::{self, GuardedClearOutcome, InflightTurnState};
use crate::services::tui_input::human_frame::history::v2::{
    Registry,
    native::{Disposition, RestoredRecord},
};
use crate::services::tui_o::writer::input_facts::native::Evidence;

pub(in crate::services::discord) struct BootInputs {
    inputs: Vec<RestoredInput>,
}

#[derive(Clone)]
pub(in crate::services::discord::turn_finalizer) struct RestoredInput {
    record: RestoredRecord,
    row: Option<InflightTurnState>,
    key: TurnKey,
    pub(in crate::services::discord::turn_finalizer) settled: bool,
}

#[derive(Clone, Copy)]
pub(in crate::services::discord::turn_finalizer) enum RestoreStep {
    Register,
    Settle,
    Abstain(bool),
}

impl RestoredInput {
    fn capture(record: RestoredRecord, generation: u64) -> Result<Self, String> {
        let ticket = &record.settlement.ticket;
        let episode = ticket.owning_episode.as_ref();
        let message = episode
            .map(|e| e.user_message_id)
            .or_else(|| ticket.key["identity"]["primary"].as_u64())
            .ok_or("native boot identity missing")?;
        if message == 0
            || ticket.key["attempt"]["provider"] != "claude"
            || episode.is_some_and(|e| e.generation == 0 || e.generation >= generation)
        {
            return Err("native boot generation mismatch".into());
        }
        let key = TurnKey::new(
            ChannelId::new(ticket.channel_id),
            message,
            episode.map_or(generation, |e| e.generation),
        )
        .with_episode_nonce(episode.map(|e| e.turn_nonce.as_str()));
        let row = inflight::load_inflight_state_read_only_result(
            &ProviderKind::Claude,
            ticket.channel_id,
        )?;
        let input = Self {
            settled: false,
            record,
            row,
            key,
        };
        input.proof()?;
        Ok(input)
    }

    pub(in crate::services::discord::turn_finalizer) fn identity(
        &self,
    ) -> (
        &str,
        &crate::services::tui_input::human_frame::history::v2::native::Ticket,
    ) {
        (&self.record.namespace, &self.record.settlement.ticket)
    }

    pub(in crate::services::discord::turn_finalizer) fn refreshed(
        &self,
        history: &Registry,
    ) -> Result<Self, String> {
        let (namespace, original) = self.identity();
        let ticket = history.native_ticket(namespace, &original.key)?;
        if ticket.key != original.key
            || ticket.owning_episode != original.owning_episode
            || ticket.extra != original.extra
            || (original.source.is_some() && original.source != ticket.source)
        {
            return Err("native boot identity changed".into());
        }
        if let Some(source) = &ticket.source {
            source.validate(&ticket.key)?;
        }
        let mut current = self.clone();
        current.record = history.native_boot_record(namespace, &ticket)?;
        current.proof()?;
        Ok(current)
    }

    pub(in crate::services::discord::turn_finalizer) async fn settle_saved(
        &self,
        shared: &Arc<SharedData>,
    ) -> Result<bool, String> {
        let history = Registry::runtime()?;
        let (namespace, ticket) = self.identity();
        super::super::terminal_handler::prepare_saved_notice(&history, namespace, ticket, shared)
            .await?;
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeBoot {
                inputs: vec![self.clone()],
                step: RestoreStep::Settle,
                shared: shared.clone(),
                ack,
            })
            .map_err(|_| "native boot cleanup actor unavailable")?;
        rx.await.map_err(|_| "native boot cleanup reply lost")??;
        Ok(history
            .native_settlement(namespace, ticket)?
            .cleanup_complete)
    }

    pub(in crate::services::discord::turn_finalizer) async fn abstain_idle(
        mut self,
        shared: &Arc<SharedData>,
        history: &Registry,
    ) -> Result<(), String> {
        self.record.settlement =
            history.native_settlement(&self.record.namespace, &self.record.settlement.ticket)?;
        self.record
            .settlement
            .barrier(&self.record.namespace)?
            .ok_or("boot abstain barrier missing")?;
        if self.record.settlement.outcome.is_some() {
            return Ok(());
        }
        let mut wake = self.record.settlement.ticket.owning_episode.is_some();
        if wake {
            let slot = shared.mailbox(self.key.channel_id).snapshot().await;
            if slot.active_user_message_id.map(|m| m.get()) == Some(self.key.user_msg_id)
                && self
                    .key
                    .matches_episode_nonce(slot.active_turn_nonce.as_deref())
            {
                return Err("boot abstain cannot claim live actor".into());
            }
            if let Some(row) = inflight::load_inflight_state_read_only_result(
                &ProviderKind::Claude,
                self.key.channel_id.get(),
            )? {
                if self.owns_row(&row) {
                    let captured = self.row.as_ref().ok_or("boot abstain row appeared")?;
                    if slot.cancel_token.is_some()
                        || captured.started_at != row.started_at
                        || captured.turn_start_offset != row.turn_start_offset
                        || !self.owns_row(captured)
                    {
                        return Err("boot abstain row authority changed".into());
                    }
                    if !crate::services::discord::receipt_settle::record_or_stamp(
                        shared.pg_pool.as_ref(),
                        &row,
                        crate::db::turns::TerminalOutcome::Empty,
                    )
                    .await
                    {
                        return Err("boot abstain receipt pending".into());
                    }
                    let (clear, detail) = GuardedClearOutcome::for_native_snapshot_with_detail(
                        &ProviderKind::Claude,
                        &row,
                    );
                    if !matches!(
                        (clear, detail.reason),
                        (GuardedClearOutcome::Cleared, _)
                            | (GuardedClearOutcome::Missing, "not_found")
                    ) {
                        return Err("boot abstain row CAS deferred".into());
                    }
                } else {
                    wake = false;
                }
            } else {
                wake = slot.cancel_token.is_none();
            }
        }
        self.preserve_terminal(history)?;
        self.settled = true;
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeBoot {
                inputs: vec![self],
                step: RestoreStep::Abstain(wake),
                shared: shared.clone(),
                ack,
            })
            .map_err(|_| "boot abstain actor unavailable")?;
        rx.await.map_err(|_| "boot abstain acknowledgement lost")?
    }

    /// The boot half of the native completion records, before Settle removes the row.
    async fn record_completion(&self, shared: &Arc<SharedData>, history: &Registry) {
        let proof = self.record.settlement.outcome.as_ref().map(|o| &o.extra["native_proof"]);
        let Some(Ok(evidence)) = proof.map(|p| serde_json::from_value(p.clone())) else {
            return;
        };
        use super::super::terminal_handler::native_supply::native_completion::record;
        record(shared, history, &evidence, self.key, self.row.as_ref()).await;
    }

    fn proof(&self) -> Result<(), String> {
        validate_record(&self.record)
    }

    fn owns_row(&self, row: &InflightTurnState) -> bool {
        let ticket = &self.record.settlement.ticket;
        ticket.source.as_ref().is_some_and(|source| {
            row.provider == "claude"
                && row.channel_id == ticket.channel_id
                && row.effective_finalizer_turn_id() == self.key.user_msg_id
                && row.born_generation == self.key.generation
                && self.key.matches_episode_nonce(row.turn_nonce.as_deref())
                && row.session_id.as_deref() == Some(source.session_id.as_str())
                && row.output_path.as_deref() == source.transcript.to_str()
                && row.hosted_execution_nonce.as_deref()
                    == source.target["execution_nonce"].as_str()
        })
    }

    fn preserve_terminal(&self, history: &Registry) -> Result<(), String> {
        let root = crate::services::discord::runtime_store::runtime_root()
            .ok_or("native boot root unavailable")?;
        let record = history
            .try_native_boot_record(&self.record.namespace, &self.record.settlement.ticket)?;
        inflight::persist_native_boot_record(&root, &record)
    }

    async fn drive(&mut self, shared: &Arc<SharedData>, history: &Registry) -> Result<(), String> {
        self.record.settlement = history
            .try_native_settlement(&self.record.namespace, &self.record.settlement.ticket)?;
        self.proof()?;
        let Some(outcome) = self.record.settlement.outcome.clone() else {
            return Ok(());
        };
        if self.record.settlement.cleanup_complete {
            self.preserve_terminal(history)?;
            self.settled = true;
            return Ok(());
        }
        if outcome.disposition == Disposition::WithdrawnNotExecuted
            && !self.record.settlement.withdrawal_notice
        {
            return Err("native boot withdrawal notice pending".into());
        }
        let primary = self.record.settlement.ticket.owning_episode.is_some();
        let mut wake = primary;
        if primary {
            let snapshot = shared.mailbox(self.key.channel_id).snapshot().await;
            if snapshot.active_user_message_id.map(|id| id.get()) == Some(self.key.user_msg_id)
                && self
                    .key
                    .matches_episode_nonce(snapshot.active_turn_nonce.as_deref())
            {
                return Err("native boot cannot claim a live episode actor".into());
            }
            let current = inflight::load_inflight_state_read_only_result(
                &ProviderKind::Claude,
                self.key.channel_id.get(),
            )?;
            match current {
                Some(row) if self.owns_row(&row) => {
                    if let Some(value) = outcome.extra.get("control_proof") {
                        let intent: crate::services::tui_input::human_frame::history::v2::native::control::Intent = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                        if !intent.runtime_matches(
                            &serde_json::to_value(&row).map_err(|e| e.to_string())?,
                        ) {
                            return Err("native boot control runtime changed".into());
                        }
                    }
                    let captured = self
                        .row
                        .as_ref()
                        .ok_or("native boot row appeared after preparation")?;
                    if snapshot.cancel_token.is_some()
                        || !self.owns_row(captured)
                        || captured.started_at != row.started_at
                        || captured.turn_start_offset != row.turn_start_offset
                    {
                        return Err("native boot row authority changed".into());
                    }
                    let (cleared, detail) = GuardedClearOutcome::for_native_snapshot_with_detail(
                        &ProviderKind::Claude,
                        &row,
                    );
                    if !matches!(
                        (cleared, detail.reason),
                        (GuardedClearOutcome::Cleared, _)
                            | (GuardedClearOutcome::Missing, "not_found")
                    ) {
                        return Err("native boot row CAS deferred".into());
                    }
                }
                Some(_) => wake = false,
                None => wake = snapshot.cancel_token.is_none(),
            }
        }
        let first = history.try_ack_native_cleanup(
            &self.record.namespace,
            &self.record.settlement.ticket,
            &outcome,
        )?;
        self.preserve_terminal(history)?;
        self.settled = true;
        if first && wake {
            super::super::super::turn_completion_events::publish_queue_eligible_completion_event(
                shared,
                self.key.channel_id,
                Some(self.key.user_msg_id),
            );
        }
        Ok(())
    }
}

impl BootInputs {
    pub(in crate::services::discord) fn prepare(
        root: &std::path::Path,
        generation: u64,
    ) -> Result<Self, String> {
        let records = inflight::native_boot_records(root, &ProviderKind::Claude)?;
        for record in &records {
            RestoredInput::capture(record.clone(), generation)?;
        }
        if records.is_empty() {
            return Ok(Self { inputs: Vec::new() });
        }
        let history = Registry::at(root.join("injected_frames.json"));
        let adopted = history.adopt_native_records(&records)?;
        let mut inputs = std::collections::BTreeMap::new();
        for record in adopted {
            if record.settlement.extra.get("not_sent") == Some(&serde_json::Value::Bool(true)) {
                if record.settlement.outcome.is_some()
                    || record.settlement.extra.get("any_key")
                        == Some(&serde_json::Value::Bool(true))
                {
                    return Err("native boot no-key proof conflicts".into());
                }
                continue;
            }
            let id = serde_json::to_string(&(&record.namespace, &record.settlement.ticket.key))
                .map_err(|e| e.to_string())?;
            inputs.insert(id, RestoredInput::capture(record, generation)?);
        }
        Ok(Self {
            inputs: inputs.into_values().collect(),
        })
    }

    pub(in crate::services::discord) async fn restore(
        self,
        shared: &Arc<SharedData>,
    ) -> Result<(), String> {
        self.send(self.inputs.clone(), RestoreStep::Register, shared)
            .await?;
        let history = Registry::runtime()?;
        for input in &self.inputs {
            super::super::terminal_handler::prepare_saved_notice(
                &history,
                &input.record.namespace,
                &input.record.settlement.ticket,
                shared,
            )
            .await?;
            input.record_completion(shared, &history).await;
        }
        self.send(self.inputs.clone(), RestoreStep::Settle, shared)
            .await
    }

    async fn send(
        &self,
        inputs: Vec<RestoredInput>,
        step: RestoreStep,
        shared: &Arc<SharedData>,
    ) -> Result<(), String> {
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeBoot {
                inputs,
                step,
                shared: shared.clone(),
                ack,
            })
            .map_err(|_| "native boot actor unavailable")?;
        rx.await.map_err(|_| "native boot acknowledgement lost")?
    }
}

pub(super) async fn restore(
    inputs: Vec<RestoredInput>,
    step: RestoreStep,
    shared: &Arc<SharedData>,
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
) -> Result<(), String> {
    let history = Registry::runtime()?;
    let mut failure = None;
    for mut input in inputs {
        let result = match step {
            RestoreStep::Register => Ok(()),
            RestoreStep::Settle => input.drive(shared, &history).await,
            RestoreStep::Abstain(_) => input.proof().and_then(|()| {
                if input.settled
                    && input.record.settlement.outcome.is_none()
                    && input
                        .record
                        .settlement
                        .barrier(&input.record.namespace)?
                        .is_some()
                {
                    Ok(())
                } else {
                    Err("boot abstain acknowledgement invalid".into())
                }
            }),
        };
        if let Err(error) = result {
            failure.get_or_insert(error);
        }
        let key = input.key;
        let id =
            serde_json::to_string(&(&input.record.namespace, &input.record.settlement.ticket.key))
                .map_err(|e| e.to_string())?;
        let entry = ledger.entry(key.exact_key()).or_insert_with(|| {
            LedgerEntry::registered(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::None,
                None,
                CompletionAdmissionPlan::Immediate,
                None,
            )
        });
        if let Some(known) = entry.native_boot_inputs.get(&id)
            && known.settled
            && known.record.settlement.ticket == input.record.settlement.ticket
            && known.record.settlement.barrier(&known.record.namespace)?
                == input.record.settlement.barrier(&input.record.namespace)?
        {
            input.settled = true;
        }
        entry.native_boot_inputs.insert(id, input);
        let settled = entry.native_boot_inputs.values().all(|input| input.settled);
        entry.completion_admission.native_input = Some(settled);
        entry.phase = if settled {
            Phase::Finalized
        } else {
            Phase::Pending
        };
        if settled {
            entry.finalized_at = Some(Instant::now());
            entry.completion_admission.note_mailbox_released();
            if matches!(step, RestoreStep::Abstain(true)) {
                super::super::completion_admission::publish_claimed_queue_eligible(shared, entry);
            } else {
                entry.completion_admission.queue_eligible_published = true;
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

pub(in crate::services::discord) async fn boot_ready(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) -> bool {
    if !super::enabled() {
        return true;
    }
    shared.turn_finalizer.native_boot_ready(channel).await
}

impl TurnFinalizer {
    pub(in crate::services::discord) async fn native_boot_ready(&self, channel: ChannelId) -> bool {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::NativeBootReady { channel, ack })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

#[cfg(test)]
#[path = "native_restore_gc_tests.rs"]
mod gc_tests;

pub(in crate::services::discord::turn_finalizer) fn validate_record(
    record: &RestoredRecord,
) -> Result<(), String> {
    let Some(outcome) = record.settlement.outcome.as_ref() else {
        record.settlement.barrier(&record.namespace)?;
        return Ok(());
    };
    if let Some(value) = outcome.extra.get("control_proof") {
        let intent: crate::services::tui_input::human_frame::history::v2::native::control::Intent =
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        if !intent.validates(&record.namespace, &record.settlement.ticket, outcome)
            || record.settlement.extra.get("any_key") != Some(&serde_json::Value::Bool(true))
            || intent.row.as_ref().is_some_and(|row| !intent.owns_row(row))
        {
            return Err("native boot control proof mismatch".into());
        }
        return Ok(());
    }
    let evidence: Evidence = serde_json::from_value(
        outcome
            .extra
            .get("native_proof")
            .ok_or("native boot result proof missing")?
            .clone(),
    )
    .map_err(|e| e.to_string())?;
    let source = evidence
        .ticket
        .source
        .as_ref()
        .ok_or("native boot source missing")?;
    if source.provider != "claude"
        || source.session_id.is_empty()
        || source.file == [0, 0]
        || (outcome.disposition == Disposition::Executed
            && evidence.membership_row.as_deref().is_none_or(str::is_empty))
        || evidence.namespace != record.namespace
        || evidence.ticket != record.settlement.ticket
        || evidence.disposition != outcome.disposition
        || outcome.range != Some([evidence.range.0, evidence.range.1])
        || evidence.range.0 < source.offset
        || evidence.range.0 >= evidence.range.1
        || source.target != evidence.ticket.key["attempt"]["target"]
        || source.target["execution_nonce"]
            .as_str()
            .is_none_or(str::is_empty)
        || record.settlement.extra.get("any_key") != Some(&serde_json::Value::Bool(true))
        || evidence
            .raw
            .as_ref()
            .is_some_and(|raw| !raw.seals(&evidence))
    {
        return Err("native boot result proof mismatch".into());
    }
    record
        .settlement
        .admits_native(&record.namespace, outcome)?;
    match outcome.disposition {
        Disposition::Executed | Disposition::Cancelled => {
            let member = evidence
                .membership_range
                .ok_or("native boot membership missing")?;
            let closer = evidence.closer_range.ok_or("native boot closer missing")?;
            if evidence.run.is_none()
                || member.0 < source.offset
                || member.0 >= member.1
                || member.1 != evidence.range.0
                || closer.0 < member.1
                || closer.0 >= closer.1
                || closer.1 != evidence.range.1
            {
                return Err("native boot run proof mismatch".into());
            }
        }
        Disposition::WithdrawnNotExecuted | Disposition::TransferredUnverified => {
            if evidence.run.is_some()
                || evidence.membership_range.is_some()
                || evidence.closer_range.is_some()
                || evidence.membership_row.is_some()
            {
                return Err("native boot withdrawal carries run proof".into());
            }
        }
        Disposition::OperatorReleased => {
            return Err("native boot control proof unavailable".into());
        }
    }
    Ok(())
}
