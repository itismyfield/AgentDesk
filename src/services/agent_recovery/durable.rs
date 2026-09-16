//! Durable-first publication of channel-local recovery transitions.
//!
//! The state machine runs on an unpublished snapshot. PostgreSQL fences that
//! snapshot and its WAL atomically; only a successful commit may replace the
//! read-side cache or yield a spawn/restore plan. Failed writes need no rollback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use sqlx::PgPool;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::checkpoint::{
    commit_recovery_transition, load_channel_state, load_channel_states, load_checkpoint_events,
};
use super::*;

mod operations;
mod provider_errors;
pub(crate) use operations::{
    OperationPlan, PendingOperation, acknowledge_start_durable, active_channels, owner_provider,
    pending_operation, recovery_state, retry_interrupted_durable, try_execution,
};
pub(crate) use provider_errors::observe_provider_error;

#[cfg(test)]
mod postgres_tests;

#[derive(Default)]
pub(super) struct Coordinator {
    pub(super) runtime: Mutex<RecoveryRuntime>,
    pub(super) pool: Mutex<Option<PgPool>>,
    channels: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

pub(super) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn coordinator() -> &'static Coordinator {
    static COORDINATOR: OnceLock<Coordinator> = OnceLock::new();
    COORDINATOR.get_or_init(Coordinator::default)
}

fn conflict(message: impl Into<String>) -> RecoveryStoreError {
    RecoveryStoreError::Conflict(message.into())
}

impl Coordinator {
    fn pool(&self) -> Result<PgPool, RecoveryStoreError> {
        lock(&self.pool)
            .clone()
            .ok_or_else(|| conflict("durable recovery store is unavailable"))
    }

    async fn channel_guard(&self, channel_id: &str) -> OwnedMutexGuard<()> {
        let gate = lock(&self.channels)
            .entry(channel_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        gate.lock_owned().await
    }

    async fn snapshot(
        &self,
        pool: &PgPool,
        channel_id: &str,
    ) -> Result<RecoveryRuntime, RecoveryStoreError> {
        let mut staged = {
            let runtime = lock(&self.runtime);
            RecoveryRuntime {
                catalog: runtime.catalog.clone(),
                max_checkpoint_bytes: runtime.max_checkpoint_bytes,
                ..RecoveryRuntime::default()
            }
        };
        if let Some(state) = load_channel_state(pool, channel_id).await? {
            let events =
                load_checkpoint_events(pool, channel_id, DEFAULT_READ_EVENT_LIMIT as i64).await?;
            if state.lock_held()
                && let Some(turn_id) = state.primary_turn_id.as_ref()
            {
                staged
                    .open_keys
                    .insert((channel_id.to_string(), turn_id.clone()));
            }
            staged.events.insert(channel_id.to_string(), events);
            staged.states.insert(channel_id.to_string(), state);
        }
        Ok(staged)
    }

    fn publish(&self, channel_id: &str, mut staged: RecoveryRuntime) {
        let mut runtime = lock(&self.runtime);
        runtime
            .open_keys
            .retain(|(channel, _)| channel != channel_id);
        runtime.open_keys.extend(staged.open_keys);
        if let Some(state) = staged.states.remove(channel_id) {
            runtime.states.insert(channel_id.to_string(), state);
        } else {
            runtime.states.remove(channel_id);
        }
        if let Some(events) = staged.events.remove(channel_id) {
            runtime.events.insert(channel_id.to_string(), events);
        } else {
            runtime.events.remove(channel_id);
        }
        // A progress/refresh transaction must not resurrect a consumed packet.
        // Only a newly staged restore publishes a new packet.
        if let Some(packet) = staged.pending_restore.remove(channel_id) {
            runtime
                .pending_restore
                .insert(channel_id.to_string(), packet);
        }
    }

    async fn transition<T>(
        &self,
        channel_id: &str,
        operation: impl FnOnce(&mut RecoveryRuntime) -> Result<Option<T>, RecoveryStoreError>,
    ) -> Result<Option<T>, RecoveryStoreError> {
        let pool = self.pool()?;
        let _channel_guard = self.channel_guard(channel_id).await;
        let mut staged = self.snapshot(&pool, channel_id).await?;
        let before = staged.states.get(channel_id).cloned();
        let before_seq = before.as_ref().map_or(0, |state| state.next_seq);
        let Some(outcome) = operation(&mut staged)? else {
            // Discard the snapshot, including any rejected partial mutation.
            return Ok(None);
        };
        let state = staged
            .states
            .get(channel_id)
            .ok_or_else(|| conflict("transition did not produce a channel state"))?;
        let events: Vec<_> = staged
            .events(channel_id)
            .iter()
            .filter(|event| event.seq > before_seq)
            .cloned()
            .collect();
        let allowed = [before
            .as_ref()
            .map_or(ChannelRecoveryStatus::Owner, |state| state.status)];
        commit_recovery_transition(
            &pool,
            state,
            &events,
            RecoveryTransition {
                expected_generation: before.as_ref().map_or(0, |state| state.generation),
                expected_next_seq: before_seq,
                expected_writer_agent_id: before
                    .as_ref()
                    .map(|state| state.active_writer_agent_id.as_str()),
                allowed_statuses: &allowed,
            },
        )
        .await?;
        self.publish(channel_id, staged);
        Ok(Some(outcome))
    }

    async fn refresh(&self, channel_id: &str) -> Result<(), RecoveryStoreError> {
        let pool = self.pool()?;
        let _channel_guard = self.channel_guard(channel_id).await;
        let staged = self.snapshot(&pool, channel_id).await?;
        self.publish(channel_id, staged);
        Ok(())
    }
}

pub fn attach_pg_pool(pool: PgPool) {
    *lock(&coordinator().pool) = Some(pool);
}

pub fn install_catalog(catalog: RecoveryCatalog) {
    lock(&coordinator().runtime).install_catalog(catalog);
}

pub fn clear_catalog() {
    lock(&coordinator().runtime).clear_catalog();
}

pub async fn channel_recovery_intake(
    provider: &ProviderKind,
    channel_id: &str,
) -> Option<RecoveryIntake> {
    if lock(&coordinator().pool).is_none() && !recovery_enabled(channel_id) {
        return None;
    }
    if let Err(error) = coordinator().refresh(channel_id).await {
        tracing::warn!(channel_id, error = %error, "recovery intake cannot verify durable ownership");
        return Some(RecoveryIntake::Skip);
    }
    lock(&coordinator().runtime).channel_recovery_intake(provider, channel_id)
}

fn recovery_enabled(channel_id: &str) -> bool {
    lock(&coordinator().runtime)
        .catalog
        .policy_for_channel(channel_id)
        .is_some_and(|policy| policy.enabled)
}

pub fn allows_cli_turn(channel_id: &str, agent_id: &str) -> bool {
    lock(&coordinator().runtime).allows_cli_turn(channel_id, agent_id)
}

pub fn allows_cli_turn_for_provider(channel_id: &str, provider: &ProviderKind) -> bool {
    lock(&coordinator().runtime).allows_cli_turn_for_provider(channel_id, provider)
}

pub fn inherit_workspace(channel_id: &str) -> Option<String> {
    lock(&coordinator().runtime).inherit_workspace(channel_id)
}

pub fn fallback_prompt_prefix(channel_id: &str) -> Option<String> {
    lock(&coordinator().runtime).fallback_prompt_prefix(channel_id)
}

pub fn take_restore_packet(channel_id: &str) -> Option<String> {
    lock(&coordinator().runtime).take_restore_packet(channel_id)
}

pub fn fallback_provider(channel_id: &str) -> Option<ProviderKind> {
    lock(&coordinator().runtime).fallback_provider(channel_id)
}

/// Capture when registering a turn, not when its completion arrives.
pub fn lease_for_provider(channel_id: &str, provider: &ProviderKind) -> Option<RecoveryLease> {
    let runtime = lock(&coordinator().runtime);
    if let Some(state) = runtime.states.get(channel_id) {
        if !state.lock_held()
            && runtime
                .binding_for_channel(channel_id)
                .is_none_or(|binding| {
                    !binding.policy.as_ref().is_some_and(|policy| policy.enabled)
                        || binding.owner_agent_id != state.owner_agent_id
                })
        {
            return None;
        }
        if runtime
            .writer_provider(state, &state.active_writer_agent_id)
            .as_ref()
            != Some(provider)
        {
            return None;
        }
        return Some(RecoveryLease::from_state(state));
    }
    let binding = runtime.binding_for_channel(channel_id)?;
    if !binding.policy.as_ref().is_some_and(|policy| policy.enabled) {
        return None;
    }
    (binding.owner_provider == *provider).then(|| RecoveryLease {
        channel_id: channel_id.to_string(),
        generation: 0,
        active_writer_agent_id: binding.owner_agent_id.clone(),
    })
}

pub async fn complete_turn_durable(
    lease: &RecoveryLease,
    payload: CheckpointPayload,
) -> Result<(), RecoveryStoreError> {
    coordinator()
        .transition(&lease.channel_id, |runtime| {
            apply_completion(runtime, lease, payload)
        })
        .await?;
    Ok(())
}

fn apply_completion(
    runtime: &mut RecoveryRuntime,
    lease: &RecoveryLease,
    mut payload: CheckpointPayload,
) -> Result<Option<()>, RecoveryStoreError> {
    if let Some(previous) =
        super::restore::latest_progress_or_complete(runtime.events(&lease.channel_id))
    {
        if payload.goal.is_empty() {
            payload.goal = previous.payload.goal.clone();
        }
        if payload.next.is_empty() {
            payload.next = previous.payload.next.clone();
        }
        if payload.files.is_empty() {
            payload.files = previous.payload.files.clone();
        }
        if payload.decisions.is_empty() {
            payload.decisions = previous.payload.decisions.clone();
        }
        if payload.last_user_message.is_empty() {
            payload.last_user_message = previous.payload.last_user_message.clone();
        }
        if payload.progress == "agent turn complete" && !previous.payload.progress.is_empty() {
            payload.progress = format!("{}\nAgent turn complete.", previous.payload.progress);
        }
    }
    let binding = runtime
        .binding_for_channel(&lease.channel_id)
        .ok_or_else(|| conflict("recovery binding is unavailable"))?;
    let fallback = binding
        .policy
        .as_ref()
        .ok_or_else(|| conflict("recovery policy is absent"))?
        .fallback_agent_id
        .clone();
    let state = runtime.ensure_state(&binding, &fallback);
    if state.generation != lease.generation
        || state.active_writer_agent_id != lease.active_writer_agent_id
    {
        return Err(conflict(
            "turn completion does not own its original recovery lease",
        ));
    }
    let event = if state.active_writer_agent_id == state.owner_agent_id {
        runtime.note_owner_progress(&lease.channel_id, payload)
    } else if matches!(
        state.status,
        ChannelRecoveryStatus::TakeoverPending | ChannelRecoveryStatus::FallbackRunning
    ) {
        runtime.note_fallback_progress(&lease.channel_id, CheckpointEventKind::Complete, payload)
    } else {
        return Err(conflict("fallback completion is not running"));
    }
    .map_err(|error| conflict(error.message()))?;
    if event.is_some()
        && let Some(state) = runtime.states.get_mut(&lease.channel_id)
        && state.status == ChannelRecoveryStatus::RestorePending
    {
        state.status = ChannelRecoveryStatus::Restored;
        runtime
            .open_keys
            .retain(|(channel, _)| channel != &lease.channel_id);
    }
    Ok(event.map(|_| ()))
}

pub async fn observe_durable(input: ObserveInput) -> ObserveOutcome {
    observe_with_checkpoint_durable(input, None, None).await
}
pub async fn observe_with_checkpoint_durable(
    input: ObserveInput,
    checkpoint: Option<CheckpointPayload>,
    workspace: Option<String>,
) -> ObserveOutcome {
    if !recovery_enabled(&input.channel_id) {
        return ObserveOutcome::default();
    }
    let result = coordinator()
        .transition(&input.channel_id, |runtime| {
            if let Some(payload) = checkpoint {
                runtime
                    .note_owner_progress(&input.channel_id, payload)
                    .map_err(|error| conflict(error.message()))?;
            }
            let mut outcome = runtime.observe(input.clone());
            if let (Some(spawn), Some(workspace)) = (outcome.spawn.as_mut(), workspace) {
                spawn.cwd = workspace.clone();
                if let Some(context) = runtime
                    .states
                    .get_mut(&input.channel_id)
                    .and_then(|state| state.context.as_mut())
                {
                    context.workspace = workspace;
                }
            }
            Ok(outcome.spawn.is_some().then_some(outcome))
        })
        .await;
    match result {
        Ok(Some(outcome)) => outcome,
        Ok(None) => ObserveOutcome::default(),
        Err(error) => {
            tracing::warn!(channel_id = %input.channel_id, error = %error,
                "recovery takeover did not commit; suppressing fallback spawn");
            ObserveOutcome::default()
        }
    }
}

pub async fn try_restore_owner_durable(
    channel_id: &str,
    observing_provider: &ProviderKind,
    owner_healthy: bool,
    fallback_inflight: bool,
) -> Option<RestorePlan> {
    if !owner_healthy || fallback_inflight {
        return None;
    }
    match coordinator()
        .transition(channel_id, |runtime| {
            Ok(runtime.try_restore_owner(
                channel_id,
                observing_provider,
                owner_healthy,
                fallback_inflight,
            ))
        })
        .await
    {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(channel_id, error = %error,
                "recovery restore did not commit; owner remains fenced");
            None
        }
    }
}

pub async fn abort_takeover_durable(channel_id: &str, generation: i64, fallback_agent_id: &str) {
    let result = coordinator()
        .transition(channel_id, |runtime| {
            let Some(state) = runtime.states.get(channel_id) else {
                return Err(conflict("recovery state is absent"));
            };
            if state.generation != generation
                || state.active_writer_agent_id != fallback_agent_id
                || state.status != ChannelRecoveryStatus::FallbackRunning
            {
                return Err(conflict("failed spawn no longer owns the takeover"));
            }
            runtime
                .append_event(
                    channel_id,
                    fallback_agent_id,
                    CheckpointEventKind::Stall,
                    CheckpointPayload::compact(
                        fallback_agent_id,
                        "",
                        "fallback startup aborted",
                        "",
                        Vec::new(),
                        "retry primary ownership",
                        "",
                    ),
                )
                .map_err(|error| conflict(error.message()))?;
            runtime.abort(channel_id);
            Ok(Some(()))
        })
        .await;
    if let Err(error) = result {
        tracing::warn!(channel_id, error = %error, "fallback compensation did not commit");
    }
}

pub async fn hydrate_from_pg(pool: &PgPool) {
    attach_pg_pool(pool.clone());
    let states = match load_channel_states(pool).await {
        Ok(states) => states,
        Err(error) => {
            tracing::error!(error = %error, "failed to hydrate durable recovery ownership");
            return;
        }
    };
    for state in states {
        if let Err(error) = coordinator()
            .transition(&state.channel_id, |runtime| {
                runtime.retry_interrupted_launch(&state.channel_id)
            })
            .await
        {
            tracing::warn!(channel_id = %state.channel_id, error = %error, "restart recovery intent remains fenced");
        }
        if let Err(error) = coordinator().refresh(&state.channel_id).await {
            tracing::warn!(channel_id = %state.channel_id, error = %error,
                "failed to hydrate recovery channel");
        }
    }
}
