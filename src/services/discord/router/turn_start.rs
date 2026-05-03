use super::super::*;
use crate::services::observability::recovery_audit::RecoveryAuditRecord;
use crate::services::observability::turn_lifecycle::{
    SessionStrategyDetails, TurnEvent, TurnLifecycleEmit, emit_turn_lifecycle,
    provider_session_fingerprint,
};
use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessTurnStartOutcome {
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadlessTurnReservation {
    pub(super) user_msg_id: MessageId,
    pub(super) placeholder_msg_id: MessageId,
}

impl HeadlessTurnReservation {
    pub(in crate::services::discord) fn turn_id(&self, channel_id: ChannelId) -> String {
        discord_turn_id(channel_id, self.user_msg_id)
    }
}

pub(super) fn discord_turn_id(channel_id: ChannelId, user_msg_id: MessageId) -> String {
    format!("discord:{}:{}", channel_id.get(), user_msg_id.get())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadlessTurnStartError {
    Conflict(String),
    Internal(String),
}

impl std::fmt::Display for HeadlessTurnStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for HeadlessTurnStartError {}

#[cfg(test)]
pub(super) const HEADLESS_TURN_MESSAGE_ID_BASE: u64 = 9_100_000_000_000_000_000;
#[cfg(not(test))]
const HEADLESS_TURN_MESSAGE_ID_BASE: u64 = 9_100_000_000_000_000_000;
const HEADLESS_TURN_MESSAGE_ID_EPOCH_MILLIS: u64 = 1_700_000_000_000;
const HEADLESS_TURN_MESSAGE_IDS_PER_MILLI: u64 = 1_024;

fn next_headless_turn_message_id() -> MessageId {
    static HEADLESS_TURN_MESSAGE_ID_SEQ: AtomicU64 = AtomicU64::new(0);
    ensure_headless_turn_message_id_seeded(&HEADLESS_TURN_MESSAGE_ID_SEQ);
    MessageId::new(HEADLESS_TURN_MESSAGE_ID_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn ensure_headless_turn_message_id_seeded(sequence: &AtomicU64) {
    if sequence.load(Ordering::Acquire) != 0 {
        return;
    }
    let _ = sequence.compare_exchange(
        0,
        headless_turn_message_id_seed(current_unix_millis(), std::process::id()),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
pub(super) fn headless_turn_message_id_seed(now_millis: u64, process_id: u32) -> u64 {
    headless_turn_message_id_seed_impl(now_millis, process_id)
}

#[cfg(not(test))]
fn headless_turn_message_id_seed(now_millis: u64, process_id: u32) -> u64 {
    headless_turn_message_id_seed_impl(now_millis, process_id)
}

fn headless_turn_message_id_seed_impl(now_millis: u64, process_id: u32) -> u64 {
    let max_elapsed_millis =
        (u64::MAX - HEADLESS_TURN_MESSAGE_ID_BASE - (HEADLESS_TURN_MESSAGE_IDS_PER_MILLI - 1))
            / HEADLESS_TURN_MESSAGE_IDS_PER_MILLI;
    let elapsed_millis = now_millis
        .saturating_sub(HEADLESS_TURN_MESSAGE_ID_EPOCH_MILLIS)
        .min(max_elapsed_millis);
    HEADLESS_TURN_MESSAGE_ID_BASE
        + (elapsed_millis * HEADLESS_TURN_MESSAGE_IDS_PER_MILLI)
        + (u64::from(process_id) % HEADLESS_TURN_MESSAGE_IDS_PER_MILLI)
}

pub(in crate::services::discord) fn reserve_headless_turn() -> HeadlessTurnReservation {
    HeadlessTurnReservation {
        user_msg_id: next_headless_turn_message_id(),
        placeholder_msg_id: next_headless_turn_message_id(),
    }
}

pub(super) fn resolve_session_id_for_current_turn(
    session_id: Option<String>,
    reset_applied: bool,
) -> Option<String> {
    if reset_applied { None } else { session_id }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionResetReason {
    IdleExpired,
    AssistantTurnCap,
}

pub(super) fn session_reset_reason_for_turn(
    session: &DiscordSession,
    now: tokio::time::Instant,
) -> Option<SessionResetReason> {
    if now.duration_since(session.last_active) > super::super::SESSION_MAX_IDLE {
        Some(SessionResetReason::IdleExpired)
    } else if session.assistant_turn_count() >= super::super::SESSION_MAX_ASSISTANT_TURNS {
        Some(SessionResetReason::AssistantTurnCap)
    } else {
        None
    }
}

pub(super) fn session_reset_reason_lifecycle_code(reason: SessionResetReason) -> &'static str {
    match reason {
        SessionResetReason::IdleExpired => "idle_timeout",
        SessionResetReason::AssistantTurnCap => "assistant_turn_cap",
    }
}

pub(super) fn dispatch_reset_lifecycle_code(
    reset_provider_state: bool,
    recreate_tmux: bool,
) -> &'static str {
    match (reset_provider_state, recreate_tmux) {
        (true, true) => "dispatch_provider_reset_recreate_tmux",
        (true, false) => "dispatch_provider_reset",
        (false, true) => "dispatch_recreate_tmux",
        (false, false) => "dispatch_session_reuse",
    }
}

#[derive(Debug, Clone)]
pub(super) struct FormattedSessionRetryContext {
    pub(super) raw_context: String,
    pub(super) formatted_context: String,
    pub(super) audit_record: Option<RecoveryAuditRecord>,
}

pub(super) fn take_session_retry_context(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    turn_id: Option<&str>,
) -> Option<FormattedSessionRetryContext> {
    let context = super::super::turn_bridge::take_session_retry_context_for_turn_with_audit(
        None::<&crate::db::Db>,
        shared.pg_pool.as_ref(),
        channel_id.get(),
        turn_id,
    )?;
    let formatted_context =
        super::response_format::format_session_retry_context(&context.raw_context)?;
    Some(FormattedSessionRetryContext {
        raw_context: context.raw_context,
        formatted_context,
        audit_record: context.audit_record,
    })
}

pub(super) async fn emit_session_strategy_lifecycle(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    turn_id: &str,
    session_key: Option<&str>,
    dispatch_id: Option<&str>,
    provider_session_id: Option<&str>,
    reason: &'static str,
    cli_was_just_spawned: bool,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let provider_session_id = provider_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let resumed = provider_session_id.is_some();
    if resumed && !cli_was_just_spawned {
        return;
    }
    let event = session_strategy_lifecycle_event(provider_session_id, reason);
    let summary = if resumed {
        format!("selected resumed provider session strategy: {reason}")
    } else {
        format!("selected fresh provider session strategy: {reason}")
    };
    let mut emit = TurnLifecycleEmit::new(
        turn_id.to_string(),
        channel_id.get().to_string(),
        event,
        summary,
    );
    if let Some(session_key) = session_key.map(str::trim).filter(|value| !value.is_empty()) {
        emit = emit.session_key(session_key.to_string());
    }
    if let Some(dispatch_id) = dispatch_id.map(str::trim).filter(|value| !value.is_empty()) {
        emit = emit.dispatch_id(dispatch_id.to_string());
    }
    if let Err(error) = emit_turn_lifecycle(pool, emit).await {
        tracing::warn!(
            "failed to emit session strategy lifecycle event for turn {}: {error}",
            turn_id
        );
    }
}

pub(super) fn session_strategy_lifecycle_event(
    provider_session_id: Option<&str>,
    reason: &'static str,
) -> TurnEvent {
    if let Some(provider_session_id) = provider_session_id {
        TurnEvent::SessionResumed(SessionStrategyDetails::resumed(reason, provider_session_id))
    } else {
        TurnEvent::SessionFresh(SessionStrategyDetails::fresh(reason))
    }
}

pub(super) fn cli_just_spawned_for_emit(tmux_session_name: Option<&str>) -> bool {
    match tmux_session_name {
        Some(name) if !name.trim().is_empty() => {
            !crate::services::platform::tmux::has_session(name)
        }
        _ => true,
    }
}

pub(super) async fn log_session_strategy_diagnostic(
    channel_id: ChannelId,
    provider: &ProviderKind,
    dispatch_profile: DispatchProfile,
    session_strategy_reason: &str,
    provider_session_id: Option<&str>,
    adk_session_key: Option<&str>,
    tmux_session_name: Option<&str>,
    recovery_context_present: bool,
    memento_context_loaded: bool,
) {
    let tmux_alive = if let Some(tmux_name) = tmux_session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let tmux_name = tmux_name.to_string();
        tokio::task::spawn_blocking(move || {
            crate::services::platform::tmux::has_session(&tmux_name)
        })
        .await
        .ok()
    } else {
        None
    };
    let provider_session = provider_session_id
        .map(provider_session_fingerprint)
        .unwrap_or_else(|| "none".to_string());
    let tmux_alive_label = match tmux_alive {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    };
    let tmux_label = tmux_session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("-");
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] [session-strategy] channel={} provider={} dispatch={} reason={} resumed={} provider_session_fp={} adk_key_present={} tmux={} tmux_alive={} recovery_context_present={} memento_context_loaded={}",
        channel_id.get(),
        provider.as_str(),
        super::response_format::dispatch_profile_label(dispatch_profile),
        session_strategy_reason,
        provider_session_id.is_some(),
        provider_session,
        adk_session_key.is_some(),
        tmux_label,
        tmux_alive_label,
        recovery_context_present,
        memento_context_loaded
    );
}

pub(super) async fn refresh_session_strategy_after_pending_reset(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    session_id: &mut Option<String>,
    memento_context_loaded: &mut bool,
    session_strategy_reason: &mut &'static str,
) {
    let refreshed = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .map(|session| (session.session_id.clone(), session.memento_context_loaded))
    };
    if let Some((refreshed_session_id, refreshed_memento_context_loaded)) = refreshed {
        if session_id.is_some() && refreshed_session_id.is_none() {
            *session_strategy_reason = "explicit_provider_reset";
        }
        *session_id = refreshed_session_id;
        *memento_context_loaded = refreshed_memento_context_loaded;
    }
}

pub(super) fn load_session_runtime_state(
    sessions: &mut std::collections::HashMap<ChannelId, DiscordSession>,
    channel_id: ChannelId,
) -> Option<(Option<String>, bool, String)> {
    sessions.get_mut(&channel_id).and_then(|session| {
        let current_path = session.validated_path(channel_id)?;
        Some((
            session.session_id.clone(),
            session.memento_context_loaded,
            current_path,
        ))
    })
}

pub(super) fn session_runtime_state_after_redirect(
    sessions: &mut std::collections::HashMap<ChannelId, DiscordSession>,
    original_channel_id: ChannelId,
    effective_channel_id: ChannelId,
    original_state: (Option<String>, bool, String),
) -> (Option<String>, bool, String) {
    if effective_channel_id == original_channel_id {
        return original_state;
    }

    load_session_runtime_state(sessions, effective_channel_id).unwrap_or(original_state)
}

pub(in crate::services::discord) async fn release_mailbox_after_placeholder_post_failure(
    shared: &Arc<SharedData>,
    provider: &super::super::ProviderKind,
    channel_id: ChannelId,
) -> bool {
    let finish = super::super::mailbox_finish_turn(shared, provider, channel_id).await;
    if finish.mailbox_online && finish.has_pending {
        super::super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "intake_placeholder_post_failed",
        );
        true
    } else {
        false
    }
}
