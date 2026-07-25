macro_rules! log_command_received {
    ($channel_id:expr, $user_name:expr, $command:expr $(, $($fields:tt)+)?) => {
        log_info_event!(
            "discord_command_received",
            channel_id = $channel_id,
            user_name = %$user_name,
            command = %$command,
            $($($fields)+,)?
        );
    };
}

macro_rules! log_info_event {
    ($event:literal, $($fields:tt)*) => {
        tracing::info!(event = $event, $($fields)* $event)
    };
}

mod command_policy;
mod config;
mod control;
mod diagnostics;
mod fast_mode;
mod goals;
mod help;
mod inspect;
mod meeting_cmd;
mod model_picker;
mod model_ui;
mod node;
mod receipt;
mod recovery_ops;
mod restart;
mod resume_autocomplete;
mod session;
mod sidecar;
mod skill;
mod text_commands;
mod tui_passthrough;
mod voice;

pub(super) const STOPPING_RESPONSE: &str = "중지하고 있어요...";
pub(super) const ALREADY_STOPPING_RESPONSE: &str = "이미 중지 중이에요.";
pub(super) const NO_ACTIVE_TURN_RESPONSE: &str = "중지할 활성 턴이 없어요.";
pub(super) const SESSION_CLEARED_RESPONSE: &str = "세션을 초기화했어요.";

pub(super) fn session_started_response(path: &str) -> String {
    format!("`{path}`에서 세션을 시작했어요.")
}

pub(super) fn session_restored_response(path: &str) -> String {
    format!("`{path}`에서 세션을 복원했어요.")
}

pub(in crate::services::discord) fn owner_error_response(summary: &str, detail: &str) -> String {
    const DETAIL_PREVIEW_CHARS: usize = 1_400;

    let detail = detail.trim();
    let truncated = detail.chars().count() > DETAIL_PREVIEW_CHARS;
    let preview: String = detail.chars().take(DETAIL_PREVIEW_CHARS).collect();
    let preview = super::formatting::escape_for_code_fence(&preview).replace("||", "|\u{200b}|");
    let suffix = if truncated {
        "\n…(이하 생략)"
    } else {
        ""
    };

    format!("⚠️ {summary}\n||**상세**\n```text\n{preview}{suffix}\n```||")
}

pub(in crate::services::discord) fn shell_command_output_response(
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> String {
    let mut parts = Vec::new();
    if !stdout.is_empty() {
        parts.push(format!("```\n{}\n```", stdout.trim_end()));
    }
    if !stderr.is_empty() {
        parts.push(shell_command_stderr_response(stderr));
    }
    if parts.is_empty() || exit_code != 0 {
        parts.push(format!("(종료 코드: {exit_code})"));
    }
    parts.join("\n")
}

pub(in crate::services::discord) fn shell_command_stderr_response(stderr: &str) -> String {
    owner_error_response("셸 명령이 오류 출력을 반환했어요.", stderr)
}

pub(in crate::services::discord) fn shell_command_execution_error_response(detail: &str) -> String {
    owner_error_response("셸 명령을 실행하지 못했어요.", detail)
}

pub(in crate::services::discord) fn shell_command_task_error_response(detail: &str) -> String {
    owner_error_response("셸 명령을 처리하는 중 오류가 발생했어요.", detail)
}

pub(super) fn shell_command_timeout_response(detail: &str) -> String {
    owner_error_response(
        "셸 명령이 제한 시간을 초과해 중지됐어요.\n명령을 나누거나 경로 범위를 좁힌 뒤, `--exclude-dir`/`-name` 필터를 추가해 다시 시도해 주세요.",
        detail,
    )
}

#[allow(unused_imports)]
pub(in crate::services::discord) use command_policy::{CommandRisk, PolicyDecision};
pub(in crate::services::discord) use command_policy::{
    command_risk, evaluate_policy, high_risk_enabled_via_env, risk_tier_summary_for_help,
    slash_command_risk,
};

pub(in crate::services::discord) use super::model_catalog::{
    provider_supports_model_override, validate_model_input,
};
#[cfg(unix)]
pub(in crate::services::discord) use config::effective_provider_for_channel;
pub(in crate::services::discord) use config::{
    ModelPickerAction, build_allowall_policy_note, build_model_picker_components_from_snapshot,
    build_model_picker_embed_from_snapshot, channel_codex_goals_setting, channel_fast_mode_setting,
    clear_model_picker_pending, current_working_dir, effective_model_snapshot,
    model_picker_pending_to_override, parse_model_picker_custom_id, resolve_model_for_turn,
    update_channel_model_override, would_channel_model_override_change,
};
pub(super) use config::{cmd_adduser, cmd_allowall, cmd_allowed, cmd_allowedtools, cmd_removeuser};
pub(in crate::services::discord) use control::{
    SoftClearNotifyMode, clear_channel_session_state, clear_channel_session_state_with_session_key,
    reset_channel_provider_state, reset_managed_process_session, reset_provider_session_if_pending,
};
pub(super) use control::{cmd_cancel_queued, cmd_clear, cmd_down, cmd_shell, cmd_stop};
pub(in crate::services::discord) use diagnostics::{
    build_health_report, build_inflight_report, build_queue_report, build_status_report,
};
pub(super) use diagnostics::{
    cmd_adk_phase, cmd_debug, cmd_deletesession, cmd_health, cmd_inflight, cmd_metrics, cmd_queue,
    cmd_sessions, cmd_status,
};
pub(super) use fast_mode::cmd_fast;
pub(super) use goals::cmd_goals;
pub(super) use help::cmd_help;
pub(super) use inspect::cmd_adk;
pub(super) use meeting_cmd::cmd_meeting;
pub(super) use model_picker::cmd_model;
pub(super) use node::cmd_node;
pub(in crate::services::discord) use node::{
    channel_node_override, handle_node_picker_interaction, is_node_picker_custom_id,
};
pub(super) use receipt::{cmd_receipt, cmd_usage};
pub(super) use recovery_ops::{cmd_deadlock_recover, cmd_machine_flip, cmd_stuck_pr_rebase};
pub(super) use restart::cmd_restart;
pub(super) use session::{cmd_pwd, cmd_resume, cmd_start};
pub(super) use sidecar::cmd_sidecar;
pub(in crate::services::discord) use skill::build_provider_skill_prompt;
pub(super) use skill::{cmd_cc, cmd_skill};
pub(in crate::services::discord) use text_commands::handle_text_command_with_uploads;
pub(super) use tui_passthrough::{cmd_compact, cmd_context, cmd_cost, cmd_effort};
pub(in crate::services::discord) use voice::{
    auto_join_voice_channels, handle_vc_text_command, join_voice_channel, notify_voice_alert,
    register_songbird, voice_occupancy,
};
pub(super) use voice::{cmd_vc_join, cmd_vc_leave, cmd_voice};

#[cfg(test)]
mod response_wording_tests {
    use super::super::{DiscordBotSettings, discord_io::user_is_authorized};
    use super::{
        ALREADY_STOPPING_RESPONSE, NO_ACTIVE_TURN_RESPONSE, SESSION_CLEARED_RESPONSE,
        STOPPING_RESPONSE, owner_error_response,
        resume_autocomplete::{
            CandidateView, DISCORD_AUTOCOMPLETE_LIMIT, DISCORD_CHOICE_NAME_LIMIT, build_choices,
            candidate_label, candidate_matches,
        },
        session_restored_response, session_started_response,
        shell_command_execution_error_response, shell_command_output_response,
        shell_command_task_error_response,
    };

    #[test]
    fn shared_command_states_use_korean_responses() {
        assert_eq!(STOPPING_RESPONSE, "중지하고 있어요...");
        assert_eq!(ALREADY_STOPPING_RESPONSE, "이미 중지 중이에요.");
        assert_eq!(NO_ACTIVE_TURN_RESPONSE, "중지할 활성 턴이 없어요.");
        assert_eq!(SESSION_CLEARED_RESPONSE, "세션을 초기화했어요.");
        assert_eq!(
            session_started_response("/tmp/work"),
            "`/tmp/work`에서 세션을 시작했어요."
        );
        assert_eq!(
            session_restored_response("/tmp/work"),
            "`/tmp/work`에서 세션을 복원했어요."
        );
    }

    #[test]
    fn owner_error_detail_is_folded_and_markdown_safe() {
        let response = owner_error_response("명령을 실행하지 못했어요.", "bad ``` fence || leak");

        assert!(response.starts_with("⚠️ 명령을 실행하지 못했어요.\n||**상세**"));
        assert!(response.ends_with("```||"));
        assert!(!response.contains("bad ``` fence"));
        assert!(!response.contains("|| leak"));
    }

    #[test]
    fn bare_shell_command_error_uses_shared_folded_korean_rendering() {
        let stderr = "bad ``` fence || leak";
        let bare_command_response = shell_command_output_response("", stderr, 1);
        let explicit_shell_response = shell_command_output_response("", stderr, 1);

        assert_eq!(bare_command_response, explicit_shell_response);
        assert!(
            bare_command_response
                .starts_with("⚠️ 셸 명령이 오류 출력을 반환했어요.\n||**상세**\n```text\n")
        );
        assert!(bare_command_response.ends_with("```||\n(종료 코드: 1)"));
        assert!(!bare_command_response.contains("bad ``` fence"));
        assert!(!bare_command_response.contains("|| leak"));

        let execution_error = shell_command_execution_error_response(stderr);
        assert!(execution_error.starts_with("⚠️ 셸 명령을 실행하지 못했어요."));
        assert!(execution_error.ends_with("```||"));

        let task_error = shell_command_task_error_response(stderr);
        assert!(task_error.starts_with("⚠️ 셸 명령을 처리하는 중 오류가 발생했어요."));
        assert!(task_error.ends_with("```||"));
    }

    #[test]
    fn resume_candidate_label_is_unicode_safe_and_uses_uuid_fallback() {
        let long_title = CandidateView {
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            cwd: "/tmp/claude-adk-cc-20260101-000001".to_string(),
            title: Some("한글제목".repeat(40)),
            modified_at_ms: 0,
        };
        let label = candidate_label(&long_title, 86_400_000);
        assert!(label.chars().count() <= DISCORD_CHOICE_NAME_LIMIT);
        assert!(label.is_char_boundary(label.len()));
        assert!(label.contains(" · 1일 전 · claude-adk-cc-20260101-000001"));

        let untitled = CandidateView {
            session_id: "22222222-2222-2222-2222-222222222222".to_string(),
            cwd: "/tmp/work\nname".to_string(),
            title: None,
            modified_at_ms: 59_000,
        };
        let label = candidate_label(&untitled, 60_000);
        assert!(label.starts_with(&untitled.session_id));
        assert!(!label.contains('\n'));
        assert!(label.contains("방금 전"));
    }

    #[test]
    fn resume_choices_filter_candidates_and_use_bounded_marker_values() {
        let matchable = CandidateView {
            session_id: "11111111-1111-1111-1111-111111111111".to_string(),
            cwd: "/tmp/Feature-Tree".to_string(),
            title: Some("Resume Design".to_string()),
            modified_at_ms: 0,
        };
        assert!(candidate_matches(&matchable, "resume"));
        assert!(candidate_matches(&matchable, "11111111"));
        assert!(candidate_matches(&matchable, "feature-tree"));
        assert!(!candidate_matches(&matchable, "missing"));

        let candidates = (0..30).map(|index| CandidateView {
            session_id: format!("00000000-0000-0000-0000-{index:012}"),
            cwd: "/tmp/worktree".to_string(),
            title: Some(format!("Candidate {index}")),
            modified_at_ms: 0,
        });
        let choices = build_choices(candidates, "", 1_000);
        assert_eq!(choices.len(), DISCORD_AUTOCOMPLETE_LIMIT);
        let serialized = serde_json::to_value(&choices[0]).expect("serialize choice");
        assert!(
            serialized["value"]
                .as_str()
                .is_some_and(|value| value.starts_with("pick:"))
        );
    }

    #[test]
    fn resume_authorization_includes_owner_allowlist_and_allow_all() {
        let mut settings = DiscordBotSettings {
            owner_user_id: Some(10),
            allowed_user_ids: vec![20],
            ..DiscordBotSettings::default()
        };
        assert!(user_is_authorized(&settings, 10));
        assert!(user_is_authorized(&settings, 20));
        assert!(!user_is_authorized(&settings, 30));
        settings.allow_all_users = true;
        assert!(user_is_authorized(&settings, 30));
    }
}

/// Apply the issue #1005 owner guard to a slash command.
///
/// Returns `Ok(true)` if the caller may proceed, `Ok(false)` if the request
/// was denied (a denial message has already been posted to the channel via
/// `ctx.say(...)`). Slash command handlers should:
///
/// ```ignore
/// if !enforce_slash_command_policy(&ctx, "/shell").await? {
///     return Ok(());
/// }
/// ```
///
/// Mirrors the gate at the top of `text_commands::handle_text_command` so a
/// non-owner with `allow_all_users=true` cannot invoke `/shell`, `/allowed`,
/// `/clear`, etc. Codex review of issue #1005 PR caught that the slash
/// surface was missing this check.
pub(in crate::services::discord) async fn enforce_slash_command_policy(
    ctx: &super::Context<'_>,
    slash_cmd: &str,
) -> Result<bool, super::Error> {
    let risk = slash_command_risk(slash_cmd);
    if !risk.is_high_risk() {
        return Ok(true);
    }
    let is_owner = super::check_owner(ctx.author().id, &ctx.data().shared).await;
    let high_risk_enabled = high_risk_enabled_via_env();
    let decision = evaluate_policy(risk, is_owner, high_risk_enabled);
    if let Some(reply) = decision.denial_message(slash_cmd) {
        tracing::warn!(
            event = "discord_command_denied",
            channel_id = ctx.channel_id().get(),
            user_id = ctx.author().id.get(),
            user_name = %ctx.author().name,
            command = slash_cmd,
            reason = "command_policy",
            risk = ?risk,
            "discord_command_denied"
        );
        ctx.say(reply).await?;
        return Ok(false);
    }
    Ok(true)
}
