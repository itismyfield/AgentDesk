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

// #5168: the memento reflect-gate tests below belong to
// `note_memento_context_from_turn` / `take_memento_reflect_request`, not to the
// background-task observer this module is named for. They live here on purpose:
// `scripts/test_lane_coverage_baseline.txt` is an append-forbidden ratchet and
// no curated CI lane selects a NEW `memory_lifecycle` test module, so declaring
// one would register as baseline growth and fail the gate. Same reasoning as the
// `forget_ratio_tests` module comment in `services/memory/memento_throttle.rs`.
#[cfg(test)]
mod background_memory_task_tests {
    use super::super::recall_feedback::transcript_contains_explicit_memento_tool_call;
    use super::{
        BackgroundMemoryTask, BackgroundMemoryTaskKind, DiscordSession,
        note_memento_context_from_turn, observe_background_memory_tasks,
        observe_background_memory_tasks_with_timeout, settings, take_memento_reflect_request,
    };
    use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
    use crate::services::memory::{CaptureResult, SessionEndReason, TokenUsage};
    use crate::services::provider::ProviderKind;
    use crate::ui::ai_screen::{HistoryItem, HistoryType};
    use poise::serenity_prelude::ChannelId;
    use std::time::{Duration, Instant};

    fn memento_settings() -> settings::ResolvedMemorySettings {
        settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        }
    }

    /// A live session that has already exchanged one turn: everything the
    /// reflect request needs EXCEPT the memento context gate.
    fn live_session() -> DiscordSession {
        DiscordSession {
            session_id: Some("provider-session-1".to_string()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: vec![
                HistoryItem {
                    item_type: HistoryType::User,
                    content: "remember the deploy order".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: "noted".to_string(),
                },
            ],
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(4242),
            channel_name: Some("ops".to_string()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        }
    }

    /// The transcript a path-B turn produces when the MODEL calls memento
    /// itself through the MCP (Claude namespacing).
    fn model_memento_tool_call_transcript() -> Vec<SessionTranscriptEvent> {
        vec![
            SessionTranscriptEvent {
                kind: SessionTranscriptEventKind::ToolUse,
                tool_name: Some("mcp__memento__context".to_string()),
                summary: None,
                content: "{\"structured\":true}".to_string(),
                status: None,
                is_error: false,
            },
            SessionTranscriptEvent {
                kind: SessionTranscriptEventKind::ToolResult,
                tool_name: Some("mcp__memento__context".to_string()),
                summary: None,
                content: "{\"fragments\":[]}".to_string(),
                status: None,
                is_error: false,
            },
        ]
    }

    fn take_reflect(
        session: &mut DiscordSession,
    ) -> Option<crate::services::memory::ReflectRequest> {
        take_memento_reflect_request(
            session,
            &memento_settings(),
            &ProviderKind::Claude,
            None,
            4242,
            SessionEndReason::IdleExpiry,
        )
    }

    /// #5168 P0: the whole production chain, end to end —
    /// transcript events -> `transcript_contains_explicit_memento_tool_call`
    /// -> `note_memento_context_from_turn` -> `take_memento_reflect_request`.
    ///
    /// Slice 2 (`abb72d261`) deleted the only production writer of
    /// `memento_context_loaded`, which made `take_memento_reflect_request`
    /// return `None` unconditionally in production and silently killed both the
    /// session-end and the idle-expiry reflect. This test fails if any link in
    /// that chain is removed again.
    #[test]
    fn model_memento_tool_call_arms_session_end_reflect() {
        let events = model_memento_tool_call_transcript();
        let observed = transcript_contains_explicit_memento_tool_call(&events);
        assert!(
            observed,
            "the model's own mcp__memento__context call must be observable in the turn transcript"
        );

        let mut untouched = live_session();
        assert!(
            take_reflect(&mut untouched).is_none(),
            "a session where the model never called memento must not reflect"
        );

        let mut session = live_session();
        assert!(
            note_memento_context_from_turn(&mut session, false, observed),
            "a channel-owning turn with a model memento call must arm the reflect gate"
        );
        assert!(
            session.memento_context_loaded,
            "arming the gate must set memento_context_loaded"
        );

        let request = take_reflect(&mut session)
            .expect("a session that pulled memento context must produce a reflect request");
        assert_eq!(request.session_id, "provider-session-1");
        assert_eq!(request.reason, SessionEndReason::IdleExpiry);
        assert!(
            request.transcript.contains("remember the deploy order"),
            "the reflect request must carry the session transcript, got: {}",
            request.transcript
        );

        assert!(
            take_reflect(&mut session).is_none(),
            "reflect must stay one-shot per armed session"
        );
    }

    /// A turn whose transcript holds no memento tool call must leave the gate
    /// closed — the gate tracks the model's real calls, not turn count.
    #[test]
    fn turn_without_memento_tool_call_leaves_reflect_disarmed() {
        let events = vec![SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::ToolUse,
            tool_name: Some("Bash".to_string()),
            summary: None,
            content: "{}".to_string(),
            status: None,
            is_error: false,
        }];
        let observed = transcript_contains_explicit_memento_tool_call(&events);
        assert!(!observed, "an unrelated tool must not arm the memento gate");

        let mut session = live_session();
        assert!(!note_memento_context_from_turn(
            &mut session,
            false,
            observed
        ));
        assert!(!session.memento_context_loaded);
        assert!(take_reflect(&mut session).is_none());
    }

    /// #4658 F1: a scheduled-snapshot turn is isolated from the channel, so its
    /// memento calls must not arm the live channel session's reflect gate.
    #[test]
    fn snapshot_turn_memento_tool_call_does_not_arm_channel_reflect() {
        let observed =
            transcript_contains_explicit_memento_tool_call(&model_memento_tool_call_transcript());
        let mut session = live_session();
        assert!(
            !note_memento_context_from_turn(&mut session, true, observed),
            "an isolated snapshot turn must not arm the channel session"
        );
        assert!(!session.memento_context_loaded);
        assert!(take_reflect(&mut session).is_none());
    }

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

/// #5168: arm the session's memento-reflect gate from the MODEL's own memento
/// tool calls (path B), replacing the deleted server-side recall gate (path A).
///
/// `session.memento_context_loaded` gates every reflect request through
/// [`take_memento_reflect_request`] — both the turn-end path
/// (`completion_postlude`) and the idle-expiry path (`idle_detector`). Under
/// path A the flag was set at turn INTAKE, right after AgentDesk itself called
/// memento `recall`/`context` on the model's behalf. Slice 2 (`abb72d261`)
/// deleted those two intake call sites and with them the only production
/// writer of the flag, so it stayed `false` forever and reflect stopped firing
/// everywhere. Reflect is NOT on #5168's removal list — the issue's 24h
/// measurement counts it as a live feature — so the flag is re-defined for
/// path B rather than deleted:
///
///   "memento context entered this session" == "the MODEL called a memento
///   `context`/`recall` tool during a turn of this session".
///
/// The server can observe exactly that without re-introducing a server-side
/// recall: the turn's transcript events carry the MCP tool names, and
/// `recall_feedback::transcript_contains_explicit_memento_tool_call` already
/// classifies them for the tool_feedback analysis performed in the same
/// postlude (`completion_postlude.rs`). The caller passes that observation in
/// so the transcript is scanned once per turn.
///
/// Returns whether this turn armed the gate.
pub(in crate::services::discord) fn note_memento_context_from_turn(
    session: &mut DiscordSession,
    isolated_from_channel: bool,
    memento_tool_call_observed: bool,
) -> bool {
    // #4658 F1 isolation guard: a scheduled-snapshot turn must produce zero
    // channel-scoped side effects, and arming the channel session's reflect
    // gate is one — it would make the live channel reflect over a transcript
    // the snapshot turn produced.
    if isolated_from_channel || !memento_tool_call_observed {
        return false;
    }
    session.note_memento_context_loaded();
    true
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
