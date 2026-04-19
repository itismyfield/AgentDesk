use poise::serenity_prelude as serenity;
use serenity::CreateAttachment;
use std::path::Path;
use std::sync::Arc;

use crate::services::provider::ProviderKind;

use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::settings::cleanup_channel_uploads;
use super::super::turn_bridge::cancel_active_token;
use super::super::{
    Context, Error, SharedData, check_auth, mailbox_cancel_active_turn, mailbox_clear_channel,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionClearBehavior {
    NativeProviderClear,
    ResetManagedProcess,
    Noop,
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
        ProviderKind::Claude => ManagedSessionClearBehavior::NativeProviderClear,
        ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionClearBehavior::ResetManagedProcess
        }
        ProviderKind::Gemini | ProviderKind::Unsupported(_) => ManagedSessionClearBehavior::Noop,
    }
}

fn managed_session_reset_behavior(provider: &ProviderKind) -> ManagedSessionResetBehavior {
    match provider {
        ProviderKind::Claude => ManagedSessionResetBehavior::Noop,
        ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionResetBehavior::ResetManagedProcess
        }
        ProviderKind::Gemini | ProviderKind::Unsupported(_) => ManagedSessionResetBehavior::Noop,
    }
}

fn pending_session_reset_plan(
    provider: &ProviderKind,
    fast_mode_reset_pending: bool,
    model_reset_pending: bool,
    legacy_session_reset_pending: bool,
) -> Option<PendingSessionResetPlan> {
    if fast_mode_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "fast mode reset pending",
            recreate_tmux: matches!(provider, ProviderKind::Claude),
        });
    }
    if model_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "model session reset pending",
            recreate_tmux: false,
        });
    }
    if legacy_session_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "session reset pending",
            recreate_tmux: false,
        });
    }
    None
}

pub(in crate::services::discord) fn reset_managed_process_session(session_name: &str) -> bool {
    let lingering_pid =
        crate::services::session_backend::process_session_pid(session_name).map(|pid| pid as i32);
    if let Some(handle) = crate::services::session_backend::remove_process_session(session_name) {
        crate::services::session_backend::terminate_process_handle(handle);
        return true;
    }
    if let Some(pid) = lingering_pid {
        if let Ok(pid) = u32::try_from(pid) {
            crate::services::process::kill_pid_tree(pid);
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn recreate_tmux_session(session_name: &str, reset_source: &str) -> bool {
    if !crate::services::platform::tmux::has_session(session_name) {
        return false;
    }
    crate::services::tmux_diagnostics::record_tmux_exit_reason(
        session_name,
        &format!("hard reset via {reset_source}"),
    );
    crate::services::platform::tmux::kill_session(session_name)
}

#[cfg(not(unix))]
fn recreate_tmux_session(_session_name: &str, _reset_source: &str) -> bool {
    false
}

async fn resolve_session_key_for_clear(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    if let Some(key) =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider).await
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

pub(in crate::services::discord) async fn reset_channel_provider_state(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    reset_source: &str,
    reset_provider_state: bool,
    clear_history: bool,
    recreate_tmux: bool,
) -> Option<String> {
    let tmux_name = {
        let mut data = shared.core.lock().await;
        data.sessions.get_mut(&channel_id).and_then(|session| {
            if reset_provider_state {
                session.session_id = None;
                session.clear_provider_session();
            }
            if clear_history {
                session.history.clear();
            }
            session
                .channel_name
                .as_ref()
                .map(|channel_name| provider.build_tmux_session_name(channel_name))
        })
    };

    if reset_provider_state
        && let Some(session_key) =
            resolve_session_key_for_clear(http, shared, channel_id, provider).await
    {
        super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
    }

    if let Some(name) = tmux_name.as_deref() {
        if reset_provider_state {
            match managed_session_reset_behavior(provider) {
                ManagedSessionResetBehavior::ResetManagedProcess => {
                    reset_managed_process_session(name);
                }
                ManagedSessionResetBehavior::Noop => {}
            }
        }
        if recreate_tmux {
            recreate_tmux_session(name, reset_source);
        }
    }

    tmux_name
}

pub(in crate::services::discord) async fn notify_turn_stop(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    stop_source: &str,
) {
    let session_key = resolve_session_key_for_clear(http, shared, channel_id, provider)
        .await
        .unwrap_or_else(|| format!("channel:{}", channel_id.get()));
    if let Some(db) = shared.db.as_ref() {
        crate::services::message_outbox::enqueue_lifecycle_notification(
            db,
            &format!("channel:{}", channel_id.get()),
            Some(&session_key),
            "lifecycle.stop_turn",
            &format!("🛑 현재 턴 중단 ({stop_source}) — tmux는 유지됩니다."),
        );
    }
}

pub(in crate::services::discord) async fn reset_provider_session_if_pending(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) {
    let legacy_session_reset_pending = shared.session_reset_pending.remove(&channel_id).is_some();
    let fast_mode_reset_pending = shared
        .fast_mode_session_reset_pending
        .remove(&channel_id)
        .is_some();
    let model_reset_pending = shared
        .model_session_reset_pending
        .remove(&channel_id)
        .is_some();

    let Some(plan) = pending_session_reset_plan(
        provider,
        fast_mode_reset_pending,
        model_reset_pending,
        legacy_session_reset_pending,
    ) else {
        return;
    };

    let _ = reset_channel_provider_state(
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
}

pub(in crate::services::discord) async fn clear_channel_session_state(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
) {
    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .map(|ch_name| provider.build_tmux_session_name(ch_name))
    };

    let cleared = mailbox_clear_channel(shared, provider, channel_id).await;
    if cleared.removed_token.is_some() {
        shared
            .global_active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            cleanup_channel_uploads(channel_id);
            session.session_id = None;
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    shared.dispatch_role_overrides.remove(&channel_id);

    shared.fast_mode_session_reset_pending.remove(&channel_id);
    shared.model_session_reset_pending.remove(&channel_id);
    shared.session_reset_pending.remove(&channel_id);

    if let Some(token) = cleared.removed_token {
        cancel_active_token(
            &token,
            super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
            clear_source,
        );
    }

    let session_key = resolve_session_key_for_clear(http, shared, channel_id, provider).await;
    if let Some(session_key) = session_key.as_deref() {
        super::super::adk_session::clear_provider_session_id(session_key, shared.api_port).await;
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
            None,
            shared.api_port,
        )
        .await;
    }

    match managed_session_clear_behavior(provider) {
        ManagedSessionClearBehavior::NativeProviderClear =>
        {
            #[cfg(unix)]
            if let Some(name) = tmux_name {
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/clear", "Enter"])
                })
                .await;
            }
        }
        ManagedSessionClearBehavior::ResetManagedProcess => {
            if let Some(name) = tmux_name {
                reset_managed_process_session(&name);
            }
        }
        ManagedSessionClearBehavior::Noop => {}
    }

    // Notify bot message for session clear visibility
    if let Some(db) = shared.db.as_ref() {
        crate::services::message_outbox::enqueue_lifecycle_notification(
            db,
            &format!("channel:{}", channel_id.get()),
            session_key.as_deref(),
            "lifecycle.soft_clear",
            &format!("🧹 세션 클리어 ({clear_source})"),
        );
    }
}

/// /stop — Cancel in-progress AI request
///
/// #441: flows through mailbox_cancel_active_turn → cancel_active_token
/// → token.cancelled triggers turn_bridge loop exit → mailbox_finish_turn canonical cleanup
#[poise::command(slash_command, rename = "stop")]
pub(in crate::services::discord) async fn cmd_stop(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /stop");

    let channel_id = ctx.channel_id();
    let result = mailbox_cancel_active_turn(&ctx.data().shared, channel_id).await;

    match result.token {
        Some(token) => {
            if result.already_stopping {
                ctx.say("Already stopping...").await?;
                return Ok(());
            }

            ctx.say("Stopping...").await?;

            cancel_active_token(
                &token,
                super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                "/stop",
            );
            notify_turn_stop(
                &ctx.serenity_context().http,
                &ctx.data().shared,
                &ctx.data().provider,
                channel_id,
                "/stop",
            )
            .await;
            tracing::info!("  [{ts}] ■ Cancel signal sent");
        }
        None => {
            ctx.say("No active request to stop.").await?;
        }
    }
    Ok(())
}

/// /clear — Clear AI conversation history
#[poise::command(slash_command, rename = "clear")]
pub(in crate::services::discord) async fn cmd_clear(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /clear");

    let http = ctx.serenity_context().http.clone();
    clear_channel_session_state(
        &http,
        &ctx.data().shared,
        &ctx.data().provider,
        ctx.channel_id(),
        "/clear",
    )
    .await;

    ctx.say("Session cleared.").await?;
    tracing::info!("  [{ts}] ▶ [{user_name}] Session cleared");
    Ok(())
}

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

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /down {file}");

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

#[cfg(test)]
mod tests {
    use super::{
        ManagedSessionClearBehavior, ManagedSessionResetBehavior, PendingSessionResetPlan,
        build_fallback_session_key_for_clear, fallback_channel_name_for_clear,
        managed_session_clear_behavior, managed_session_reset_behavior, pending_session_reset_plan,
    };
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;

    #[test]
    fn managed_session_clear_behavior_matches_provider_transport() {
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Claude),
            ManagedSessionClearBehavior::NativeProviderClear
        );
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Codex),
            ManagedSessionClearBehavior::ResetManagedProcess
        );
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Qwen),
            ManagedSessionClearBehavior::ResetManagedProcess
        );
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Gemini),
            ManagedSessionClearBehavior::Noop
        );
    }

    #[test]
    fn managed_session_reset_behavior_matches_provider_transport() {
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Claude),
            ManagedSessionResetBehavior::Noop
        );
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Codex),
            ManagedSessionResetBehavior::ResetManagedProcess
        );
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Qwen),
            ManagedSessionResetBehavior::ResetManagedProcess
        );
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Gemini),
            ManagedSessionResetBehavior::Noop
        );
    }

    #[test]
    fn pending_session_reset_plan_recreates_claude_tmux_for_fast_mode() {
        assert_eq!(
            pending_session_reset_plan(&ProviderKind::Claude, true, false, false),
            Some(PendingSessionResetPlan {
                reset_source: "fast mode reset pending",
                recreate_tmux: true,
            })
        );
        assert_eq!(
            pending_session_reset_plan(&ProviderKind::Codex, true, false, false),
            Some(PendingSessionResetPlan {
                reset_source: "fast mode reset pending",
                recreate_tmux: false,
            })
        );
    }

    #[test]
    fn pending_session_reset_plan_prefers_specific_flags_and_keeps_legacy_fallback() {
        assert_eq!(
            pending_session_reset_plan(&ProviderKind::Claude, false, true, true),
            Some(PendingSessionResetPlan {
                reset_source: "model session reset pending",
                recreate_tmux: false,
            })
        );
        assert_eq!(
            pending_session_reset_plan(&ProviderKind::Gemini, false, false, true),
            Some(PendingSessionResetPlan {
                reset_source: "session reset pending",
                recreate_tmux: false,
            })
        );
        assert_eq!(
            pending_session_reset_plan(&ProviderKind::Gemini, false, false, false),
            None
        );
    }

    #[test]
    fn fallback_channel_name_for_clear_uses_synthetic_thread_name() {
        let channel_id = ChannelId::new(12345);
        let channel_name = fallback_channel_name_for_clear(
            Some("thread-title"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(channel_name.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn fallback_channel_name_for_clear_uses_parent_id_when_name_missing() {
        let channel_id = ChannelId::new(12345);
        let channel_name =
            fallback_channel_name_for_clear(None, Some((ChannelId::new(777), None)), channel_id);

        assert_eq!(channel_name.as_deref(), Some("777-t12345"));
    }

    #[test]
    fn fallback_channel_name_for_clear_uses_live_name_for_non_threads() {
        let channel_id = ChannelId::new(12345);
        let channel_name =
            fallback_channel_name_for_clear(Some("agentdesk-qwen"), None, channel_id);

        assert_eq!(channel_name.as_deref(), Some("agentdesk-qwen"));
    }

    #[test]
    fn fallback_clear_key_uses_namespaced_session_key() {
        let key = build_fallback_session_key_for_clear("tokenxyz", &ProviderKind::Codex, "adk-cdx");
        let expected_tmux = ProviderKind::Codex.build_tmux_session_name("adk-cdx");
        let expected = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz",
            &ProviderKind::Codex,
            &expected_tmux,
        );
        assert_eq!(key, expected);
    }
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

    let ts = chrono::Local::now().format("%H:%M:%S");
    let preview = truncate_str(&command, 60);
    tracing::info!("  [{ts}] ◀ [{user_name}] /shell {preview}");

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
                parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
            }
            if parts.is_empty() {
                parts.push(format!("(exit code: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(exit code: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => format!("Failed to execute: {}", e),
        Err(e) => format!("Task error: {}", e),
    };

    send_long_message_ctx(ctx, &response).await?;
    tracing::info!("  [{ts}] ▶ [{user_name}] Shell done");
    Ok(())
}
