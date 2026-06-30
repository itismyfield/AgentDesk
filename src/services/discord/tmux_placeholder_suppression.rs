use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;

use super::super::formatting::{build_streaming_placeholder_text, truncate_str};
use super::super::placeholder_cleanup::{
    PlaceholderCleanupOperation, PlaceholderCleanupOutcome, PlaceholderCleanupRecord,
    classify_delete_error,
};
use super::super::{SharedData, rate_limit_wait};
use crate::services::discord;

const SUPPRESSED_INTERNAL_LABEL: &str = "(자동으로 처리된 내부 작업이라 여기서 멈췄어요)";
const SUPPRESSED_RESTART_LABEL: &str =
    "(서버가 재시작되면서 답변이 중간에 멈췄어요 — 필요하시면 다시 질문해 주세요)";

pub(super) fn watcher_should_render_status_only_placeholder(
    placeholder_exists: bool,
    current_tool_line: Option<&str>,
    task_notification_kind: Option<TaskNotificationKind>,
) -> bool {
    placeholder_exists
        || current_tool_line.is_some_and(|line| !line.trim().is_empty())
        || task_notification_kind.is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SuppressedPlaceholderAction {
    None,
    Delete,
    Edit(String),
}

pub(super) fn rewrite_placeholder_as_terminal_suppressed(
    text: &str,
    label: &str,
    provider: &ProviderKind,
) -> String {
    let cleaned = discord::single_message_panel::strip_placeholder_terminal_status(text, provider);
    let trimmed = cleaned.trim_end();
    if trimmed.ends_with(label) {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        // #1009: label itself may exceed DISCORD_MSG_LIMIT when monitor entries
        // balloon — guard here too (the with-body branch below already guards).
        let limit = discord::DISCORD_MSG_LIMIT;
        if label.len() > limit {
            return truncate_str(label, limit);
        }
        return label.to_string();
    }

    let suffix = format!("\n\n{label}");
    let max_base_len = discord::DISCORD_MSG_LIMIT.saturating_sub(suffix.len());
    let base = if trimmed.len() > max_base_len {
        truncate_str(trimmed, max_base_len)
    } else {
        trimmed.to_string()
    };
    let composed = format!("{base}{suffix}");
    // Final belt-and-suspenders guard (rare: suffix.len() ≥ DISCORD_MSG_LIMIT).
    if composed.len() > discord::DISCORD_MSG_LIMIT {
        truncate_str(&composed, discord::DISCORD_MSG_LIMIT)
    } else {
        composed
    }
}

pub(super) fn reconstructed_inflight_placeholder_body(
    state: &discord::inflight::InflightTurnState,
    provider: &ProviderKind,
) -> String {
    let current_portion = state
        .full_response
        .get(state.response_sent_offset..)
        .unwrap_or("");
    let current_portion =
        discord::formatting::format_for_discord_with_status_panel(current_portion, provider);
    let status_block = discord::formatting::build_placeholder_status_block(
        "⠼",
        state.prev_tool_status.as_deref(),
        state.current_tool_line.as_deref(),
        &state.full_response,
    );
    build_streaming_placeholder_text(&current_portion, &status_block)
}

fn orphan_suppressed_placeholder_action(
    state: &discord::inflight::InflightTurnState,
    provider: &ProviderKind,
    has_active_turn: bool,
    tmux_session_name: &str,
) -> SuppressedPlaceholderAction {
    if has_active_turn
        || state.rebind_origin
        || state.current_msg_id == 0
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
    {
        return SuppressedPlaceholderAction::None;
    }

    let body = reconstructed_inflight_placeholder_body(state, provider);
    let placeholder_was_exposed = state.response_sent_offset > 0
        || !discord::single_message_panel::strip_placeholder_terminal_status(&body, provider)
            .trim()
            .is_empty()
        || discord::single_message_panel::streaming_footer_only_surface_was_exposed(
            &body, provider,
        );
    if !placeholder_was_exposed {
        return SuppressedPlaceholderAction::Delete;
    }
    SuppressedPlaceholderAction::Edit(rewrite_placeholder_as_terminal_suppressed(
        &body,
        SUPPRESSED_RESTART_LABEL,
        provider,
    ))
}

/// Unified entry point for every placeholder-suppression decision.
///
/// Three production sites produced identical edit/delete/log scaffolding before
/// #1055 (`tmux_output_watcher_with_restore` bridge-guard duplicate relay,
/// task-notification terminal suppress, and restored-watcher orphan reconcile).
/// `decide_placeholder_suppression` + `apply_placeholder_suppression` replaces
/// those copies so future regressions fix in one place. See `Shared Agent Rules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderSuppressOrigin {
    OrphanRestartHandoff,
    ActiveBridgeTurnGuard,
    TaskNotificationTerminal,
}

impl PlaceholderSuppressOrigin {
    fn log_scope(self) -> &'static str {
        match self {
            Self::OrphanRestartHandoff => "orphan suppressed placeholder reconcile",
            Self::ActiveBridgeTurnGuard => "active bridge suppressed placeholder",
            Self::TaskNotificationTerminal => "suppressed placeholder",
        }
    }
}

pub(super) struct PlaceholderSuppressContext<'a> {
    pub(super) origin: PlaceholderSuppressOrigin,
    pub(super) provider: &'a ProviderKind,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) response_sent_offset: usize,
    pub(super) last_edit_text: &'a str,
    pub(super) inflight_state: Option<&'a discord::inflight::InflightTurnState>,
    pub(super) has_active_turn: bool,
    pub(super) tmux_session_name: &'a str,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) reattach_offset_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlaceholderSuppressDecision {
    None,
    /// Keep the user-facing body already streamed to the placeholder; strip
    /// the in-progress spinner/status-block suffix so the message is not
    /// frozen mid-stream. `reason` feeds the observability log only.
    /// `cleaned_body` is the stripped body that should be written back.
    Preserve {
        reason: &'static str,
        cleaned_body: String,
    },
    Edit(String),
    Delete,
}

fn strip_placeholder_indicators_for_preserve(text: &str, provider: &ProviderKind) -> String {
    discord::single_message_panel::strip_placeholder_terminal_status(text, provider)
        .trim_end()
        .to_string()
}

pub(super) fn decide_placeholder_suppression(
    ctx: &PlaceholderSuppressContext<'_>,
) -> PlaceholderSuppressDecision {
    match ctx.origin {
        PlaceholderSuppressOrigin::OrphanRestartHandoff => {
            let Some(state) = ctx.inflight_state else {
                return PlaceholderSuppressDecision::None;
            };
            match orphan_suppressed_placeholder_action(
                state,
                ctx.provider,
                ctx.has_active_turn,
                ctx.tmux_session_name,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
        PlaceholderSuppressOrigin::ActiveBridgeTurnGuard => {
            if ctx.reattach_offset_match {
                return PlaceholderSuppressDecision::Preserve {
                    reason: "reattach-offset-match",
                    cleaned_body: strip_placeholder_indicators_for_preserve(
                        ctx.last_edit_text,
                        ctx.provider,
                    ),
                };
            }
            let footer_only_was_exposed =
                discord::single_message_panel::streaming_footer_only_surface_was_exposed(
                    ctx.last_edit_text,
                    ctx.provider,
                );
            match suppressed_placeholder_action(
                ctx.placeholder_msg_id.is_some(),
                ctx.provider,
                ctx.response_sent_offset,
                ctx.last_edit_text,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(content) if footer_only_was_exposed => {
                    PlaceholderSuppressDecision::Edit(content)
                }
                // #3533: this duplicate-relay guard fires because a bridge turn
                // already owns delivery, so an exposed body was/will be delivered by
                // it — preserve it (strip the live spinner) instead of stamping
                // SUPPRESSED_INTERNAL_LABEL (wrong on the restart-straddling turn).
                SuppressedPlaceholderAction::Edit(_) => PlaceholderSuppressDecision::Preserve {
                    reason: "active-bridge-duplicate-delivered",
                    cleaned_body: strip_placeholder_indicators_for_preserve(
                        ctx.last_edit_text,
                        ctx.provider,
                    ),
                },
            }
        }
        PlaceholderSuppressOrigin::TaskNotificationTerminal => {
            // #1708: Background, Subagent, and MonitorAutoTurn task notifications
            // are all auto-fired by tooling, not by the user. If the main turn is
            // streaming user-facing body when one of them terminates, we must
            // preserve that body — otherwise the SUPPRESSED_INTERNAL_LABEL edit
            // overwrites the actual response (Monitor stream-end was the trigger
            // observed in the 2026-05-04 adk-cc incident).
            let preserves_body = matches!(
                ctx.task_notification_kind,
                Some(
                    TaskNotificationKind::Background
                        | TaskNotificationKind::Subagent
                        | TaskNotificationKind::MonitorAutoTurn
                )
            );
            match suppressed_placeholder_action(
                ctx.placeholder_msg_id.is_some(),
                ctx.provider,
                ctx.response_sent_offset,
                ctx.last_edit_text,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(_) if preserves_body => {
                    PlaceholderSuppressDecision::Preserve {
                        reason: "auto-task-notification-kind",
                        cleaned_body: strip_placeholder_indicators_for_preserve(
                            ctx.last_edit_text,
                            ctx.provider,
                        ),
                    }
                }
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
    }
}

pub(super) async fn apply_placeholder_suppression(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    placeholder_msg_id: Option<serenity::MessageId>,
    origin: PlaceholderSuppressOrigin,
    decision: PlaceholderSuppressDecision,
    detail: Option<&str>,
) {
    match decision {
        PlaceholderSuppressDecision::None => {}
        PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let detail_suffix = detail.map(|d| format!(" — {d}")).unwrap_or_default();
            tracing::info!(
                "  [{ts}] 👁 {} preserved placeholder ({reason}){detail_suffix}",
                origin.log_scope()
            );
            if let Some(msg_id) = placeholder_msg_id {
                if cleaned_body.is_empty() {
                    delete_nonterminal_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        origin.log_scope(),
                    )
                    .await;
                } else {
                    edit_preserve_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        &cleaned_body,
                        origin.log_scope(),
                    )
                    .await;
                }
            }
        }
        PlaceholderSuppressDecision::Delete => {
            if let Some(msg_id) = placeholder_msg_id {
                delete_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    origin.log_scope(),
                )
                .await;
            }
        }
        PlaceholderSuppressDecision::Edit(content) => {
            if let Some(msg_id) = placeholder_msg_id {
                edit_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    &content,
                    origin.log_scope(),
                )
                .await;
            }
        }
    }
}

pub(super) fn record_placeholder_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: &str,
    operation: PlaceholderCleanupOperation,
    outcome: PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let PlaceholderCleanupOutcome::Failed { class, detail } = &outcome {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            operation.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    let record = PlaceholderCleanupRecord {
        provider: provider.clone(),
        channel_id,
        message_id,
        tmux_session_name: Some(tmux_session_name.to_string()),
        operation,
        outcome,
        source,
    };
    shared.ui.placeholder_cleanup.record(record);
}

pub(super) async fn delete_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteTerminal,
        source,
    )
    .await
}

pub(super) async fn delete_nonterminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteNonterminal,
        source,
    )
    .await
}

/// #3871: which streamed rollover-prefix message ids the watcher MUST delete
/// after a terminal delivery so the frozen prefixes don't duplicate the body.
///
/// When a `>DISCORD_MSG_LIMIT` answer rolls over mid-stream, the prefix
/// placeholder is FROZEN as a standalone permanent message and a fresh
/// placeholder is opened for the remainder. The terminal full-body fallback
/// (`session_bound_fallback_uses_full_body`) re-posts the WHOLE body as ordered
/// chunks, so every frozen prefix is now a duplicate copy of bytes already in
/// the replay → delete them all (watcher parity with the sink's
/// `terminal_full_replay_cleanup_msg_ids`). On the remainder-only path
/// (`false`) the frozen prefixes carry the legit, already-delivered
/// `[0..response_sent_offset]` prose and MUST be preserved — return nothing.
pub(super) fn watcher_rollover_prefixes_to_delete_on_terminal(
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: &[MessageId],
) -> Vec<MessageId> {
    if session_bound_fallback_uses_full_body {
        frozen_rollover_msg_ids.to_vec()
    } else {
        Vec::new()
    }
}

/// #3871: delete the streamed rollover-prefix messages the watcher froze during
/// streaming, after a terminal full-body replay re-posted their bytes. Mirrors
/// the sink's drain-and-delete of `terminal_full_replay_cleanup_msg_ids`; each
/// id is a non-terminal streamed prefix so `DeleteNonterminal` is used. No-op on
/// the remainder-only path (see [`watcher_rollover_prefixes_to_delete_on_terminal`]).
pub(super) async fn delete_watcher_rollover_frozen_prefixes(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: Vec<MessageId>,
) {
    for frozen_prefix in watcher_rollover_prefixes_to_delete_on_terminal(
        session_bound_fallback_uses_full_body,
        &frozen_rollover_msg_ids,
    ) {
        rate_limit_wait(shared, channel_id).await;
        let _ = delete_nonterminal_placeholder(
            http,
            channel_id,
            shared,
            provider,
            tmux_session_name,
            frozen_prefix,
            "watcher_terminal_rollover_prefix_dedup_3871",
        )
        .await;
    }
}

async fn delete_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    let outcome = match channel_id.delete_message(http, message_id).await {
        Ok(_) => PlaceholderCleanupOutcome::Succeeded,
        Err(error) => classify_delete_error(&error.to_string()),
    };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

async fn edit_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditTerminal,
        source,
    )
    .await
}

async fn edit_preserve_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditPreserve,
        source,
    )
    .await
}

async fn edit_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    rate_limit_wait(shared, channel_id).await;
    let outcome =
        match discord::http::edit_channel_message(http, channel_id, message_id, content).await {
            Ok(_) => PlaceholderCleanupOutcome::Succeeded,
            Err(error) => PlaceholderCleanupOutcome::failed(error.to_string()),
        };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FallbackPlaceholderCleanupDecision {
    RelayCommitted,
    PreserveInflightForCleanupRetry,
}

pub(super) fn fallback_placeholder_cleanup_decision(
    cleanup: &PlaceholderCleanupOutcome,
) -> FallbackPlaceholderCleanupDecision {
    if cleanup.is_committed() {
        FallbackPlaceholderCleanupDecision::RelayCommitted
    } else {
        FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry
    }
}

fn suppressed_placeholder_action(
    has_placeholder: bool,
    provider: &ProviderKind,
    response_sent_offset: usize,
    last_edit_text: &str,
) -> SuppressedPlaceholderAction {
    if !has_placeholder {
        return SuppressedPlaceholderAction::None;
    }

    let placeholder_was_exposed = response_sent_offset > 0
        || !discord::single_message_panel::strip_placeholder_terminal_status(
            last_edit_text,
            provider,
        )
        .trim()
        .is_empty()
        || discord::single_message_panel::streaming_footer_only_surface_was_exposed(
            last_edit_text,
            provider,
        );
    if placeholder_was_exposed {
        SuppressedPlaceholderAction::Edit(rewrite_placeholder_as_terminal_suppressed(
            last_edit_text,
            SUPPRESSED_INTERNAL_LABEL,
            provider,
        ))
    } else {
        SuppressedPlaceholderAction::Delete
    }
}

#[cfg(test)]
mod placeholder_suppression_tests {
    use super::*;

    const TEST_SESSION: &str = "adk-claude-test";

    fn footer_status_block() -> String {
        let long_tail = "└ cargo test --lib tmux ".repeat(120);
        let panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n└ cargo test --lib tmux\nSubagents\n└ review inspect\n{long_tail}"
        );
        let status_block = discord::single_message_panel::compose_footer_status_block("⠋", &panel);
        assert!(status_block.starts_with("⠋ 진행 중 — Claude (<t:1700000000:R>)"));
        assert!(status_block.contains("Tools"));
        assert!(status_block.contains("Subagents"));
        assert!(status_block.contains('…'));
        status_block
    }

    fn footer_only_placeholder() -> String {
        build_streaming_placeholder_text("", &footer_status_block())
    }

    fn body_with_footer(body: &str) -> String {
        build_streaming_placeholder_text(body, &footer_status_block())
    }

    fn completion_footer_block() -> String {
        "Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%\n\nSubagents\n└ bgworker Long background job ✓".to_string()
    }

    fn completion_footer_only_placeholder() -> String {
        completion_footer_block()
    }

    fn body_with_completion_footer(body: &str) -> String {
        format!("{body}\n\n{}", completion_footer_block())
    }

    struct RuntimeRootEnvRestore {
        previous: Option<std::ffi::OsString>,
    }

    impl Drop for RuntimeRootEnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn set_runtime_root_for_test(path: &std::path::Path) -> RuntimeRootEnvRestore {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        RuntimeRootEnvRestore { previous }
    }

    fn scoped_runtime_root_for_test() -> (
        std::sync::MutexGuard<'static, ()>,
        tempfile::TempDir,
        RuntimeRootEnvRestore,
    ) {
        let lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let env = set_runtime_root_for_test(tempdir.path());
        (lock, tempdir, env)
    }

    fn orphan_state(
        full_response: &str,
        response_sent_offset: usize,
    ) -> discord::inflight::InflightTurnState {
        let mut state = discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("test".to_string()),
            1,
            2,
            3,
            "user".to_string(),
            Some("session".to_string()),
            Some(TEST_SESSION.to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.full_response = full_response.to_string();
        state.response_sent_offset = response_sent_offset;
        state
    }

    #[test]
    fn status_only_legacy_spinner_placeholder_deletes() {
        assert_eq!(
            suppressed_placeholder_action(true, &ProviderKind::Claude, 0, "⠋ 계속 처리 중"),
            SuppressedPlaceholderAction::Delete
        );
    }

    #[test]
    fn footer_mode_status_only_placeholder_edits_to_keep_message_target() {
        let placeholder = footer_only_placeholder();

        let action = suppressed_placeholder_action(true, &ProviderKind::Claude, 0, &placeholder);
        let SuppressedPlaceholderAction::Edit(content) = action else {
            panic!("footer-only placeholder should edit instead of deleting its message target");
        };
        assert_eq!(content, SUPPRESSED_INTERNAL_LABEL);
    }

    #[test]
    fn completion_footer_only_placeholder_edits_to_keep_message_target() {
        let placeholder = completion_footer_only_placeholder();

        let action = suppressed_placeholder_action(true, &ProviderKind::Claude, 0, &placeholder);
        let SuppressedPlaceholderAction::Edit(content) = action else {
            panic!("completion-footer-only placeholder should edit instead of deleting");
        };
        assert_eq!(content, SUPPRESSED_INTERNAL_LABEL);
    }

    #[test]
    fn real_body_with_footer_edits_label_and_strips_footer() {
        let placeholder = body_with_footer("visible assistant body");
        let action = suppressed_placeholder_action(true, &ProviderKind::Claude, 0, &placeholder);

        let SuppressedPlaceholderAction::Edit(content) = action else {
            panic!("real body plus footer should edit the terminal label");
        };
        assert_eq!(
            content,
            format!("visible assistant body\n\n{SUPPRESSED_INTERNAL_LABEL}")
        );
        assert!(!content.contains("진행 중 — Claude"));
        assert!(!content.contains("Tools"));
        assert!(!content.contains("Subagents"));
    }

    #[test]
    fn real_body_with_completion_footer_edits_label_and_strips_footer() {
        let placeholder = body_with_completion_footer("visible assistant body");
        let action = suppressed_placeholder_action(true, &ProviderKind::Claude, 0, &placeholder);

        let SuppressedPlaceholderAction::Edit(content) = action else {
            panic!("real body plus completion footer should edit the terminal label");
        };
        assert_eq!(
            content,
            format!("visible assistant body\n\n{SUPPRESSED_INTERNAL_LABEL}")
        );
        assert!(!content.contains("Context   📦"));
        assert!(!content.contains("Subagents"));
    }

    #[test]
    fn response_sent_offset_counts_as_exposure_even_with_empty_text() {
        assert_eq!(
            suppressed_placeholder_action(true, &ProviderKind::Claude, 1, ""),
            SuppressedPlaceholderAction::Edit(SUPPRESSED_INTERNAL_LABEL.to_string())
        );
    }

    #[test]
    fn orphan_restart_status_only_placeholder_deletes() {
        let (_lock, _tempdir, _env) = scoped_runtime_root_for_test();
        let state = orphan_state("", 0);

        assert_eq!(
            orphan_suppressed_placeholder_action(
                &state,
                &ProviderKind::Claude,
                false,
                TEST_SESSION
            ),
            SuppressedPlaceholderAction::Delete
        );
    }

    #[test]
    fn orphan_restart_real_body_edits_restart_label() {
        let (_lock, _tempdir, _env) = scoped_runtime_root_for_test();
        let state = orphan_state("visible assistant body", 0);
        let action = orphan_suppressed_placeholder_action(
            &state,
            &ProviderKind::Claude,
            false,
            TEST_SESSION,
        );

        let SuppressedPlaceholderAction::Edit(content) = action else {
            panic!("orphan restart body should edit the restart label");
        };
        assert_eq!(
            content,
            format!("visible assistant body\n\n{SUPPRESSED_RESTART_LABEL}")
        );
    }

    #[test]
    fn orphan_restart_subagent_notification_body_is_sanitized_3818() {
        let (_lock, _tempdir, _env) = scoped_runtime_root_for_test();
        let full_response = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
No response requested.\n\
<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";
        let mut state = orphan_state(full_response, 0);
        state.provider = ProviderKind::Codex.as_str().to_string();

        let action =
            orphan_suppressed_placeholder_action(&state, &ProviderKind::Codex, false, TEST_SESSION);

        let SuppressedPlaceholderAction::Edit(content) = action else {
            panic!("orphan restart subagent body should edit the restart label");
        };
        assert!(content.contains("Subagent completed"));
        assert!(content.contains("Review complete."));
        assert!(content.ends_with(SUPPRESSED_RESTART_LABEL));
        assert!(!content.contains("[Provider Session Reuse]"));
        assert!(!content.contains("No response requested."));
        assert!(!content.contains("<subagent_notification>"));
        assert!(!content.contains("agent_path"));
        assert!(!content.contains("/tmp/private-agent"));
    }

    #[test]
    fn orphan_restart_offset_counts_as_exposure_even_with_empty_body() {
        let (_lock, _tempdir, _env) = scoped_runtime_root_for_test();
        let state = orphan_state("", 1);

        assert_eq!(
            orphan_suppressed_placeholder_action(
                &state,
                &ProviderKind::Claude,
                false,
                TEST_SESSION
            ),
            SuppressedPlaceholderAction::Edit(SUPPRESSED_RESTART_LABEL.to_string())
        );
    }

    fn active_bridge_ctx<'a>(
        placeholder: &'a str,
        response_sent_offset: usize,
        reattach_offset_match: bool,
    ) -> PlaceholderSuppressContext<'a> {
        PlaceholderSuppressContext {
            origin: PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            provider: &ProviderKind::Claude,
            placeholder_msg_id: Some(MessageId::new(1)),
            response_sent_offset,
            last_edit_text: placeholder,
            inflight_state: None,
            has_active_turn: false,
            tmux_session_name: TEST_SESSION,
            task_notification_kind: None,
            reattach_offset_match,
        }
    }

    #[test]
    fn active_bridge_exposed_body_preserves_instead_of_internal_label() {
        // #3533: a duplicate-relay guard on an already-exposed body (e.g. the
        // in-flight turn that straddled a dcserver restart, reattach NOT matched)
        // must PRESERVE the delivered body, never stamp SUPPRESSED_INTERNAL_LABEL.
        let placeholder = body_with_footer("visible assistant body");
        let ctx = active_bridge_ctx(&placeholder, 0, false);

        let PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } = decide_placeholder_suppression(&ctx)
        else {
            panic!("active bridge duplicate of an exposed body should preserve, not label");
        };
        assert_eq!(reason, "active-bridge-duplicate-delivered");
        assert_eq!(cleaned_body, "visible assistant body");
        assert!(!cleaned_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!cleaned_body.contains("진행 중 — Claude"));
    }

    #[test]
    fn active_bridge_offset_exposed_empty_text_preserves() {
        // response_sent_offset > 0 (already-delivered bytes) with no residual text
        // is the bare restart-boundary signal — still preserve, never label.
        let ctx = active_bridge_ctx("", 1, false);
        assert!(matches!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Preserve { .. }
        ));
    }

    #[test]
    fn active_bridge_footer_only_placeholder_edits_to_preserve_target() {
        // No body and no sent offset still carries a visible single-message
        // footer. Keep the Discord message id alive so the bridge-owned
        // completion footer can edit the same target instead of hitting 404.
        let placeholder = footer_only_placeholder();
        let ctx = active_bridge_ctx(&placeholder, 0, false);
        let PlaceholderSuppressDecision::Edit(content) = decide_placeholder_suppression(&ctx)
        else {
            panic!("footer-only active bridge placeholder should edit, not delete");
        };
        assert_eq!(content, SUPPRESSED_INTERNAL_LABEL);
    }

    // #3871: a >DISCORD_MSG_LIMIT answer rolls over mid-stream (the prefix is
    // FROZEN as a standalone message, a fresh placeholder opens for the
    // remainder), then the terminal full-body fallback re-posts the WHOLE body
    // as ordered chunks. Pin the dup-relay closure: the frozen prefix bytes ARE
    // re-sent in the replay, so unless they are deleted the user sees the prose
    // twice. Assert (a) the frozen prefix is scheduled for deletion, (b) the
    // full body is chunked exactly once (the single delivery), and (c) the
    // remainder-only path PRESERVES the prefix (no spurious delete / data loss).
    #[test]
    fn rollover_frozen_prefix_is_deleted_on_full_body_fallback_no_dup() {
        use crate::services::discord::formatting::{plan_streaming_rollover, split_message};

        // A genuinely-long answer that exceeds the 2000-byte limit and forces at
        // least one streaming rollover (distinct paragraphs give the splitter a
        // clean boundary).
        let full_body = (0..40)
            .map(|i| format!("Paragraph {i}: {}", "lorem ipsum dolor sit amet ".repeat(3)))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(
            full_body.len() > discord::DISCORD_MSG_LIMIT,
            "fixture must exceed DISCORD_MSG_LIMIT to trigger rollover"
        );

        // Simulate the watcher streaming rollover loop: each rollover freezes the
        // current placeholder (a synthetic message id) and advances the offset.
        let status_block = footer_status_block();
        let mut response_sent_offset = 0usize;
        let mut frozen_msg_ids: Vec<MessageId> = Vec::new();
        let mut next_msg_id = 1u64;
        while let Some(plan) =
            plan_streaming_rollover(&full_body[response_sent_offset..], &status_block)
        {
            // The placeholder holding `current_portion[..split_at]` is frozen and
            // becomes permanent; a new placeholder (next id) takes the remainder.
            frozen_msg_ids.push(MessageId::new(next_msg_id));
            next_msg_id += 1;
            response_sent_offset += plan.split_at;
        }
        assert!(
            !frozen_msg_ids.is_empty(),
            "the >2000-byte answer must have frozen at least one rollover prefix"
        );

        // (a) Terminal FULL-BODY fallback: the re-posted body is split once into
        // ordered chunks (the single delivery)...
        let replay_chunks = split_message(&full_body);
        // ...and EVERY frozen prefix is scheduled for deletion (no dup left).
        let to_delete = watcher_rollover_prefixes_to_delete_on_terminal(true, &frozen_msg_ids);
        assert_eq!(
            to_delete, frozen_msg_ids,
            "full-body fallback must delete all frozen rollover prefixes so the prose is not duplicated"
        );

        // The dup-closure: the frozen prefix bytes are genuinely re-sent in the
        // replay, so deletion is necessary — the joined replay chunks contain the
        // leading prefix content. (No full-body re-send is left UNDELETED.)
        let replay_joined = replay_chunks.concat();
        assert!(
            replay_joined.contains(&full_body[..200.min(full_body.len())]),
            "the full-body replay re-sends the leading prefix bytes (the duplicate source)"
        );

        // (c) Remainder-only path: the frozen prefixes carry the already-delivered
        // `[0..offset]` prose and MUST be preserved (deleting them would lose
        // content). The cleanup set is empty.
        let preserved = watcher_rollover_prefixes_to_delete_on_terminal(false, &frozen_msg_ids);
        assert!(
            preserved.is_empty(),
            "remainder-only delivery must preserve frozen prefixes (no spurious delete / no data loss)"
        );
    }
}
