use poise::serenity_prelude as serenity;
use serenity::MessageId;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;

use super::super::formatting::{build_streaming_placeholder_text, truncate_str};
use crate::services::discord;

#[cfg(test)]
use super::super::outbound::delivery_frontier_probe;
#[cfg(test)]
use super::super::placeholder_cleanup::committed_terminal_anchor_protects_delete;
#[cfg(test)]
use serenity::ChannelId;

mod evidence;
mod ops;

pub(super) use self::evidence::{
    GuardedDeliveredElsewhereSignal, GuardedNonterminalDeleteDecision,
    guarded_cleanup_delivered_elsewhere_signal, guarded_nonterminal_delete_decision,
    placeholder_real_body_exposure_evidence,
};
pub(super) use self::ops::{
    FallbackPlaceholderCleanupDecision, apply_placeholder_suppression,
    delete_nonterminal_placeholder, delete_nonterminal_placeholder_unless_delivered,
    delete_terminal_placeholder, delete_terminal_placeholder_unless_delivered,
    delete_watcher_rollover_frozen_prefixes, fallback_placeholder_cleanup_decision,
    record_placeholder_cleanup, watcher_rollover_prefixes_to_delete_on_terminal,
};

#[cfg(test)]
use self::evidence::{
    apply_terminal_committed_delete_proof_gate,
    guarded_cleanup_delivered_elsewhere_signal_from_anchor,
};
use self::ops::strip_placeholder_indicators_for_preserve;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GuardedCleanupTargetAuthor {
    Watcher,
    CrossActor,
    Unknown,
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
                SuppressedPlaceholderAction::Edit(_) if footer_only_was_exposed => {
                    // The active bridge route may still edit `inflight.current_msg_id`,
                    // and the restored watcher seed can point at that same id. Leave
                    // footer-only chrome untouched for the bridge overwrite.
                    PlaceholderSuppressDecision::None
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
            // #1708/#4144: production suppression currently reaches this origin
            // only with Some(kind); the None arm is a defensive guard, not a known
            // watcher-restart path. Preserve exposed body unless a future
            // task-notification kind explicitly opts into destructive stamping.
            let (preserves_body, preserve_reason) = match ctx.task_notification_kind {
                None => (true, "unclassified-task-notification-kind"),
                Some(
                    TaskNotificationKind::Background
                    | TaskNotificationKind::Subagent
                    | TaskNotificationKind::MonitorAutoTurn,
                ) => (true, "auto-task-notification-kind"),
            };
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
                        reason: preserve_reason,
                        cleaned_body: strip_placeholder_indicators_for_preserve(
                            ctx.last_edit_text,
                            ctx.provider,
                        ),
                    }
                }
                SuppressedPlaceholderAction::Edit(content) => {
                    // Defensive only: every current task-notification kind plus
                    // unclassified None preserves exposed bodies. Reaching this
                    // requires a future kind to explicitly opt out above.
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
    }
}
pub(super) fn guarded_cleanup_target_author(
    live_inflight: Option<&discord::inflight::InflightTurnState>,
    message_id: MessageId,
) -> GuardedCleanupTargetAuthor {
    let Some(inflight) = live_inflight else {
        return GuardedCleanupTargetAuthor::Unknown;
    };
    if inflight.current_msg_id != message_id.get() {
        return GuardedCleanupTargetAuthor::Unknown;
    }
    match inflight.effective_relay_owner_kind() {
        discord::inflight::RelayOwnerKind::Watcher => GuardedCleanupTargetAuthor::Watcher,
        discord::inflight::RelayOwnerKind::None
        | discord::inflight::RelayOwnerKind::StandbyRelay
        | discord::inflight::RelayOwnerKind::SessionBoundRelay
        | discord::inflight::RelayOwnerKind::Unknown => GuardedCleanupTargetAuthor::CrossActor,
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

    let real_body_was_exposed =
        placeholder_real_body_exposure_evidence(provider, response_sent_offset, last_edit_text)
            .is_some();
    let placeholder_was_exposed = real_body_was_exposed
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
            "🟢 진행 중\n턴 트리거: https://discord.com/channels/1/2/3\n턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)\n마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>)\n\nTools\n└ cargo test --lib tmux\nSubagents\n└ review inspect\n{long_tail}"
        );
        let status_block = discord::single_message_panel::compose_footer_status_block("⠋", &panel);
        assert!(status_block.starts_with("⠋ 진행 중"));
        assert!(status_block.contains("마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>)"));
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

    fn delivered_anchor(
        channel_id: ChannelId,
        message_id: MessageId,
        range: (u64, u64),
    ) -> delivery_frontier_probe::CurrentGenerationAnchor {
        delivery_frontier_probe::CurrentGenerationAnchor {
            panel_msg_id: message_id.get(),
            panel_channel_id: channel_id.get(),
            range,
        }
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
    fn response_sent_offset_alone_does_not_expose_placeholder_body() {
        assert_eq!(
            suppressed_placeholder_action(true, &ProviderKind::Claude, 1, ""),
            SuppressedPlaceholderAction::Delete
        );
    }

    #[test]
    fn no_response_cleanup_requires_real_body_in_target_message() {
        let placeholder = body_with_footer("visible assistant body");

        assert_eq!(
            placeholder_real_body_exposure_evidence(&ProviderKind::Claude, 0, &placeholder),
            Some("last_edit_text_body")
        );
        assert_eq!(
            placeholder_real_body_exposure_evidence(&ProviderKind::Claude, 1, ""),
            None
        );
    }

    #[test]
    fn no_response_cleanup_deletes_footer_only_chrome() {
        let placeholder = footer_only_placeholder();

        assert_eq!(
            placeholder_real_body_exposure_evidence(&ProviderKind::Claude, 0, &placeholder),
            None
        );
        assert!(
            discord::single_message_panel::streaming_footer_only_surface_was_exposed(
                &placeholder,
                &ProviderKind::Claude
            )
        );
    }

    #[test]
    fn same_message_frontier_is_hard_preserve_for_sink_delivered_body() {
        let channel_id = ChannelId::new(42);
        let msg_id = MessageId::new(88);
        let signal = guarded_cleanup_delivered_elsewhere_signal_from_anchor(
            channel_id,
            msg_id,
            (100, 200),
            delivered_anchor(channel_id, msg_id, (100, 200)),
            Some(200),
            Some(200),
        );

        assert_eq!(
            signal,
            GuardedDeliveredElsewhereSignal::Protected {
                evidence: "current_generation_anchor_same_message"
            }
        );
        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &footer_only_placeholder(),
                false,
                signal,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "current_generation_anchor_same_message"
            }
        );
    }

    #[test]
    fn same_wrapper_truncation_downgrades_stale_different_message_frontier() {
        let channel_id = ChannelId::new(42);
        let candidate_msg = MessageId::new(88);
        let delivered_msg = MessageId::new(99);
        let signal = guarded_cleanup_delivered_elsewhere_signal_from_anchor(
            channel_id,
            candidate_msg,
            (1_200, 1_300),
            delivered_anchor(channel_id, delivered_msg, (1_000, 2_000)),
            Some(2_500),
            Some(900),
        );

        assert_eq!(
            signal,
            GuardedDeliveredElsewhereSignal::Ambiguous {
                evidence: "current_generation_anchor_live_frontier_mismatch"
            }
        );
        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &footer_only_placeholder(),
                false,
                signal,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "current_generation_anchor_live_frontier_mismatch"
            }
        );
    }

    #[test]
    fn stale_frontier_beyond_current_eof_is_ambiguous() {
        let channel_id = ChannelId::new(42);
        let candidate_msg = MessageId::new(88);
        let delivered_msg = MessageId::new(99);

        assert_eq!(
            guarded_cleanup_delivered_elsewhere_signal_from_anchor(
                channel_id,
                candidate_msg,
                (200, 300),
                delivered_anchor(channel_id, delivered_msg, (100, 1_000)),
                Some(350),
                Some(350),
            ),
            GuardedDeliveredElsewhereSignal::Ambiguous {
                evidence: "current_generation_anchor_exceeds_current_eof"
            }
        );
    }

    #[test]
    fn cross_actor_body_preserves_no_edit_instead_of_stripping_snapshot() {
        let mut inflight = orphan_state("", 0);
        let msg_id = MessageId::new(88);
        inflight.current_msg_id = msg_id.get();
        inflight.set_relay_owner_kind(discord::inflight::RelayOwnerKind::SessionBoundRelay);
        let placeholder = body_with_footer("complete sink-authored answer");

        let author = guarded_cleanup_target_author(Some(&inflight), msg_id);
        assert_eq!(author, GuardedCleanupTargetAuthor::CrossActor);
        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::Ambiguous {
                    evidence: "current_generation_anchor_range_mismatch"
                },
                author,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "last_edit_text_body"
            }
        );
    }

    #[test]
    fn unknown_author_body_preserves_no_edit() {
        let placeholder = body_with_footer("possibly cross-actor body");

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "last_edit_text_body"
            }
        );
    }

    #[test]
    fn different_message_same_coordinate_frontier_still_deletes_duplicate() {
        let channel_id = ChannelId::new(42);
        let candidate_msg = MessageId::new(88);
        let delivered_msg = MessageId::new(99);
        let signal = guarded_cleanup_delivered_elsewhere_signal_from_anchor(
            channel_id,
            candidate_msg,
            (150, 250),
            delivered_anchor(channel_id, delivered_msg, (100, 300)),
            Some(300),
            Some(300),
        );

        assert_eq!(
            signal,
            GuardedDeliveredElsewhereSignal::Found {
                evidence: "current_generation_anchor_different_message"
            }
        );
        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &footer_only_placeholder(),
                false,
                signal,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::Delete {
                evidence: "current_generation_anchor_different_message"
            }
        );
    }

    #[test]
    fn late_epoch_guard_deletes_rollover_tail_when_offset_only() {
        let placeholder = footer_only_placeholder();

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                128,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::Delete {
                evidence: "no_delivered_elsewhere_signal"
            }
        );
    }

    #[test]
    fn late_epoch_guard_preserves_ambiguous_body_with_strip() {
        let placeholder = body_with_footer("visible assistant body");

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                128,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::Ambiguous {
                    evidence: "current_generation_anchor_range_mismatch"
                },
                GuardedCleanupTargetAuthor::Watcher,
            ),
            GuardedNonterminalDeleteDecision::PreserveWithStrip {
                evidence: "last_edit_text_body",
                cleaned_body: "visible assistant body".to_string()
            }
        );
    }

    #[test]
    fn late_epoch_guard_preserves_committed_terminal_anchor_evidence() {
        let placeholder = footer_only_placeholder();

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &placeholder,
                true,
                GuardedDeliveredElsewhereSignal::Found {
                    evidence: "current_generation_anchor_different_message"
                },
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "committed_terminal_anchor"
            }
        );
    }

    #[test]
    fn late_epoch_guard_deletes_placeholder_without_evidence() {
        let placeholder = footer_only_placeholder();

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::Delete {
                evidence: "no_delivered_elsewhere_signal"
            }
        );
    }

    #[test]
    fn duplicate_copy_with_delivered_elsewhere_deletes_body_placeholder() {
        let placeholder = body_with_footer("visible assistant body");

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                128,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::Found {
                    evidence: "current_generation_anchor_different_message"
                },
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::Delete {
                evidence: "current_generation_anchor_different_message"
            }
        );
    }

    #[test]
    fn duplicate_relay_guard_deletes_rollover_tail_when_offset_only() {
        let placeholder = footer_only_placeholder();

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                128,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::Delete {
                evidence: "no_delivered_elsewhere_signal"
            }
        );
    }

    #[test]
    fn duplicate_relay_guard_deletes_placeholder_without_evidence() {
        let placeholder = footer_only_placeholder();

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::Delete {
                evidence: "no_delivered_elsewhere_signal"
            }
        );
    }

    #[test]
    fn sole_copy_ambiguous_body_preserves_with_strip() {
        let placeholder = body_with_footer("possibly sole assistant body");

        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                &placeholder,
                false,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Watcher,
            ),
            GuardedNonterminalDeleteDecision::PreserveWithStrip {
                evidence: "last_edit_text_body",
                cleaned_body: "possibly sole assistant body".to_string()
            }
        );
    }

    // #4158 hardening: the terminal-committed arm (SkipAlreadyCommitted) must
    // NOT delete on the disposable-chrome default. When the durable delivered
    // anchor is absent (shadow-write disabled → `NotFound`) the base table would
    // `Delete` a body-less placeholder, but on the terminal-committed arm that
    // placeholder might be the sink's `PlaceholderEdit` delivery target, so the
    // proof gate downgrades every non-`Found` delete to a fail-safe preserve.
    #[test]
    fn proof_gate_downgrades_notfound_default_delete_to_preserve() {
        let delete = GuardedNonterminalDeleteDecision::Delete {
            evidence: "no_delivered_elsewhere_signal",
        };
        assert_eq!(
            apply_terminal_committed_delete_proof_gate(delete, false, true),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "terminal_committed_requires_delivered_elsewhere_proof",
            },
            "terminal-committed arm must preserve when there is no positive Found proof"
        );
    }

    #[test]
    fn proof_gate_keeps_found_delete_and_leaves_other_arms_untouched() {
        // Positive `Found` proof (a DIFFERENT message holds the range) still
        // deletes — the real #4158 residue path (shadow anchor enabled).
        let found_delete = GuardedNonterminalDeleteDecision::Delete {
            evidence: "current_generation_anchor_different_message",
        };
        assert_eq!(
            apply_terminal_committed_delete_proof_gate(found_delete.clone(), true, true),
            found_delete,
            "a Found-backed delete must survive the proof gate"
        );

        // The no-response arm (`require = false`) keeps the base decision table:
        // a disposable-chrome default delete is NOT downgraded.
        let default_delete = GuardedNonterminalDeleteDecision::Delete {
            evidence: "no_delivered_elsewhere_signal",
        };
        assert_eq!(
            apply_terminal_committed_delete_proof_gate(default_delete.clone(), false, false),
            default_delete,
            "with require=false the base table is unchanged (no-response arm parity)"
        );

        // Preserve decisions are never altered by the gate.
        let preserve = GuardedNonterminalDeleteDecision::PreserveNoEdit {
            evidence: "committed_terminal_anchor",
        };
        assert_eq!(
            apply_terminal_committed_delete_proof_gate(preserve.clone(), false, true),
            preserve,
            "a preserve decision is untouched by the proof gate"
        );
    }

    #[test]
    fn live_inflight_anchor_preserves_no_edit_without_registry() {
        let registry =
            crate::services::discord::placeholder_cleanup::PlaceholderCleanupRegistry::default();
        let mut inflight = orphan_state("", 0);
        let msg_id = MessageId::new(77);
        let channel_id = ChannelId::new(inflight.channel_id);
        inflight.current_msg_id = msg_id.get();
        inflight.terminal_delivery_committed = true;

        let anchor_pass = committed_terminal_anchor_protects_delete(
            &registry,
            &ProviderKind::Claude,
            channel_id,
            msg_id,
            Some(&inflight),
        );

        assert!(anchor_pass);
        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                "",
                anchor_pass,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "committed_terminal_anchor"
            }
        );
    }

    #[test]
    fn no_response_gate_preserves_bridge_delivered_current_msg_without_body() {
        let registry =
            crate::services::discord::placeholder_cleanup::PlaceholderCleanupRegistry::default();
        let mut inflight = orphan_state("", 0);
        let msg_id = MessageId::new(88);
        let channel_id = ChannelId::new(inflight.channel_id);
        inflight.current_msg_id = msg_id.get();
        inflight.terminal_delivery_committed = true;

        let anchor_pass = committed_terminal_anchor_protects_delete(
            &registry,
            &ProviderKind::Claude,
            channel_id,
            msg_id,
            Some(&inflight),
        );

        assert!(anchor_pass);
        assert_eq!(
            guarded_nonterminal_delete_decision(
                &ProviderKind::Claude,
                0,
                "",
                anchor_pass,
                GuardedDeliveredElsewhereSignal::NotFound,
                GuardedCleanupTargetAuthor::Unknown,
            ),
            GuardedNonterminalDeleteDecision::PreserveNoEdit {
                evidence: "committed_terminal_anchor"
            }
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

    fn task_notification_terminal_ctx<'a>(
        placeholder: &'a str,
        response_sent_offset: usize,
        task_notification_kind: Option<TaskNotificationKind>,
    ) -> PlaceholderSuppressContext<'a> {
        PlaceholderSuppressContext {
            origin: PlaceholderSuppressOrigin::TaskNotificationTerminal,
            provider: &ProviderKind::Claude,
            placeholder_msg_id: Some(MessageId::new(1)),
            response_sent_offset,
            last_edit_text: placeholder,
            inflight_state: None,
            has_active_turn: false,
            tmux_session_name: TEST_SESSION,
            task_notification_kind,
            reattach_offset_match: false,
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
    fn active_bridge_offset_only_empty_text_deletes() {
        // Offset alone is not proof that the target message contains body; after
        // rollover it may describe prose frozen into an earlier message.
        let ctx = active_bridge_ctx("", 1, false);
        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
    }

    #[test]
    fn active_bridge_footer_only_placeholder_preserves_bridge_target_without_label() {
        // No body and no sent offset still carries a visible single-message
        // footer. Keep the Discord message id untouched so the bridge-owned
        // completion footer can edit the same target instead of hitting 404.
        let placeholder = footer_only_placeholder();
        let ctx = active_bridge_ctx(&placeholder, 0, false);

        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::None
        );
    }

    #[test]
    fn task_notification_none_kind_exposed_body_preserves() {
        // #4144 defensive guard: terminal suppression is currently gated by
        // task_notification_kind.is_some(), so None should not be reached in
        // production. If a future caller reaches it, still preserve exposed body.
        let placeholder = body_with_footer("visible assistant body");
        let ctx = task_notification_terminal_ctx(&placeholder, 0, None);

        let PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } = decide_placeholder_suppression(&ctx)
        else {
            panic!("unclassified task notification with exposed body should preserve");
        };
        assert_eq!(reason, "unclassified-task-notification-kind");
        assert_eq!(cleaned_body, "visible assistant body");
        assert!(!cleaned_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!cleaned_body.contains("진행 중 — Claude"));
    }

    #[test]
    fn task_notification_none_kind_unexposed_placeholder_deletes() {
        let ctx = task_notification_terminal_ctx("⠋ 계속 처리 중", 0, None);

        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
    }

    #[test]
    fn task_notification_subagent_kind_exposed_body_preserves() {
        let placeholder = body_with_footer("visible assistant body");
        let ctx =
            task_notification_terminal_ctx(&placeholder, 0, Some(TaskNotificationKind::Subagent));

        let PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } = decide_placeholder_suppression(&ctx)
        else {
            panic!("subagent task notification with exposed body should preserve");
        };
        assert_eq!(reason, "auto-task-notification-kind");
        assert_eq!(cleaned_body, "visible assistant body");
        assert!(!cleaned_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!cleaned_body.contains("진행 중 — Claude"));
    }

    #[test]
    fn task_notification_subagent_kind_unexposed_placeholder_deletes() {
        let ctx = task_notification_terminal_ctx(
            "⠋ 계속 처리 중",
            0,
            Some(TaskNotificationKind::Subagent),
        );

        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
    }

    #[test]
    fn task_notification_background_kind_exposed_body_preserves() {
        let placeholder = body_with_footer("visible assistant body");
        let ctx =
            task_notification_terminal_ctx(&placeholder, 0, Some(TaskNotificationKind::Background));

        let PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } = decide_placeholder_suppression(&ctx)
        else {
            panic!("background task notification with exposed body should preserve");
        };
        assert_eq!(reason, "auto-task-notification-kind");
        assert_eq!(cleaned_body, "visible assistant body");
        assert!(!cleaned_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!cleaned_body.contains("진행 중 — Claude"));
    }

    #[test]
    fn task_notification_background_kind_unexposed_placeholder_deletes() {
        let ctx = task_notification_terminal_ctx(
            "⠋ 계속 처리 중",
            0,
            Some(TaskNotificationKind::Background),
        );

        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
    }

    #[test]
    fn task_notification_monitor_auto_turn_kind_exposed_body_preserves() {
        let placeholder = body_with_footer("visible assistant body");
        let ctx = task_notification_terminal_ctx(
            &placeholder,
            0,
            Some(TaskNotificationKind::MonitorAutoTurn),
        );

        let PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } = decide_placeholder_suppression(&ctx)
        else {
            panic!("monitor auto-turn task notification with exposed body should preserve");
        };
        assert_eq!(reason, "auto-task-notification-kind");
        assert_eq!(cleaned_body, "visible assistant body");
        assert!(!cleaned_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!cleaned_body.contains("진행 중 — Claude"));
    }

    #[test]
    fn task_notification_monitor_auto_turn_kind_unexposed_placeholder_deletes() {
        let ctx = task_notification_terminal_ctx(
            "⠋ 계속 처리 중",
            0,
            Some(TaskNotificationKind::MonitorAutoTurn),
        );

        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
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
