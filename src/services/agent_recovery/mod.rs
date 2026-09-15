//! Live Discord-channel recovery fallback.
//!
//! Same-channel exclusive intake. Mailbox handoff is never used.

pub(crate) mod admission;
pub mod checkpoint;
pub mod context;
pub mod detector;
pub mod handoff;
mod lifecycle;
pub mod policy;
pub mod restore;

use crate::services::provider::ProviderKind;
use std::collections::{HashMap, HashSet};

pub use checkpoint::{
    ChannelRecoveryStatus, ChannelState, CheckpointEvent, CheckpointEventKind, CheckpointPayload,
    DEFAULT_MAX_CHECKPOINT_BYTES, DEFAULT_READ_EVENT_LIMIT, READ_BYTE_CAP, RecoveryLease,
    RecoveryStoreError, RecoveryTransition,
};
pub use detector::{
    DetectorSignal, MailboxStallKind, classify_trigger, mailbox_kind_from_name,
    trigger_from_error_message,
};
pub use handoff::{FallbackSpawnPlan, RecoveryIntake, dual_processing, effective_handles};
pub use policy::{
    OrgAgentInput, OrgChannelInput, PolicyError, RecoveryCatalog, RecoveryConfigWire,
    RecoveryPolicy, TriggerKind, WorkspaceMode, build_recovery_catalog,
    load_org_recovery_catalog_from_yaml, validate_distinct_fallback_agent,
};
pub use restore::{
    RestorePlan, RestoreSessionMode, format_restore_packet, session_mode_for_provider,
};

use checkpoint::{last_n_events, prepare_event};
use policy::ChannelRecoveryBinding;
use restore::build_restore_plan;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObserveInput {
    pub channel_id: String,
    pub primary_turn_id: String,
    pub signal: DetectorSignal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObserveOutcome {
    pub trigger: Option<TriggerKind>,
    pub spawn: Option<FallbackSpawnPlan>,
}

#[derive(Debug)]
pub struct RecoveryRuntime {
    catalog: RecoveryCatalog,
    states: HashMap<String, ChannelState>,
    events: HashMap<String, Vec<CheckpointEvent>>,
    claimed_turns: HashMap<String, String>,
    open_keys: HashSet<(String, String)>,
    spawned: Vec<FallbackSpawnPlan>,
    mailbox_handoff_calls: u32,
    pending_restore: HashMap<String, String>,
    max_checkpoint_bytes: usize,
}

impl Default for RecoveryRuntime {
    fn default() -> Self {
        Self {
            catalog: RecoveryCatalog::default(),
            states: HashMap::new(),
            events: HashMap::new(),
            claimed_turns: HashMap::new(),
            open_keys: HashSet::new(),
            spawned: Vec::new(),
            mailbox_handoff_calls: 0,
            pending_restore: HashMap::new(),
            max_checkpoint_bytes: DEFAULT_MAX_CHECKPOINT_BYTES,
        }
    }
}

impl RecoveryRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_checkpoint_bytes(mut self, max_checkpoint_bytes: usize) -> Self {
        self.max_checkpoint_bytes = max_checkpoint_bytes.max(1);
        self
    }

    pub fn install_catalog(&mut self, catalog: RecoveryCatalog) {
        if self.catalog == catalog {
            return;
        }
        self.catalog = catalog;
    }

    pub fn clear_catalog(&mut self) {
        self.catalog = RecoveryCatalog::default();
    }

    pub fn catalog(&self) -> &RecoveryCatalog {
        &self.catalog
    }

    pub fn mailbox_handoff_calls(&self) -> u32 {
        self.mailbox_handoff_calls
    }

    pub fn spawned(&self) -> &[FallbackSpawnPlan] {
        &self.spawned
    }

    pub fn events(&self, channel_id: &str) -> &[CheckpointEvent] {
        self.events
            .get(channel_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn last_n(&self, channel_id: &str) -> Vec<CheckpointEvent> {
        last_n_events(
            self.events(channel_id),
            DEFAULT_READ_EVENT_LIMIT,
            READ_BYTE_CAP,
        )
    }

    pub fn claim_turn(&mut self, channel_id: &str, turn_id: &str) {
        self.claimed_turns
            .insert(channel_id.to_string(), turn_id.to_string());
    }

    pub fn inherit_workspace(&self, channel_id: &str) -> Option<String> {
        self.states
            .get(channel_id)
            .filter(|state| state.lock_held())?;
        self.binding_for_channel(channel_id)
            .map(|binding| binding.workspace)
    }

    pub fn channel_recovery_intake(
        &self,
        provider: &ProviderKind,
        channel_id: &str,
    ) -> Option<RecoveryIntake> {
        let state = self.states.get(channel_id)?;
        if !state.lock_held() {
            return None;
        }
        if state.status != ChannelRecoveryStatus::FallbackRunning {
            return Some(RecoveryIntake::Skip);
        }
        let Some(holder_provider) = self.writer_provider(state, &state.active_writer_agent_id)
        else {
            return Some(RecoveryIntake::Skip);
        };
        if provider == &holder_provider {
            Some(RecoveryIntake::Allow)
        } else {
            Some(RecoveryIntake::Skip)
        }
    }

    pub fn allows_cli_turn(&self, channel_id: &str, agent_id: &str) -> bool {
        match self.states.get(channel_id) {
            Some(state) if state.lock_held() => {
                state.status == ChannelRecoveryStatus::FallbackRunning
                    && state.active_writer_agent_id == agent_id
            }
            _ => true,
        }
    }

    pub fn allows_cli_turn_for_provider(&self, channel_id: &str, provider: &ProviderKind) -> bool {
        match self.states.get(channel_id) {
            Some(state) if state.lock_held() => {
                state.status == ChannelRecoveryStatus::FallbackRunning
                    && self
                        .writer_provider(state, &state.active_writer_agent_id)
                        .as_ref()
                        == Some(provider)
            }
            _ => true,
        }
    }

    pub fn fallback_prompt_prefix(&self, channel_id: &str) -> Option<String> {
        let state = self.states.get(channel_id)?;
        if !state.lock_held() || state.active_writer_agent_id != state.fallback_agent_id {
            return None;
        }
        let binding = self.binding_for_channel(channel_id)?;
        Some(handoff::format_fallback_prompt(
            &binding,
            &self.last_n(channel_id),
        ))
    }

    pub fn take_restore_packet(&mut self, channel_id: &str) -> Option<String> {
        self.pending_restore.remove(channel_id)
    }

    pub fn note_owner_progress(
        &mut self,
        channel_id: &str,
        payload: CheckpointPayload,
    ) -> Result<Option<CheckpointEvent>, checkpoint::CheckpointError> {
        let Some(binding) = self.binding_for_channel(channel_id) else {
            return Ok(None);
        };
        if binding.policy.as_ref().is_none_or(|policy| !policy.enabled) {
            return Ok(None);
        }
        let event = self.append_event(
            channel_id,
            &binding.owner_agent_id,
            CheckpointEventKind::OwnerProgress,
            payload,
        )?;
        Ok(Some(event))
    }

    pub fn observe(&mut self, input: ObserveInput) -> ObserveOutcome {
        let Some(binding) = self.catalog.channels.get(&input.channel_id).cloned() else {
            return ObserveOutcome::default();
        };
        let policy = binding.policy.as_ref();
        let trigger = classify_trigger(&input.signal, policy);
        let Some(trigger) = trigger else {
            return ObserveOutcome::default();
        };
        let key = (input.channel_id.clone(), input.primary_turn_id.clone());
        if self.open_keys.contains(&key) {
            return ObserveOutcome {
                trigger: Some(trigger),
                spawn: None,
            };
        }
        if self
            .states
            .get(&input.channel_id)
            .is_some_and(ChannelState::lock_held)
        {
            return ObserveOutcome {
                trigger: Some(trigger),
                spawn: None,
            };
        }
        let Some(policy) = policy.cloned() else {
            return ObserveOutcome::default();
        };
        let Some(fallback_provider) = self.catalog.agent_provider(&policy.fallback_agent_id) else {
            return ObserveOutcome::default();
        };
        self.claimed_turns
            .insert(input.channel_id.clone(), input.primary_turn_id.clone());
        let stall_payload = CheckpointPayload::compact(
            binding.owner_agent_id.clone(),
            "",
            format!("stalled: {}", trigger.as_str()),
            "",
            Vec::new(),
            "fallback continues on the original channel",
            "",
        );
        if self
            .append_event(
                &input.channel_id,
                &binding.owner_agent_id,
                CheckpointEventKind::Stall,
                stall_payload,
            )
            .is_err()
        {
            return ObserveOutcome {
                trigger: Some(trigger),
                spawn: None,
            };
        }
        let context = context::RecoveryContext::capture(&binding, &self.catalog);
        let state = self.ensure_state(&binding, &policy.fallback_agent_id);
        state.owner_agent_id = binding.owner_agent_id.clone();
        state.fallback_agent_id = policy.fallback_agent_id.clone();
        state.context = context;
        state.status = ChannelRecoveryStatus::TakeoverPending;
        state.active_writer_agent_id = policy.fallback_agent_id.clone();
        state.primary_turn_id = Some(input.primary_turn_id.clone());
        state.generation += 1;
        self.open_keys.insert(key);
        let events = self.last_n(&input.channel_id);
        let generation = self
            .states
            .get(&input.channel_id)
            .map(|state| state.generation)
            .unwrap_or_default();
        let Some(spawn) =
            FallbackSpawnPlan::from_binding(&binding, fallback_provider, &events, generation)
        else {
            return ObserveOutcome {
                trigger: Some(trigger),
                spawn: None,
            };
        };
        self.spawned.push(spawn.clone());
        ObserveOutcome {
            trigger: Some(trigger),
            spawn: Some(spawn),
        }
    }

    pub fn note_fallback_progress(
        &mut self,
        channel_id: &str,
        kind: CheckpointEventKind,
        payload: CheckpointPayload,
    ) -> Result<Option<CheckpointEvent>, checkpoint::CheckpointError> {
        let Some(state) = self.states.get(channel_id) else {
            return Ok(None);
        };
        if !state.lock_held() {
            return Ok(None);
        }
        let writer = state.active_writer_agent_id.clone();
        let event = self.append_event(channel_id, &writer, kind, payload)?;
        if kind == CheckpointEventKind::Complete
            && let Some(state) = self.states.get_mut(channel_id)
        {
            state.status = ChannelRecoveryStatus::FallbackDone;
        }
        Ok(Some(event))
    }

    pub fn fallback_provider(&self, channel_id: &str) -> Option<ProviderKind> {
        if let Some(state) = self
            .states
            .get(channel_id)
            .filter(|state| state.lock_held())
        {
            return self.writer_provider(state, &state.fallback_agent_id);
        }
        let binding = self.catalog.channels.get(channel_id)?;
        self.catalog
            .agent_provider(&binding.policy.as_ref()?.fallback_agent_id)
    }

    pub fn is_fallback_writer(&self, channel_id: &str, provider: &ProviderKind) -> bool {
        let Some(state) = self.states.get(channel_id) else {
            return false;
        };
        if !state.lock_held() {
            return false;
        }
        state.active_writer_agent_id == state.fallback_agent_id
            && self
                .writer_provider(state, &state.active_writer_agent_id)
                .as_ref()
                == Some(provider)
    }

    pub fn owner_may_retake(&self, channel_id: &str) -> bool {
        let Some(state) = self.states.get(channel_id) else {
            return false;
        };
        if !state.lock_held() {
            return false;
        }
        state.status == ChannelRecoveryStatus::FallbackDone
    }

    pub fn try_restore_owner(
        &mut self,
        channel_id: &str,
        observing_provider: &ProviderKind,
        owner_healthy: bool,
        fallback_inflight: bool,
    ) -> Option<RestorePlan> {
        let binding = self.binding_for_channel(channel_id)?;
        if binding.owner_provider != *observing_provider {
            return None;
        }
        if !self
            .states
            .get(channel_id)
            .is_some_and(ChannelState::lock_held)
        {
            return None;
        }
        if !owner_healthy || fallback_inflight || !self.owner_may_retake(channel_id) {
            return None;
        }
        self.restore_owner(channel_id, true, "owner available")
    }

    pub fn restore_owner(
        &mut self,
        channel_id: &str,
        fallback_succeeded: bool,
        summary: &str,
    ) -> Option<RestorePlan> {
        let binding = self.binding_for_channel(channel_id)?;
        let policy = binding.policy.as_ref()?;
        if self
            .events(channel_id)
            .iter()
            .all(|event| event.kind != CheckpointEventKind::Complete)
        {
            let payload = CheckpointPayload::compact(
                policy.fallback_agent_id.clone(),
                "",
                if fallback_succeeded {
                    "fallback complete"
                } else {
                    "fallback failed"
                },
                "",
                Vec::new(),
                "",
                "",
            );
            let _ = self.append_event(
                channel_id,
                &policy.fallback_agent_id,
                CheckpointEventKind::Complete,
                payload,
            );
        }
        let events = self.last_n(channel_id);
        let mut plan = build_restore_plan(&binding, &events, fallback_succeeded, summary);
        let restore_payload = events
            .last()
            .map(|event| event.payload.clone())
            .unwrap_or_else(|| {
                CheckpointPayload::compact(
                    binding.owner_agent_id.clone(),
                    "",
                    "unknown",
                    "",
                    Vec::new(),
                    "마지막 사용자 메시지부터 재확인",
                    "",
                )
            });
        let _ = self.append_event(
            channel_id,
            &binding.owner_agent_id,
            CheckpointEventKind::Restore,
            restore_payload,
        );
        if let Some(state) = self.states.get_mut(channel_id) {
            state.status = ChannelRecoveryStatus::RestorePending;
            state.active_writer_agent_id = binding.owner_agent_id.clone();
            state.generation += 1;
        }
        self.open_keys
            .retain(|(held_channel, _)| held_channel != channel_id);
        self.pending_restore
            .insert(channel_id.to_string(), plan.packet.clone());
        plan.owner_intake = self.channel_recovery_intake(&binding.owner_provider, channel_id);
        if let Some(fallback_provider) = self.fallback_provider(channel_id) {
            plan.fallback_intake = self.channel_recovery_intake(&fallback_provider, channel_id);
        }
        Some(plan)
    }

    pub fn abort(&mut self, channel_id: &str) {
        if let Some(state) = self.states.get_mut(channel_id) {
            state.status = ChannelRecoveryStatus::Aborted;
            state.active_writer_agent_id = state.owner_agent_id.clone();
            state.generation += 1;
        }
        self.open_keys
            .retain(|(held_channel, _)| held_channel != channel_id);
    }

    fn ensure_state(
        &mut self,
        binding: &ChannelRecoveryBinding,
        fallback_agent_id: &str,
    ) -> &mut ChannelState {
        let context = context::RecoveryContext::capture(binding, &self.catalog);
        self.states
            .entry(binding.channel_id.clone())
            .or_insert_with(|| ChannelState {
                channel_id: binding.channel_id.clone(),
                status: ChannelRecoveryStatus::Owner,
                owner_agent_id: binding.owner_agent_id.clone(),
                fallback_agent_id: fallback_agent_id.to_string(),
                active_writer_agent_id: binding.owner_agent_id.clone(),
                workspace: binding.policy.as_ref().map_or_else(
                    || "inherit".to_string(),
                    |policy| policy.workspace_mode.as_str().to_string(),
                ),
                primary_turn_id: None,
                next_seq: 0,
                generation: 0,
                context,
            })
    }

    fn append_event(
        &mut self,
        channel_id: &str,
        writer_agent_id: &str,
        kind: CheckpointEventKind,
        payload: CheckpointPayload,
    ) -> Result<CheckpointEvent, checkpoint::CheckpointError> {
        let binding = self
            .binding_for_channel(channel_id)
            .ok_or_else(|| checkpoint::CheckpointError::Serialize("unknown channel".to_string()))?;
        let fallback_agent_id = binding
            .policy
            .as_ref()
            .map(|policy| policy.fallback_agent_id.clone())
            .unwrap_or_default();
        let max_checkpoint_bytes = self.max_checkpoint_bytes;
        let seq = self.ensure_state(&binding, &fallback_agent_id).next_seq + 1;
        let event = prepare_event(
            channel_id,
            seq,
            writer_agent_id,
            kind,
            payload,
            max_checkpoint_bytes,
        )?;
        self.ensure_state(&binding, &fallback_agent_id).next_seq = seq;
        self.events
            .entry(channel_id.to_string())
            .or_default()
            .push(event.clone());
        Ok(event)
    }
}

mod durable;
pub(crate) use durable::{
    OperationPlan, PendingOperation, acknowledge_start_durable, active_channels, owner_provider,
    pending_operation, recovery_state, retry_interrupted_durable, try_execution,
};
pub use durable::{
    abort_takeover_durable, allows_cli_turn, allows_cli_turn_for_provider, attach_pg_pool,
    channel_recovery_intake, clear_catalog, complete_turn_durable, fallback_prompt_prefix,
    fallback_provider, hydrate_from_pg, inherit_workspace, install_catalog, lease_for_provider,
    observe_durable, observe_with_checkpoint_durable, take_restore_packet,
    try_restore_owner_durable,
};

#[cfg(test)]
mod tests;
