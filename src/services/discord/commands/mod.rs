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
mod receipt;
mod restart;
mod session;
mod skill;
mod text_commands;
mod voice;

#[allow(unused_imports)]
pub(in crate::services::discord) use command_policy::{CommandRisk, PolicyDecision};
pub(in crate::services::discord) use command_policy::{
    command_risk, evaluate_policy, high_risk_enabled_via_env, risk_tier_summary_for_help,
    slash_command_risk,
};

pub(in crate::services::discord) use super::model_catalog::{
    provider_supports_model_override, validate_model_input,
};
pub(in crate::services::discord) use config::{
    ModelPickerAction, build_allowall_policy_note, build_model_picker_components_from_snapshot,
    build_model_picker_embed_from_snapshot, channel_codex_goals_setting, channel_fast_mode_setting,
    clear_model_picker_pending, current_working_dir, effective_model_snapshot,
    model_picker_pending_to_override, parse_model_picker_custom_id, resolve_model_for_turn,
    update_channel_model_override, would_channel_model_override_change,
};
pub(super) use config::{cmd_adduser, cmd_allowall, cmd_allowed, cmd_allowedtools, cmd_removeuser};
pub(in crate::services::discord) use control::{
    clear_channel_session_state, notify_turn_stop, reset_channel_provider_state,
    reset_managed_process_session, reset_provider_session_if_pending,
};
pub(super) use control::{cmd_clear, cmd_down, cmd_shell, cmd_stop};
pub(in crate::services::discord) use diagnostics::{
    build_health_report, build_inflight_report, build_queue_report, build_status_report,
};
pub(super) use diagnostics::{
    cmd_debug, cmd_deletesession, cmd_health, cmd_inflight, cmd_metrics, cmd_queue, cmd_sessions,
    cmd_status,
};
pub(super) use fast_mode::cmd_fast;
pub(super) use goals::cmd_goals;
pub(super) use help::cmd_help;
pub(super) use inspect::cmd_adk;
pub(super) use meeting_cmd::cmd_meeting;
pub(super) use model_picker::cmd_model;
pub(super) use receipt::cmd_receipt;
pub(super) use restart::cmd_restart;
pub(super) use session::{cmd_pwd, cmd_start};
pub(in crate::services::discord) use skill::build_provider_skill_prompt;
pub(super) use skill::cmd_cc;
pub(in crate::services::discord) use text_commands::handle_text_command;
pub(in crate::services::discord) use voice::{
    auto_join_voice_channels, handle_vc_text_command, register_songbird,
};
pub(super) use voice::{cmd_vc_join, cmd_vc_leave};

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
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⛔ CommandPolicy denied {} for {} (id:{}) — risk={:?}",
            slash_cmd,
            ctx.author().name,
            ctx.author().id.get(),
            risk,
        );
        ctx.say(reply).await?;
        return Ok(false);
    }
    Ok(true)
}
