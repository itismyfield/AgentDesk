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

#[cfg(all(test, unix))]
mod cancel_backstop_tests;
mod command_policy;
mod config;
pub(in crate::services::discord) mod control;
mod diagnostics;
mod fast_mode;
mod goals;
mod help;
mod inspect;
mod model_picker;
mod model_ui;
mod node;
mod receipt;
mod restart;
mod session;
mod sidecar;
mod skill;
pub(in crate::services::discord) mod stop;
mod system_prompt;
mod text_commands;
mod tmux_recreate;
mod tool_call;
mod tui_passthrough;

pub(super) const STOPPING_RESPONSE: &str = "중지하고 있어요...";
pub(super) const ALREADY_STOPPING_RESPONSE: &str = "이미 중지 중이에요.";
pub(super) const NO_ACTIVE_TURN_RESPONSE: &str = "중지할 활성 턴이 없어요.";
pub(super) const HOST_REFUSED_STOP_RESPONSE: &str =
    "이 세션의 호스트를 확인하지 못해 중지하지 않았어요. 턴은 계속 진행돼요.";
pub(super) const SESSION_CLEARED_RESPONSE: &str = "세션을 초기화했어요.";

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

#[allow(unused_imports)]
pub(in crate::services::discord) use command_policy::{CommandRisk, PolicyDecision};
pub(in crate::services::discord) use command_policy::{
    evaluate_policy, high_risk_enabled_via_env, match_text_command, risk_tier_summary_for_help,
    slash_command_risk,
};

pub(in crate::services::discord) use super::model_catalog::{
    provider_supports_model_override, validate_model_input,
};
pub(super) use config::cmd_allowall;
#[cfg(unix)]
pub(in crate::services::discord) use config::effective_provider_for_channel;
pub(in crate::services::discord) use config::{
    ModelPickerAction, build_model_picker_components_from_snapshot,
    build_model_picker_embed_from_snapshot, channel_codex_goals_setting, channel_effort_level,
    channel_fast_mode_setting, clear_model_picker_pending, current_working_dir,
    effective_model_snapshot, model_picker_pending_to_override, parse_model_picker_custom_id,
    resolve_model_for_turn, update_channel_model_override, would_channel_model_override_change,
};
pub(in crate::services::discord) use control::{
    clear_channel_session_state, reset_channel_provider_state, reset_managed_process_session,
    reset_provider_session_if_pending,
};
pub(super) use control::{cmd_cancel_queued, cmd_clear, cmd_stop};
#[cfg(test)]
pub(in crate::services::discord) use diagnostics::{build_health_report, build_status_report};
pub(super) use diagnostics::{cmd_health, cmd_inflight, cmd_queue, cmd_status};
pub(super) use fast_mode::cmd_fast;
pub(super) use goals::cmd_goals;
pub(super) use help::cmd_help;
pub(super) use inspect::cmd_adk;
pub(super) use model_picker::cmd_model;
pub(super) use node::cmd_node;
pub(in crate::services::discord) use node::{
    channel_node_override, handle_node_picker_interaction, is_node_picker_custom_id,
};
pub(super) use receipt::cmd_usage;
pub(super) use restart::cmd_restart;
pub(super) use session::{cmd_pwd, cmd_resume};
pub(super) use sidecar::cmd_sidecar;
pub(super) use skill::cmd_skill;
pub(super) use system_prompt::cmd_system_prompt;
pub(in crate::services::discord) use text_commands::handle_text_command;
pub(super) use tool_call::cmd_tool_call;
pub(super) use tui_passthrough::{cmd_compact, cmd_effort};

#[cfg(test)]
mod response_wording_tests {
    use super::{
        ALREADY_STOPPING_RESPONSE, NO_ACTIVE_TURN_RESPONSE, SESSION_CLEARED_RESPONSE,
        STOPPING_RESPONSE, owner_error_response,
    };

    #[test]
    fn shared_command_states_use_korean_responses() {
        assert_eq!(STOPPING_RESPONSE, "중지하고 있어요...");
        assert_eq!(ALREADY_STOPPING_RESPONSE, "이미 중지 중이에요.");
        assert_eq!(NO_ACTIVE_TURN_RESPONSE, "중지할 활성 턴이 없어요.");
        assert_eq!(SESSION_CLEARED_RESPONSE, "세션을 초기화했어요.");
    }

    #[test]
    fn owner_error_detail_is_folded_and_markdown_safe() {
        let response = owner_error_response("명령을 실행하지 못했어요.", "bad ``` fence || leak");

        assert!(response.starts_with("⚠️ 명령을 실행하지 못했어요.\n||**상세**"));
        assert!(response.ends_with("```||"));
        assert!(!response.contains("bad ``` fence"));
        assert!(!response.contains("|| leak"));
    }
}

/// Apply the command risk policy to a slash command.
///
/// Returns `Ok(true)` if the caller may proceed, `Ok(false)` if the request
/// was denied (a denial message has already been posted to the channel via
/// `ctx.say(...)`). Slash command handlers should:
///
/// ```ignore
/// if !enforce_slash_command_policy(&ctx, "/clear").await? {
///     return Ok(());
/// }
/// ```
///
/// High-risk tiers stay owner-only even with `allow_all_users=true`.
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
