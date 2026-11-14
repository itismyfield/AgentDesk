//! Exact actor custody grants an explicit close; its durable intent survives the existing tail.
use super::*;
use crate::services::discord::inflight::GuardedClearOutcome;
use crate::services::tui_input::human_frame::history::v2::{
    Registry,
    native::{Disposition, Ticket, control::Intent},
};
use crate::services::turn_orchestrator::policy_close::ControlCustody;

struct Custody {
    input: String,
    namespace: String,
    ticket: Ticket,
    key: TurnKey,
    actor: std::sync::Weak<CancelToken>,
    history: Registry,
}

impl ControlCustody for Custody {
    fn input(&self) -> &str {
        &self.input
    }
    fn owns(
        &self,
        channel: ChannelId,
        actor: Option<&Arc<CancelToken>>,
        message: Option<serenity::model::id::MessageId>,
        nonce: Option<&str>,
    ) -> bool {
        channel == self.key.channel_id
            && message.map(|m| m.get()) == Some(self.key.user_msg_id)
            && self.key.matches_episode_nonce(nonce)
            && self
                .actor
                .upgrade()
                .as_ref()
                .zip(actor)
                .is_some_and(|(expected, current)| Arc::ptr_eq(expected, current))
    }

    fn admit(&self, operator: bool) -> Result<(), String> {
        let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
            &ProviderKind::Claude,
            self.key.channel_id.get(),
        )?;
        self.history.try_native_control(
            &self.namespace,
            &self.ticket,
            if operator {
                Disposition::OperatorReleased
            } else {
                Disposition::Cancelled
            },
            row.map(serde_json::to_value)
                .transpose()
                .map_err(|e| e.to_string())?,
        )
    }
}

impl NativePrepared {
    pub(in crate::services::discord::turn_finalizer) fn same_control_scope(
        &self,
        other: &Self,
    ) -> bool {
        self.key.exact_key() == other.key.exact_key()
            && self.history.same_control_registry(&other.history)
            && self.namespace == other.namespace
            && self.ticket.key == other.ticket.key
            && self.ticket.extra == other.ticket.extra
            && self.ticket.owning_episode == other.ticket.owning_episode
            && (self.ticket.source.is_none() || self.ticket.source == other.ticket.source)
            && match (&self.actor, &other.actor) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }

    pub(in crate::services::discord::turn_finalizer) async fn install_control(
        &self,
        history: &Registry,
        shared: &Arc<SharedData>,
    ) -> Result<(), String> {
        let Some(actor) = &self.actor else {
            return Ok(());
        };
        let custody = Arc::new(Custody {
            input: serde_json::to_string(&(&self.namespace, &self.ticket.key))
                .map_err(|e| e.to_string())?,
            namespace: self.namespace.clone(),
            ticket: self.ticket.clone(),
            key: self.key,
            actor: Arc::downgrade(actor),
            history: history.clone(),
        });
        let mailbox = shared
            .mailbox_peek(self.key.channel_id)
            .ok_or("native control actor missing")?;
        if !mailbox.install_control_custody(custody).await {
            return Err("native control custody registration refused".into());
        }
        Ok(())
    }

    pub(in crate::services::discord::turn_finalizer) async fn remove_control(
        &self,
        history: &Registry,
        shared: &Arc<SharedData>,
    ) -> Result<(), String> {
        let ticket = self.bound_ticket(history)?;
        let settlement = history.native_settlement(&self.namespace, &ticket)?;
        if settlement.extra.get("any_key") == Some(&serde_json::Value::Bool(true))
            || settlement.outcome.is_some()
        {
            return Err("native no-key witness superseded".into());
        }
        history.try_native_not_sent(&self.namespace, &ticket)?;
        if self.actor.is_some() {
            let input = serde_json::to_string(&(&self.namespace, &self.ticket.key))
                .map_err(|e| e.to_string())?;
            let mailbox = shared
                .mailbox_peek(self.key.channel_id)
                .ok_or("native no-key actor missing")?;
            if !mailbox.remove_control_custody(input).await {
                return Err("native no-key custody removal unconfirmed".into());
            }
        }
        Ok(())
    }

    async fn settle_control(
        &self,
        shared: &Arc<SharedData>,
        history: &Registry,
    ) -> Result<Option<bool>, String> {
        let ticket = self.bound_ticket(history)?;
        let settlement = history.native_settlement(&self.namespace, &ticket)?;
        let Some(value) = settlement.extra.get("control_intent") else {
            return Ok(None);
        };
        let intent: Intent = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        let outcome = settlement
            .outcome
            .as_ref()
            .ok_or("native control outcome absent")?;
        if intent.namespace != self.namespace || intent.ticket != ticket {
            return Err("native control settlement identity changed".into());
        }
        if !outcome.extra.contains_key("native_proof")
            && !intent.validates(&self.namespace, &ticket, outcome)
        {
            return Err("native control proof invalid".into());
        }
        history.try_native_control(
            &self.namespace,
            &ticket,
            intent.disposition.clone(),
            intent.row.clone(),
        )?;
        let slot = shared.mailbox(self.key.channel_id).snapshot().await;
        if slot
            .cancel_token
            .as_ref()
            .zip(self.actor.as_ref())
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
        {
            let actor = self.actor.as_ref().ok_or("native control actor missing")?;
            let finish = super::super::super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
                shared, &ProviderKind::Claude, self.key.channel_id, serenity::model::id::MessageId::new(self.key.user_msg_id),
                ticket.owning_episode.as_ref().map(|e| e.turn_nonce.clone()), self.started_before, Some(actor.clone())
            ).await;
            if finish.removed_token.is_none() || finish.persistence_error.is_some() {
                return Ok(None);
            }
            super::super::super::saturating_decrement_global_active(shared);
        }
        let slot = shared.mailbox(self.key.channel_id).snapshot().await;
        let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
            &ProviderKind::Claude,
            self.key.channel_id.get(),
        )?;
        let mut wake = slot.cancel_token.is_none();
        if let Some(row) = row {
            let value = serde_json::to_value(&row).map_err(|e| e.to_string())?;
            if intent.runtime_matches(&value) {
                if slot.cancel_token.is_some() {
                    return Err("native control row authority changed".into());
                }
                let (clear, detail) = GuardedClearOutcome::for_native_snapshot_with_detail(
                    &ProviderKind::Claude,
                    &row,
                );
                if !matches!(
                    (clear, detail.reason),
                    (GuardedClearOutcome::Cleared, _) | (GuardedClearOutcome::Missing, "not_found")
                ) {
                    return Ok(None);
                }
            } else {
                wake = false;
            }
        }
        history.ack_native_cleanup(&self.namespace, &ticket, outcome)?;
        let record = history.native_boot_record(&self.namespace, &ticket)?;
        let root = crate::services::discord::runtime_store::runtime_root()
            .ok_or("native control root unavailable")?;
        crate::services::discord::inflight::persist_native_boot_record(&root, &record)?;
        Ok(Some(wake))
    }
}

pub(in crate::services::discord::turn_finalizer) async fn reconcile(
    entry: &mut LedgerEntry,
    shared: &Arc<SharedData>,
) {
    if entry.completion_admission.native_input != Some(false) || entry.native_inputs.is_empty() {
        return;
    }
    let mut wake = true;
    for input in entry.native_inputs.values() {
        match input.settle_control(shared, &input.history).await {
            Ok(Some(allowed)) => wake &= allowed,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "native control debt retained");
                return;
            }
        }
    }
    entry.completion_admission.native_input = Some(true);
    entry.completion_admission.note_mailbox_released();
    entry.phase = Phase::Finalized;
    entry.finalized_at = Some(Instant::now());
    if wake {
        super::super::completion_admission::publish_claimed_queue_eligible(shared, entry);
    } else {
        entry.completion_admission.queue_eligible_published = true;
    }
}
