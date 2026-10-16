use poise::serenity_prelude as serenity;
use serenity::{CreateAttachment, MessageId};
use std::path::Path;
use std::sync::Arc;

use crate::services::provider::{CancelToken, ProviderKind};

use super::super::admin_host_guard::ManagedReset;
use super::super::catch_up::retry_state::clear_channel_discarding_catch_up_backlog;
use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::queue_io::mailbox_cancel_queued_primary_message;
use super::super::settings::save_bot_settings;
use super::super::turn_bridge::stop_active_turn_with_outcome;
use super::super::{Context, Error, SharedData, check_auth, saturating_decrement_global_active};
mod clear_report;
mod clear_teardown;
pub(in crate::services::discord) use clear_report::ClearReport;
pub(in crate::services::discord) mod home_fence;
mod managed_reset;
#[cfg(all(test, unix))]
use managed_reset::VERIFIED_CODEX_RESET_REFUSAL;
pub(in crate::services::discord) use managed_reset::reset_managed_process_session;
pub(in crate::services::discord) use managed_reset::{
    verified_codex_reset_refusal, verified_codex_reset_refusal_for_target,
};
pub(in crate::services::discord) mod native;
pub(in crate::services::discord) use native::native_clear_admits;

use super::config::{
    clear_codex_goals_reset_pending_for_channel, clear_fast_mode_reset_pending_for_provider,
    fast_mode_reset_pending_for_provider, fast_mode_reset_pending_key, sync_session_reset_pending,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionClearBehavior {
    ResetManagedProcess,
    Noop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum SoftClearNotifyMode {
    Enqueue,
    Suppress,
}

impl SoftClearNotifyMode {
    fn should_enqueue(self) -> bool {
        matches!(self, Self::Enqueue)
    }
}

const SOFT_CLEAR_REASON_CODE: &str = "lifecycle.soft_clear";

fn soft_clear_lifecycle_notify_row(
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
) -> Option<(&'static str, String)> {
    notify_mode.should_enqueue().then(|| {
        (
            SOFT_CLEAR_REASON_CODE,
            format!("🧹 세션 클리어 ({clear_source})"),
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionResetBehavior {
    ResetManagedProcess,
    Noop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSessionResetPlan {
    reset_source: &'static str,
    recreate_tmux: bool,
}

fn managed_session_clear_behavior(provider: &ProviderKind) -> ManagedSessionClearBehavior {
    match provider {
        // Claude/Codex/Qwen keep reusable local wrapper state; `/clear` must
        // drop that process/tmux state instead of sending provider-native keys.
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionClearBehavior::ResetManagedProcess
        }
        ProviderKind::Gemini
        | ProviderKind::Grok
        | ProviderKind::Antigravity
        | ProviderKind::OpenCode
        | ProviderKind::Unsupported(_) => ManagedSessionClearBehavior::Noop,
    }
}

fn managed_session_reset_behavior(provider: &ProviderKind) -> ManagedSessionResetBehavior {
    match provider {
        ProviderKind::Claude => ManagedSessionResetBehavior::ResetManagedProcess,
        ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionResetBehavior::ResetManagedProcess
        }
        ProviderKind::Gemini
        | ProviderKind::Grok
        | ProviderKind::Antigravity
        | ProviderKind::OpenCode
        | ProviderKind::Unsupported(_) => ManagedSessionResetBehavior::Noop,
    }
}

fn pending_session_reset_plan(
    provider: &ProviderKind,
    fast_mode_reset_pending: bool,
    codex_goals_reset_pending: bool,
    model_reset_pending: bool,
) -> Option<PendingSessionResetPlan> {
    if fast_mode_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "fast mode reset pending",
            recreate_tmux: matches!(provider, ProviderKind::Claude | ProviderKind::Codex),
        });
    }
    if codex_goals_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "codex goals reset pending",
            recreate_tmux: matches!(provider, ProviderKind::Codex),
        });
    }
    if model_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "model session reset pending",
            recreate_tmux: false,
        });
    }
    None
}

async fn resolve_session_key_for_clear(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    if let Some(key) =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider, None).await
    {
        return Some(key);
    }

    let live_channel_name =
        channel_id
            .to_channel(http)
            .await
            .ok()
            .and_then(|channel| match channel {
                serenity::Channel::Guild(guild_channel) => Some(guild_channel.name),
                _ => None,
            });
    let channel_name = fallback_channel_name_for_clear(
        live_channel_name.as_deref(),
        super::super::resolve_thread_parent(http, channel_id).await,
        channel_id,
    )?;
    Some(build_fallback_session_key_for_clear(
        &shared.token_hash,
        provider,
        &channel_name,
    ))
}

fn fallback_channel_name_for_clear(
    live_channel_name: Option<&str>,
    thread_parent: Option<(serenity::ChannelId, Option<String>)>,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    if let Some((parent_id, parent_name)) = thread_parent {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(super::super::synthetic_thread_channel_name(
            &parent_name,
            channel_id,
        ));
    }

    live_channel_name.map(ToOwned::to_owned)
}

fn build_fallback_session_key_for_clear(
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> String {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    super::super::adk_session::build_namespaced_session_key(token_hash, provider, &tmux_name)
}

#[cfg(test)]
mod clear_wait_tests;
#[cfg(test)]
#[cfg(unix)]
mod codex_verified_clear_tests;

#[allow(clippy::too_many_arguments)]
mod provider_reset;
pub(in crate::services::discord) use provider_reset::{
    reset_channel_provider_state, reset_channel_provider_state_for_home_drain,
};

/// The reset a pending flag asked for, if any. A refused one keeps its flags and is reported;
/// the caller's turn runs on the session as it is, as with any turn the host guard admits.
pub(in crate::services::discord) async fn reset_provider_session_if_pending(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    fast_mode_channel_id: serenity::ChannelId,
) -> Option<ManagedReset> {
    let fast_mode_reset_pending =
        fast_mode_reset_pending_for_provider(shared, fast_mode_channel_id, provider);
    let codex_goals_reset_pending = matches!(provider, ProviderKind::Codex)
        && shared
            .overrides
            .codex_goals_session_reset_pending
            .contains(&fast_mode_channel_id);
    let model_reset_pending = shared
        .overrides
        .model_session_reset_pending
        .contains(&channel_id);

    let Some(plan) = pending_session_reset_plan(
        provider,
        fast_mode_reset_pending,
        codex_goals_reset_pending,
        model_reset_pending,
    ) else {
        sync_session_reset_pending(shared, channel_id);
        if fast_mode_channel_id != channel_id {
            sync_session_reset_pending(shared, fast_mode_channel_id);
        }
        return None;
    };
    let reset = reset_channel_provider_state(
        http,
        shared,
        provider,
        channel_id,
        plan.reset_source,
        true,
        false,
        plan.recreate_tmux,
    )
    .await;
    // A refused reset keeps its pending flags; the channel is told why it did not apply.
    if reset.report(http, channel_id, plan.reset_source).await {
        return Some(reset);
    }

    if fast_mode_reset_pending {
        clear_fast_mode_reset_pending_for_provider(shared, fast_mode_channel_id, provider);
        persist_fast_mode_reset_marker(shared, fast_mode_channel_id, provider, false).await;
    }
    if codex_goals_reset_pending {
        clear_codex_goals_reset_pending_for_channel(shared, fast_mode_channel_id);
        persist_codex_goals_reset_marker(shared, fast_mode_channel_id, false).await;
    }
    if model_reset_pending {
        shared
            .overrides
            .model_session_reset_pending
            .remove(&channel_id);
    }
    sync_session_reset_pending(shared, channel_id);
    if fast_mode_channel_id != channel_id {
        sync_session_reset_pending(shared, fast_mode_channel_id);
    }
    Some(reset)
}

fn choose_clear_session_key(
    explicit_session_key: Option<&str>,
    resolved_session_key: Option<String>,
) -> Option<String> {
    explicit_session_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .or(resolved_session_key)
}

pub(in crate::services::discord) async fn clear_channel_session_state(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
) -> anyhow::Result<ClearReport> {
    clear_channel_session_state_with_session_key(
        http,
        shared,
        provider,
        channel_id,
        clear_source,
        notify_mode,
        None,
    )
    .await
}

pub(in crate::services::discord) async fn clear_channel_session_state_with_session_key(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
    explicit_session_key: Option<&str>,
) -> anyhow::Result<ClearReport> {
    clear_channel_session_state_fenced(
        http,
        shared,
        provider,
        channel_id,
        clear_source,
        notify_mode,
        explicit_session_key,
    )
    .await
}

/// Keep all stop sites converging on `stop_active_turn` so the
/// abort-key-then-SIGKILL ordering can never regress to the legacy pattern.
/// False when the released turn's host refused the stop, so it may still run.
async fn stop_released_turn(
    provider: &ProviderKind,
    token: Option<Arc<CancelToken>>,
    clear_source: &str,
) -> bool {
    let Some(token) = token else {
        return true;
    };
    let policy = super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession;
    let stop = stop_active_turn_with_outcome(provider, &token, policy, clear_source).await;
    // Only a legacy settlement says the stop reached the turn's host.
    stop.may_clear_inflight()
}

/// Only a channel another node holds is refused; past that the clear always commits.
async fn clear_channel_session_state_fenced(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
    explicit_session_key: Option<&str>,
) -> anyhow::Result<ClearReport> {
    let permit = home_fence::admit(channel_id, provider.as_str())?;
    #[cfg(test)]
    let permit = if home_fence::mutant("control_permit_removed") {
        drop(permit);
        None
    } else {
        permit
    };
    crate::services::cluster::channel_home::command_scope(
        permit,
        clear_channel_session_state_body(
            http,
            shared,
            provider,
            channel_id,
            clear_source,
            notify_mode,
            explicit_session_key,
        ),
    )
    .await
}

/// Discards the channel's queue, stops its turn, forgets its provider session and ends the
/// session's process; anything it could not confirm is one warning line, never a refusal.
async fn clear_channel_session_state_body(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
    explicit_session_key: Option<&str>,
) -> anyhow::Result<ClearReport> {
    home_fence::pause("clear").await;
    let mut report = ClearReport::default();
    // A pane effect holding the transition is waited out for the usual window, then overtaken.
    let transition = shared.session_transition_lock(channel_id);
    let wait = super::super::session_transition::SESSION_TRANSITION_LOCK_WAIT_TIMEOUT;
    let transition_guard = tokio::time::timeout(wait, transition.lock_owned()).await;
    if transition_guard.is_err() {
        tracing::warn!(
            channel_id = channel_id.get(),
            "clear proceeds past a held session transition"
        );
        report.transition_busy = true;
    }
    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .map(|ch_name| provider.build_tmux_session_name(ch_name))
    };

    // The clear commits here, in memory: no write below can restore what it discarded.
    let cleared = clear_channel_discarding_catch_up_backlog(shared, provider, channel_id).await;
    if cleared.removed_token.is_some() {
        saturating_decrement_global_active(shared);
    }
    report.retried = cleared.persistence.retried;
    report.failed = cleared.persistence.failed;
    clear_teardown::write_boundary(shared, channel_id, &mut report).await;

    native::clear_session_memory(shared, channel_id).await;
    native::clear_process_reset_pending(shared, channel_id).await;

    if !stop_released_turn(provider, cleared.removed_token, clear_source).await {
        report.stop_unconfirmed = true;
    }

    let resolved_session_key =
        resolve_session_key_for_clear(http, shared, channel_id, provider).await;
    let session_key = choose_clear_session_key(explicit_session_key, resolved_session_key);
    match session_key.as_deref() {
        Some(session_key) => {
            clear_teardown::forget_selector(shared, session_key, &mut report).await;
            super::super::adk_session::post_adk_session_status(
                Some(session_key),
                None,
                None,
                "idle",
                provider,
                None,
                Some(0),
                None,
                None,
                None,
                Some(channel_id),
                None,
                shared.api_port,
            )
            .await;
        }
        None => report.fail(
            "session_selector",
            "no session key resolved for the channel",
        ),
    }

    let session = (tmux_name.as_deref(), session_key.as_deref());
    let transition = transition_guard.ok();
    clear_teardown::end_session(
        shared,
        provider,
        channel_id,
        session,
        transition,
        &mut report,
    )
    .await;

    if let Some((reason_code, content)) = soft_clear_lifecycle_notify_row(clear_source, notify_mode)
    {
        let content = match report.warning() {
            Some(warning) => format!("{content}\n{warning}"),
            None => content,
        };
        // Notify bot message for clear paths that have no direct provider reply.
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            &format!("channel:{}", channel_id.get()),
            session_key.as_deref(),
            reason_code,
            &content,
        );
    }

    Ok(report)
}

/// /stop — Cancel in-progress AI request
///
/// Judges the turn's host before any write, then cancels it; token.cancelled
/// triggers turn_bridge loop exit → mailbox_finish_turn canonical cleanup
#[poise::command(slash_command, rename = "stop")]
pub(in crate::services::discord) async fn cmd_stop(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: runtime-control tier — owner-only regardless of
    // `allow_all_users`. Mirrors the text-surface gate in `handle_text_command`.
    if !super::enforce_slash_command_policy(&ctx, "/stop").await? {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/stop");

    let channel_id = ctx.channel_id();
    let forward_context =
        crate::services::session_forwarding::ForwardCallerContext::from_live_globals(
            ctx.data().shared.pg_pool.clone(),
        );
    // A channel delegated away goes to its holder, never to the legacy owner forward below.
    let provider = &ctx.data().provider;
    if let Some(reply) =
        super::stop::gateway_stop_reply(&forward_context, provider, channel_id).await
    {
        ctx.say(reply).await?;
        return Ok(());
    }
    match crate::services::session_forwarding::forward_remote_cancel_if_needed(
        &forward_context,
        &axum::http::HeaderMap::new(),
        &channel_id.get().to_string(),
        false,
    )
    .await
    {
        Ok(Some(_)) => {
            ctx.say(super::STOPPING_RESPONSE).await?;
            log_info_event!(
                "discord_cancel_acknowledged",
                channel_id = channel_id.get(),
                provider = ctx.data().provider.as_str(),
                status = "acknowledged",
            );
            return Ok(());
        }
        Ok(None) => {}
        Err(error) if error.status() == axum::http::StatusCode::NOT_FOUND => {
            ctx.say(super::NO_ACTIVE_TURN_RESPONSE).await?;
            return Ok(());
        }
        Err(error) => {
            tracing::error!(channel_id = channel_id.get(), error = %error, "/stop remote cancel failed closed");
            ctx.say("중지 요청을 owner에 전달하지 못했어요. 잠시 후 다시 시도해 주세요.")
                .await?;
            return Ok(());
        }
    }
    if home_fence::refused(&ctx).await? {
        return Ok(());
    }

    if let Err(error) = crate::services::session_forwarding::revalidate_local_cancel_owner(
        &forward_context,
        &channel_id.get().to_string(),
        None,
    )
    .await
    {
        tracing::error!(channel_id = channel_id.get(), error = %error, "/stop owner moved before local mutation");
        ctx.say("중지 요청 중 owner가 변경됐어요. 잠시 후 다시 시도해 주세요.")
            .await?;
        return Ok(());
    }

    let (shared, provider) = (&ctx.data().shared, &ctx.data().provider);
    let reply = super::stop::run_slash_stop(shared, provider, channel_id).await;
    ctx.say(reply.text()).await?;
    reply.finish(shared, provider, channel_id).await;
    Ok(())
}

pub(super) fn parse_queued_message_id(raw: &str) -> Option<MessageId> {
    raw.trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(MessageId::new)
}

/// /cancel-queued — Remove one queued message without affecting active work.
///
/// The target is an exact Discord message id. The mailbox actor serializes the
/// removal against dispatch, so a queued-to-active race is reported as stale
/// instead of cancelling the newly active turn.
#[poise::command(slash_command, rename = "cancel-queued")]
pub(in crate::services::discord) async fn cmd_cancel_queued(
    ctx: Context<'_>,
    #[description = "Queued Discord message ID"] message_id: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    if !super::enforce_slash_command_policy(&ctx, "/cancel-queued").await? {
        return Ok(());
    }

    let Some(message_id) = parse_queued_message_id(&message_id) else {
        ctx.say("유효한 큐 메시지 ID를 입력해 주세요.").await?;
        return Ok(());
    };
    if home_fence::refused(&ctx).await? {
        return Ok(());
    }

    let permit = home_fence::admit(ctx.channel_id(), ctx.data().provider.as_str())?;
    crate::services::cluster::channel_home::command_scope(permit, async {
        let removed = cancel_queued_with_home_permit(
            &ctx.data().shared,
            &ctx.data().provider,
            ctx.channel_id(),
            message_id,
        )
        .await?;
        if removed.is_some() {
            ctx.say(format!("큐 메시지 `{}`를 취소했어요.", message_id.get()))
                .await?;
        } else {
            ctx.say(format!(
                "큐 메시지 `{}`는 이미 처리됐거나 현재 채널의 대기열에 없어요.",
                message_id.get()
            ))
            .await?;
        }
        Ok(())
    })
    .await
}

async fn cancel_queued_with_home_permit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    message: MessageId,
) -> Result<Option<crate::services::turn_orchestrator::Intervention>, home_fence::CommandRefused> {
    let permit = home_fence::admit(channel, provider.as_str())?;
    crate::services::cluster::channel_home::command_scope(permit, async {
        home_fence::pause("queued").await;
        Ok(mailbox_cancel_queued_primary_message(shared, provider, channel, message).await)
    })
    .await
}

/// /clear — Clear AI conversation history
#[poise::command(slash_command, rename = "clear")]
pub(in crate::services::discord) async fn cmd_clear(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: runtime-control tier — owner-only.
    if !super::enforce_slash_command_policy(&ctx, "/clear").await? {
        return Ok(());
    }

    let permit = home_fence::admit(ctx.channel_id(), ctx.data().provider.as_str())?;
    clear_with_reply_scope(permit, async {
        log_command_received!(ctx.channel_id().get(), user_name, "/clear");

        let http = ctx.serenity_context().http.clone();
        let report = clear_channel_session_state(
            &http,
            &ctx.data().shared,
            &ctx.data().provider,
            ctx.channel_id(),
            "/clear",
            SoftClearNotifyMode::Suppress,
        )
        .await?;

        ctx.say(report.reply()).await?;
        log_info_event!(
            "discord_session_cleared",
            channel_id = ctx.channel_id().get(),
            provider = ctx.data().provider.as_str(),
            user_name = %user_name,
            status = "cleared",
        );
        Ok(())
    })
    .await
}

async fn clear_with_reply_scope<F: std::future::Future>(
    permit: Option<crate::services::cluster::channel_home::CommandPermit>,
    work: F,
) -> F::Output {
    #[cfg(test)]
    let permit = if home_fence::mutant("clear_reply_outside_scope") {
        drop(permit);
        None
    } else {
        permit
    };
    crate::services::cluster::channel_home::command_scope(permit, work).await
}

#[cfg(test)]
mod soft_clear_notify_tests {
    use poise::serenity_prelude::MessageId;

    use super::{
        SOFT_CLEAR_REASON_CODE, SoftClearNotifyMode, choose_clear_session_key,
        parse_queued_message_id, soft_clear_lifecycle_notify_row,
    };

    #[test]
    fn slash_and_text_clear_suppress_soft_clear_notify_row() {
        assert_eq!(
            soft_clear_lifecycle_notify_row("/clear", SoftClearNotifyMode::Suppress),
            None,
            "`/clear` and `!clear` already reply with the shared clear response and must not enqueue a duplicate `lifecycle.soft_clear` notify row"
        );
        assert_eq!(
            soft_clear_lifecycle_notify_row("!clear", SoftClearNotifyMode::Suppress),
            None,
            "`!clear` should leave the provider reply as the single user-visible completion surface"
        );
    }

    #[test]
    fn queued_cancel_parser_accepts_exact_nonzero_ids_only() {
        assert_eq!(parse_queued_message_id(" 42 "), Some(MessageId::new(42)));
        assert_eq!(parse_queued_message_id("0"), None);
        assert_eq!(parse_queued_message_id("stale"), None);
    }

    #[test]
    fn idle_recap_clear_keeps_soft_clear_notify_row() {
        assert_eq!(
            soft_clear_lifecycle_notify_row("idle_recap_clear", SoftClearNotifyMode::Enqueue),
            Some((
                SOFT_CLEAR_REASON_CODE,
                "🧹 세션 클리어 (idle_recap_clear)".to_string(),
            )),
            "idle recap clear has no provider reply, so it must keep the single user-visible `lifecycle.soft_clear` notify row"
        );
    }

    #[test]
    fn explicit_recap_session_key_wins_over_recomputed_channel_key() {
        assert_eq!(
            choose_clear_session_key(
                Some("claude/token/host:AgentDesk-claude-old-channel"),
                Some("claude/token/host:AgentDesk-claude-renamed-channel".to_string()),
            )
            .as_deref(),
            Some("claude/token/host:AgentDesk-claude-old-channel"),
            "idle recap clear must drop the exact session row that owns the recap card, even when channel-name drift changes the recomputed key"
        );
    }

    #[test]
    fn blank_explicit_session_key_falls_back_to_recomputed_key() {
        assert_eq!(
            choose_clear_session_key(
                Some("  "),
                Some("claude/token/host:AgentDesk-claude-channel".to_string()),
            )
            .as_deref(),
            Some("claude/token/host:AgentDesk-claude-channel")
        );
    }
}

#[cfg(test)]
mod clear_end_tests;
#[cfg(test)]
mod clear_entry_tests;
#[cfg(test)]
mod clear_persist_failure_tests;

/// /down <file> — Download file from server
#[poise::command(slash_command, rename = "down")]
pub(in crate::services::discord) async fn cmd_down(
    ctx: Context<'_>,
    #[description = "File path to download"] file: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/down", path = %file);

    let file_path = file.trim();
    if file_path.is_empty() {
        ctx.say("Usage: `/down <filepath>`\nExample: `/down /home/user/file.txt`")
            .await?;
        return Ok(());
    }

    // Resolve relative path
    let resolved_path = if Path::new(file_path).is_absolute() {
        file_path.to_string()
    } else {
        let current_path = {
            let mut data = ctx.data().shared.core.lock().await;
            data.sessions
                .get_mut(&ctx.channel_id())
                .and_then(|s| s.validated_path(ctx.channel_id()))
        };
        match current_path {
            Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_path),
            None => {
                ctx.say("No active session or session path is stale. Use absolute path or `/start <path>` first.")
                    .await?;
                return Ok(());
            }
        }
    };

    let path = Path::new(&resolved_path);
    if !path.exists() {
        ctx.say(format!("File not found: {}", resolved_path))
            .await?;
        return Ok(());
    }
    if !path.is_file() {
        ctx.say(format!("Not a file: {}", resolved_path)).await?;
        return Ok(());
    }

    // Send file as attachment
    let attachment = CreateAttachment::path(path).await?;
    ctx.send(poise::CreateReply::default().attachment(attachment))
        .await?;

    Ok(())
}

/// /shell <command> — Run shell command directly
#[poise::command(slash_command, rename = "shell")]
pub(in crate::services::discord) async fn cmd_shell(
    ctx: Context<'_>,
    #[description = "Shell command to execute"] command: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: shell/tool-grant tier — owner-only AND default-disabled.
    // Even `allow_all_users=true` must NOT unlock RCE.
    if !super::enforce_slash_command_policy(&ctx, "/shell").await? {
        return Ok(());
    }

    let preview = truncate_str(&command, 60);
    log_command_received!(ctx.channel_id().get(), user_name, "/shell", command_preview = %preview);

    // Defer for potentially long-running commands
    ctx.defer().await?;

    let working_dir = {
        let mut data = ctx.data().shared.core.lock().await;
        data.sessions
            .get_mut(&ctx.channel_id())
            .and_then(|s| s.validated_path(ctx.channel_id()))
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = command.clone();
    let working_dir_clone = working_dir.clone();

    let result = tokio::task::spawn_blocking(move || {
        let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match child {
            Ok(child) => child.wait_with_output(),
            Err(e) => Err(e),
        }
    })
    .await;

    let response = match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let mut parts = Vec::new();
            if !stdout.is_empty() {
                parts.push(format!("```\n{}\n```", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                parts.push(super::owner_error_response(
                    "셸 명령이 오류 출력을 반환했어요.",
                    stderr.trim_end(),
                ));
            }
            if parts.is_empty() {
                parts.push(format!("(종료 코드: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(종료 코드: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => super::owner_error_response("셸 명령을 실행하지 못했어요.", &e.to_string()),
        Err(e) => {
            super::owner_error_response("셸 명령을 처리하는 중 오류가 발생했어요.", &e.to_string())
        }
    };

    send_long_message_ctx(ctx, &response).await?;
    log_info_event!(
        "discord_shell_command_completed",
        channel_id = ctx.channel_id().get(),
        user_name = %user_name,
        status = "completed",
    );
    Ok(())
}

async fn persist_fast_mode_reset_marker(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    pending: bool,
) {
    let Some(token) = shared.http.cached_bot_token.get() else {
        return;
    };

    let channel_key = channel_id.get().to_string();
    let provider_key = fast_mode_reset_pending_key(channel_id, provider);
    let mut settings = shared.settings.write().await;
    if pending {
        settings
            .channel_fast_mode_reset_pending
            .remove(&channel_key);
        settings
            .channel_fast_mode_reset_pending
            .insert(provider_key);
    } else {
        settings
            .channel_fast_mode_reset_pending
            .remove(&channel_key);
        settings
            .channel_fast_mode_reset_pending
            .remove(&provider_key);
    }
    save_bot_settings(token, &settings);
}

async fn persist_codex_goals_reset_marker(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    pending: bool,
) {
    let Some(token) = shared.http.cached_bot_token.get() else {
        return;
    };

    let channel_key = channel_id.get().to_string();
    let mut settings = shared.settings.write().await;
    if pending {
        settings
            .channel_codex_goals_reset_pending
            .insert(channel_key);
    } else {
        settings
            .channel_codex_goals_reset_pending
            .remove(&channel_key);
    }
    save_bot_settings(token, &settings);
}

async fn clear_all_fast_mode_reset_markers(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    let Some(token) = shared.http.cached_bot_token.get() else {
        return;
    };

    let channel_key = channel_id.get().to_string();
    let suffix = format!(":{channel_key}");
    let mut settings = shared.settings.write().await;
    settings
        .channel_fast_mode_reset_pending
        .retain(|entry| entry != &channel_key && !entry.ends_with(&suffix));
    save_bot_settings(token, &settings);
}
