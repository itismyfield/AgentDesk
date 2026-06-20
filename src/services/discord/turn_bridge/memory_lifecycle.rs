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

pub(in crate::services::discord) fn spawn_memory_reflect_task(
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
                "  [{ts}] [memory] reflect warning for channel {} reason={}: {}",
                channel_id.get(),
                reason,
                warning
            );
        }
        result
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackgroundMemoryTaskKind {
    Reflect,
    Capture,
}

impl BackgroundMemoryTaskKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reflect => "reflect",
            Self::Capture => "capture",
        }
    }
}

pub(super) struct BackgroundMemoryTask {
    pub(super) kind: BackgroundMemoryTaskKind,
    pub(super) handle: tokio::task::JoinHandle<crate::services::memory::CaptureResult>,
}

const BACKGROUND_MEMORY_TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

enum BackgroundMemoryTaskOutcome {
    Completed(crate::services::memory::CaptureResult),
    JoinFailed(tokio::task::JoinError),
    TimedOut,
}

struct ObservedBackgroundMemoryTask {
    kind: BackgroundMemoryTaskKind,
    outcome: BackgroundMemoryTaskOutcome,
}

async fn observe_one_background_memory_task(
    mut task: BackgroundMemoryTask,
    timeout: std::time::Duration,
) -> ObservedBackgroundMemoryTask {
    let outcome = tokio::select! {
        result = &mut task.handle => match result {
            Ok(result) => BackgroundMemoryTaskOutcome::Completed(result),
            Err(err) => BackgroundMemoryTaskOutcome::JoinFailed(err),
        },
        _ = tokio::time::sleep(timeout) => {
            task.handle.abort();
            BackgroundMemoryTaskOutcome::TimedOut
        }
    };
    ObservedBackgroundMemoryTask {
        kind: task.kind,
        outcome,
    }
}

pub(super) async fn observe_background_memory_tasks(
    channel_id: ChannelId,
    tasks: Vec<BackgroundMemoryTask>,
    accumulated_memory_input_tokens: &mut u64,
    accumulated_memory_output_tokens: &mut u64,
) {
    observe_background_memory_tasks_with_timeout(
        channel_id,
        tasks,
        BACKGROUND_MEMORY_TASK_TIMEOUT,
        accumulated_memory_input_tokens,
        accumulated_memory_output_tokens,
    )
    .await;
}

async fn observe_background_memory_tasks_with_timeout(
    channel_id: ChannelId,
    tasks: Vec<BackgroundMemoryTask>,
    timeout: std::time::Duration,
    accumulated_memory_input_tokens: &mut u64,
    accumulated_memory_output_tokens: &mut u64,
) {
    let mut observers = tokio::task::JoinSet::new();
    for task in tasks {
        observers.spawn(observe_one_background_memory_task(task, timeout));
    }

    while let Some(join_result) = observers.join_next().await {
        match join_result {
            Ok(observed) => {
                let task_kind = observed.kind.as_str();
                match observed.outcome {
                    BackgroundMemoryTaskOutcome::Completed(result) => {
                        *accumulated_memory_input_tokens = accumulated_memory_input_tokens
                            .saturating_add(result.token_usage.input_tokens);
                        *accumulated_memory_output_tokens = accumulated_memory_output_tokens
                            .saturating_add(result.token_usage.output_tokens);
                    }
                    BackgroundMemoryTaskOutcome::JoinFailed(err) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] [memory] {task_kind} background task join failed for channel {}: {}",
                            channel_id.get(),
                            err
                        );
                    }
                    BackgroundMemoryTaskOutcome::TimedOut => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] [memory] {task_kind} background task timed out after {}s for channel {} — skipping token accounting",
                            timeout.as_secs(),
                            channel_id.get(),
                        );
                    }
                }
            }
            Err(err) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] [memory] background observer task join failed for channel {}: {}",
                    channel_id.get(),
                    err
                );
            }
        }
    }
}

#[cfg(test)]
mod background_memory_task_tests {
    use super::{
        BackgroundMemoryTask, BackgroundMemoryTaskKind, observe_background_memory_tasks,
        observe_background_memory_tasks_with_timeout,
    };
    use crate::services::memory::{CaptureResult, TokenUsage};
    use poise::serenity_prelude::ChannelId;
    use std::time::{Duration, Instant};

    fn completed_task(
        kind: BackgroundMemoryTaskKind,
        input_tokens: u64,
        output_tokens: u64,
    ) -> BackgroundMemoryTask {
        BackgroundMemoryTask {
            kind,
            handle: tokio::spawn(async move {
                CaptureResult {
                    token_usage: TokenUsage {
                        input_tokens,
                        output_tokens,
                    },
                    ..CaptureResult::default()
                }
            }),
        }
    }

    fn pending_task(kind: BackgroundMemoryTaskKind) -> BackgroundMemoryTask {
        BackgroundMemoryTask {
            kind,
            handle: tokio::spawn(async move { std::future::pending::<CaptureResult>().await }),
        }
    }

    #[tokio::test]
    async fn observes_reflect_and_capture_background_memory_tasks() {
        let mut input_tokens = 0;
        let mut output_tokens = 0;

        observe_background_memory_tasks(
            ChannelId::new(42),
            vec![
                completed_task(BackgroundMemoryTaskKind::Reflect, 3, 5),
                completed_task(BackgroundMemoryTaskKind::Capture, 7, 11),
            ],
            &mut input_tokens,
            &mut output_tokens,
        )
        .await;

        assert_eq!(
            (input_tokens, output_tokens),
            (10, 16),
            "reflect and capture handles must both be awaited and token-accounted"
        );
    }

    #[tokio::test]
    async fn observes_single_background_memory_task() {
        let mut input_tokens = 0;
        let mut output_tokens = 0;

        observe_background_memory_tasks(
            ChannelId::new(42),
            vec![completed_task(BackgroundMemoryTaskKind::Capture, 13, 17)],
            &mut input_tokens,
            &mut output_tokens,
        )
        .await;

        assert_eq!(
            (input_tokens, output_tokens),
            (13, 17),
            "single-task behavior must keep token accounting unchanged"
        );
    }

    #[tokio::test]
    async fn observes_background_memory_tasks_under_one_timeout_window() {
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let started_at = Instant::now();

        observe_background_memory_tasks_with_timeout(
            ChannelId::new(42),
            vec![
                pending_task(BackgroundMemoryTaskKind::Reflect),
                completed_task(BackgroundMemoryTaskKind::Capture, 7, 11),
            ],
            Duration::from_millis(25),
            &mut input_tokens,
            &mut output_tokens,
        )
        .await;

        assert!(
            started_at.elapsed() < Duration::from_millis(250),
            "pending reflect plus completed capture must not serialize into two timeout windows"
        );
        assert_eq!(
            (input_tokens, output_tokens),
            (7, 11),
            "completed task accounting should survive another task timing out"
        );
    }
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

pub(in crate::services::discord) fn take_memento_reflect_request(
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
        channel_name: session.channel_name.clone(),
        session_id,
        reason,
        transcript,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TurnEndMemoryPlan {
    pub(super) session_end_reason: Option<SessionEndReason>,
    pub(super) clear_provider_session: bool,
    pub(super) persist_transcript: bool,
    pub(super) analyze_recall_feedback: bool,
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
    if session.cleared {
        return None;
    }

    let persist_transcript = should_record_final_turn;
    if is_prompt_too_long {
        return Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: false,
            persist_transcript,
            analyze_recall_feedback: backend == settings::MemoryBackendKind::Memento,
            spawn_capture: false,
        });
    }

    // #3591: 턴수 기반(100턴) 세션 리셋 제거. 컨텍스트 폭주는 auto-compact가 관리.
    let session_end_reason = if terminal_session_reset_required {
        Some(SessionEndReason::LocalSessionReset)
    } else {
        None
    };
    let clear_provider_session = resume_failure_detected || terminal_session_reset_required;

    Some(TurnEndMemoryPlan {
        session_end_reason,
        clear_provider_session,
        persist_transcript,
        analyze_recall_feedback: backend == settings::MemoryBackendKind::Memento,
        spawn_capture: persist_transcript,
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
