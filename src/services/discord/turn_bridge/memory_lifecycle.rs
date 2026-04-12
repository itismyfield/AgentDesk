use super::super::DiscordSession;
use crate::services::discord::settings::{self, RoleBinding};
use crate::services::memory::{
    CaptureRequest, ReflectRequest, SessionEndReason, TokenUsage, build_resolved_memory_backend,
    resolve_memory_role_id,
};
use crate::services::provider::ProviderKind;
use crate::ui::ai_screen::{HistoryItem, HistoryType};
use poise::serenity_prelude::ChannelId;

pub(super) fn spawn_memory_capture_task(
    channel_id: ChannelId,
    capture_memory_settings: settings::ResolvedMemorySettings,
    capture_request: CaptureRequest,
) -> tokio::task::JoinHandle<crate::services::memory::CaptureResult> {
    tokio::spawn(async move {
        let backend = build_resolved_memory_backend(&capture_memory_settings);
        let result = backend.capture(capture_request).await;
        for warning in &result.warnings {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] [memory] capture warning for channel {}: {}",
                channel_id.get(),
                warning
            );
        }
        result
    })
}

pub(super) fn spawn_memory_reflect_task(
    channel_id: ChannelId,
    reflect_memory_settings: settings::ResolvedMemorySettings,
    reflect_request: ReflectRequest,
) -> tokio::task::JoinHandle<crate::services::memory::CaptureResult> {
    tokio::spawn(async move {
        let backend = build_resolved_memory_backend(&reflect_memory_settings);
        let reason = reflect_request.reason.as_str().to_string();
        let result = backend.reflect(reflect_request).await;
        for warning in &result.warnings {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] [memory] reflect warning for channel {} ({}): {}",
                channel_id.get(),
                reason,
                warning
            );
        }
        result
    })
}

fn build_memento_transcript(history: &[HistoryItem]) -> String {
    history
        .iter()
        .filter_map(|item| {
            let content = item.content.trim();
            if content.is_empty() {
                return None;
            }

            let label = match item.item_type {
                HistoryType::User => "User",
                HistoryType::Assistant => "Assistant",
                HistoryType::Error => "Error",
                HistoryType::System => "System",
                HistoryType::ToolUse => "ToolUse",
                HistoryType::ToolResult => "ToolResult",
            };

            Some(format!("[{label}]: {content}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn take_memento_reflect_request(
    session: &mut DiscordSession,
    memory_settings: &settings::ResolvedMemorySettings,
    provider: &ProviderKind,
    role_binding: Option<&RoleBinding>,
    channel_id: u64,
    reason: SessionEndReason,
) -> Option<ReflectRequest> {
    if memory_settings.backend != settings::MemoryBackendKind::Memento
        || !session.memento_context_loaded
        || session.memento_reflected
    {
        return None;
    }

    let session_id = session
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let transcript = build_memento_transcript(&session.history);
    if transcript.trim().is_empty() {
        return None;
    }

    session.memento_reflected = true;
    Some(ReflectRequest {
        provider: provider.clone(),
        role_id: resolve_memory_role_id(role_binding),
        channel_id,
        session_id,
        reason,
        transcript,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TurnEndMemoryPlan {
    pub(super) reflect_reason: Option<SessionEndReason>,
    pub(super) clear_provider_session: bool,
    pub(super) persist_transcript: bool,
    pub(super) spawn_capture: bool,
}

pub(super) fn plan_turn_end_memory(
    session: &DiscordSession,
    backend: settings::MemoryBackendKind,
    is_prompt_too_long: bool,
    resume_failure_detected: bool,
    terminal_session_reset_required: bool,
    should_record_final_turn: bool,
) -> Option<TurnEndMemoryPlan> {
    if session.cleared || is_prompt_too_long {
        return None;
    }

    let persist_transcript = should_record_final_turn;
    let reflect_reason = if terminal_session_reset_required {
        Some(SessionEndReason::LocalSessionReset)
    } else {
        None
    };
    let clear_provider_session = resume_failure_detected || terminal_session_reset_required;

    Some(TurnEndMemoryPlan {
        reflect_reason,
        clear_provider_session,
        persist_transcript,
        spawn_capture: persist_transcript && backend != settings::MemoryBackendKind::Memento,
    })
}

pub(super) fn optional_metric_token_fields(usage: TokenUsage) -> (Option<u64>, Option<u64>) {
    if usage.is_zero() {
        return (None, None);
    }
    (
        if usage.input_tokens > 0 {
            Some(usage.input_tokens)
        } else {
            None
        },
        if usage.output_tokens > 0 {
            Some(usage.output_tokens)
        } else {
            None
        },
    )
}
