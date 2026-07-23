use super::super::gateway::{
    DiscordGateway, HeadlessGateway, LiveDiscordTurnContext, send_intake_placeholder,
};
use super::super::*;
pub(in crate::services::discord) use super::authorization::{
    TurnKind, classify_turn_kind_from_author,
};
use super::dispatch_trigger::{
    dispatch_session_path_should_update, dispatch_should_recover_session_worktree,
    parse_dispatch_context_hints, resolve_dispatch_target_repo_dir,
};
use super::response_format::{
    build_headless_trigger_context, build_memory_injection_plan, build_race_requeued_intervention,
    build_system_discord_context, dispatch_profile_label, memento_recall_gate_decision,
    merge_reply_contexts, should_note_memento_context_loaded, wrap_user_prompt_with_author,
};
pub(in crate::services::discord) use super::turn_start::reserve_headless_turn;
pub(crate) use super::turn_start::{
    HeadlessTurnReservation, HeadlessTurnStartError, HeadlessTurnStartOutcome,
    HeadlessTurnStartStatus,
};
use super::turn_start::{
    cli_just_spawned_for_emit, dispatch_reset_lifecycle_code, emit_session_strategy_lifecycle,
    load_session_runtime_state, log_session_strategy_diagnostic, put_back_session_retry_context,
    put_back_turn_end_wip_warning, put_back_voluntary_feedback_reminder,
    refresh_session_strategy_after_pending_reset, release_mailbox_after_hosted_tui_busy_pre_submit,
    release_mailbox_after_placeholder_post_failure, session_runtime_state_after_redirect,
    take_and_merge_feedback_reminder, take_and_merge_wip_warning, take_session_retry_context,
};
#[cfg(test)]
use super::turn_start::{session_strategy_lifecycle_event, should_emit_session_strategy_lifecycle};
use crate::services::agent_protocol::RuntimeHandoffKind;
#[cfg(unix)]
use crate::services::discord::tmux_reaper::heal_stale_busy_mailbox;
use crate::services::memory::{
    RecallMode, RecallRequest, RecallResponse, RecallSizeBucket, build_memory_backend,
    note_recall_context_size, resolve_memory_role_id, resolve_memory_session_id,
};
#[cfg(test)]
use crate::services::observability::turn_lifecycle::TurnEvent;
use crate::services::provider::CancelToken;
use std::future::Future;
use std::sync::Arc;
use url::Url;
mod attachments;
mod control;
mod goal_lifecycle;
mod headless_turn;
mod intake_turn;
mod latency_spans;
mod provider_isolation;
mod tui_followup;
mod turn_lifecycle;
pub(in crate::services::discord) mod typing_indicator;
mod voice_announcement_route;
mod voice_announcement_scope;
mod watchdog;

use self::goal_lifecycle::*;
use self::provider_isolation::*;
use self::tui_followup::*;
pub(in crate::services::discord) use self::turn_lifecycle::mailbox_try_start_turn_with_terminal_marker_cleanup;
use self::turn_lifecycle::{
    cleanup_terminal_delivery_marker_after_turn_start, should_add_turn_pending_reaction,
};
use self::watchdog::*;

/// Claim an intake turn, healing a stale busy mailbox and retrying once when
/// its managed tmux session is proven absent. On non-Unix platforms the tmux
/// self-heal is unavailable, so this preserves the ordinary single claim.
async fn try_start_turn_with_stale_busy_heal(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: serenity::UserId,
    user_msg_id: serenity::MessageId,
    context: (
        (Option<&str>, &ProviderKind, Option<&str>),
        Option<LaunchState<'_>>,
    ),
) -> bool {
    let ((session_key, provider, tmux_session_name), mut launch) = context;
    let started = mailbox_try_start_turn_with_terminal_marker_cleanup(
        shared,
        channel_id,
        cancel_token.clone(),
        request_owner,
        user_msg_id,
        session_key,
    )
    .await;

    #[cfg(unix)]
    let started = if !started
        && let Some(tmux_session_name) = tmux_session_name
        && heal_stale_busy_mailbox(
            shared,
            provider,
            channel_id,
            tmux_session_name,
            "discord_intake",
        )
        .await
    {
        mailbox_try_start_turn_with_terminal_marker_cleanup(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
            session_key,
        )
        .await
    } else {
        started
    };

    #[cfg(not(unix))]
    let _ = (provider, tmux_session_name);
    if started && let Some(launch) = launch.take() {
        refresh_claimed_runtime_for_launch(
            shared,
            provider,
            channel_id,
            true,
            (launch.path, launch.session_id, launch.loaded, launch.reason),
        )
        .await;
    }
    started
}

fn intake_claim_context<'a>(
    session_key: Option<&'a str>,
    provider: &'a ProviderKind,
    tmux_session_name: Option<&'a str>,
    path: &'a mut String,
    session_id: &'a mut Option<String>,
    loaded: &'a mut bool,
    reason: &'a mut &'static str,
) -> (
    (Option<&'a str>, &'a ProviderKind, Option<&'a str>),
    Option<LaunchState<'a>>,
) {
    (
        (session_key, provider, tmux_session_name),
        Some(LaunchState::new(path, session_id, loaded, reason)),
    )
}

impl<'a> LaunchState<'a> {
    fn new(
        path: &'a mut String,
        session_id: &'a mut Option<String>,
        loaded: &'a mut bool,
        reason: &'a mut &'static str,
    ) -> Self {
        Self {
            path,
            session_id,
            loaded,
            reason,
        }
    }
}

struct LaunchState<'a> {
    path: &'a mut String,
    session_id: &'a mut Option<String>,
    loaded: &'a mut bool,
    reason: &'a mut &'static str,
}

async fn resolve_channel_runtime_for_launch(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    mut current: (String, Option<String>, bool, &'static str),
) -> (
    String,
    Option<String>,
    bool,
    &'static str,
    Option<String>,
    Option<String>,
) {
    let mut data = shared.core.lock().await;
    let channel_name = data
        .sessions
        .get(&channel_id)
        .and_then(|session| session.channel_name.clone());
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        let refreshed_path = session.validated_path(channel_id);
        let session_changed = session.session_id != current.1;
        let loaded_changed = session.memento_context_loaded != current.2;
        let path_changed = refreshed_path
            .as_deref()
            .is_some_and(|path| path != current.0);
        let pending_reset = current.1.is_some() && session.session_id.is_none();
        if pending_reset {
            current.1 = None;
            current.2 = session.memento_context_loaded;
            current.3 = "explicit_provider_reset";
        } else if refreshed_path.is_some() && (session_changed || loaded_changed || path_changed) {
            current.0 = refreshed_path.expect("checked usable runtime path");
            current.1 = session.session_id.clone();
            current.2 = session.memento_context_loaded;
            current.3 = "runtime_session_rebound";
        }
    }
    let tmux_session_name = if provider.uses_managed_tmux_backend() {
        channel_name
            .as_ref()
            .map(|name| provider.build_tmux_session_name(name))
    } else {
        None
    };
    (
        current.0,
        current.1,
        current.2,
        current.3,
        channel_name,
        tmux_session_name,
    )
}

#[cfg_attr(not(test), allow(dead_code))]
async fn refresh_claimed_runtime_for_launch(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    started: bool,
    state: (
        &mut String,
        &mut Option<String>,
        &mut bool,
        &mut &'static str,
    ),
) {
    if !started {
        return;
    }
    let current = (state.0.clone(), state.1.clone(), *state.2, *state.3);
    let runtime = resolve_channel_runtime_for_launch(shared, provider, channel_id, current).await;
    *state.0 = runtime.0;
    *state.1 = runtime.1;
    *state.2 = runtime.2;
    *state.3 = runtime.3;
}

pub(super) use self::attachments::handle_file_upload;
pub(super) use self::control::{handle_shell_command_raw, handle_text_command};
#[allow(unused_imports)]
pub(in crate::services::discord) use self::headless_turn::{
    start_headless_turn, start_reserved_headless_turn, start_voice_headless_turn,
};
pub(in crate::services::discord) use self::intake_turn::IntakeDeps;
pub(crate) use self::intake_turn::{IntakeRequest, execute_intake_turn_core};
// #4270 — pre-teardown hosted-TUI readiness probe + live-dispatch defer for the
// queued-turn promote entrypoints (idle kickoff in discord/mod.rs, live
// dispatch in gateway.rs).
#[cfg(test)]
pub(in crate::services::discord) use self::tui_followup::set_hosted_tui_promote_busy_for_tests;
pub(in crate::services::discord) use self::tui_followup::{
    defer_promoted_dispatch_if_hosted_tui_busy, hosted_tui_promote_readiness_blocked,
};

pub(super) async fn finish_admitted_local(
    deps: &IntakeDeps<'_>,
    request: IntakeRequest,
    preserve_on_cancel: bool,
    queued_drain: bool,
    preloaded_uploads: Vec<String>,
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> Result<(), Error> {
    let IntakeRequest {
        channel_id,
        user_msg_id,
        request_owner,
        request_owner_name,
        user_text,
        reply_to_user_message,
        defer_watcher_resume,
        wait_for_completion,
        merge_consecutive,
        reply_context,
        has_reply_boundary,
        dm_hint,
        turn_kind,
        preserve_on_cancel: _,
    } = request;
    intake_turn::handle_text_message(
        deps,
        channel_id,
        user_msg_id,
        request_owner,
        &request_owner_name,
        &user_text,
        reply_to_user_message,
        defer_watcher_resume,
        wait_for_completion,
        merge_consecutive,
        reply_context,
        has_reply_boundary,
        dm_hint,
        turn_kind,
        preserve_on_cancel,
        queued_drain,
        preloaded_uploads,
        voice_announcement,
    )
    .await
}

#[cfg(test)]
mod session_strategy_lifecycle_tests;
