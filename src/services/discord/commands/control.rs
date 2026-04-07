use poise::serenity_prelude as serenity;
use serenity::CreateAttachment;
use std::path::Path;
use std::sync::Arc;

use crate::services::provider::ProviderKind;
use crate::services::provider::cancel_requested;

use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::settings::cleanup_channel_uploads;
use super::super::turn_bridge::cancel_active_token;
use super::super::{Context, Error, SharedData, check_auth};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionClearBehavior {
    NativeProviderClear,
    TerminateManagedSession,
    Noop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionResetBehavior {
    TerminateManagedSession,
    Noop,
}

fn managed_session_clear_behavior(provider: &ProviderKind) -> ManagedSessionClearBehavior {
    match provider {
        ProviderKind::Claude => ManagedSessionClearBehavior::NativeProviderClear,
        ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionClearBehavior::TerminateManagedSession
        }
        ProviderKind::Gemini | ProviderKind::Unsupported(_) => ManagedSessionClearBehavior::Noop,
    }
}

fn managed_session_reset_behavior(provider: &ProviderKind) -> ManagedSessionResetBehavior {
    if provider.uses_managed_tmux_backend() {
        ManagedSessionResetBehavior::TerminateManagedSession
    } else {
        ManagedSessionResetBehavior::Noop
    }
}

async fn resolve_session_key_for_clear(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    if let Some(key) =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider).await
    {
        return Some(key);
    }

    let channel_name =
        channel_id
            .to_channel(http)
            .await
            .ok()
            .and_then(|channel| match channel {
                serenity::Channel::Guild(guild_channel) => Some(guild_channel.name),
                _ => None,
            })?;
    let hostname = crate::services::platform::hostname_short();
    Some(format!(
        "{}:{}",
        hostname,
        provider.build_tmux_session_name(&channel_name)
    ))
}

pub(in crate::services::discord) async fn reset_provider_session_if_pending(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) {
    if shared
        .model_session_reset_pending
        .remove(&channel_id)
        .is_none()
    {
        return;
    }

    let tmux_name = {
        let mut data = shared.core.lock().await;
        data.sessions.get_mut(&channel_id).and_then(|session| {
            session.session_id = None;
            session
                .channel_name
                .as_ref()
                .map(|channel_name| provider.build_tmux_session_name(channel_name))
        })
    };

    if let Some(session_key) =
        resolve_session_key_for_clear(http, shared, channel_id, provider).await
    {
        super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
    }

    match managed_session_reset_behavior(provider) {
        ManagedSessionResetBehavior::TerminateManagedSession => {
            if let Some(name) = tmux_name {
                crate::services::claude::terminate_local_session(&name);
            }
        }
        ManagedSessionResetBehavior::Noop => {}
    }
}

pub(in crate::services::discord) async fn clear_channel_session_state(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
) {
    let (cancel_token, tmux_name) = {
        let mut data = shared.core.lock().await;
        let cancel_token = data.cancel_tokens.remove(&channel_id);
        if cancel_token.is_some() {
            shared
                .global_active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }

        let tmux_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .map(|ch_name| provider.build_tmux_session_name(ch_name));

        if let Some(session) = data.sessions.get_mut(&channel_id) {
            cleanup_channel_uploads(channel_id);
            session.session_id = None;
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }

        data.active_request_owner.remove(&channel_id);
        data.intervention_queue.remove(&channel_id);
        (cancel_token, tmux_name)
    };

    shared.model_session_reset_pending.remove(&channel_id);

    if let Some(token) = cancel_token {
        cancel_active_token(&token, true, clear_source);
    }

    if let Some(session_key) =
        resolve_session_key_for_clear(http, shared, channel_id, provider).await
    {
        super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
        super::super::adk_session::post_adk_session_status(
            Some(session_key.as_str()),
            None,
            None,
            "idle",
            provider,
            None,
            Some(0),
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
        ManagedSessionClearBehavior::TerminateManagedSession => {
            if let Some(name) = tmux_name {
                crate::services::claude::terminate_local_session(&name);
            }
        }
        ManagedSessionClearBehavior::Noop => {}
    }
}

/// /stop — Cancel in-progress AI request
#[poise::command(slash_command, rename = "stop")]
pub(in crate::services::discord) async fn cmd_stop(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /stop");

    let channel_id = ctx.channel_id();
    let token = {
        let data = ctx.data().shared.core.lock().await;
        data.cancel_tokens.get(&channel_id).cloned()
    };

    match token {
        Some(token) => {
            if cancel_requested(Some(token.as_ref())) {
                ctx.say("Already stopping...").await?;
                return Ok(());
            }

            ctx.say("Stopping...").await?;

            cancel_active_token(&token, true, "/stop");
            println!("  [{ts}] ■ Cancel signal sent");
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
    println!("  [{ts}] ◀ [{user_name}] /clear");

    clear_channel_session_state(
        ctx.http(),
        &ctx.data().shared,
        &ctx.data().provider,
        ctx.channel_id(),
        "/clear",
    )
    .await;

    ctx.say("Session cleared.").await?;
    println!("  [{ts}] ▶ [{user_name}] Session cleared");
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
    println!("  [{ts}] ◀ [{user_name}] /down {file}");

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
        ManagedSessionClearBehavior, ManagedSessionResetBehavior, managed_session_clear_behavior,
        managed_session_reset_behavior,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn managed_session_clear_behavior_matches_provider_transport() {
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Claude),
            ManagedSessionClearBehavior::NativeProviderClear
        );
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Codex),
            ManagedSessionClearBehavior::TerminateManagedSession
        );
        assert_eq!(
            managed_session_clear_behavior(&ProviderKind::Qwen),
            ManagedSessionClearBehavior::TerminateManagedSession
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
            ManagedSessionResetBehavior::TerminateManagedSession
        );
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Codex),
            ManagedSessionResetBehavior::TerminateManagedSession
        );
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Qwen),
            ManagedSessionResetBehavior::TerminateManagedSession
        );
        assert_eq!(
            managed_session_reset_behavior(&ProviderKind::Gemini),
            ManagedSessionResetBehavior::Noop
        );
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
    println!("  [{ts}] ◀ [{user_name}] /shell {preview}");

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
    println!("  [{ts}] ▶ [{user_name}] Shell done");
    Ok(())
}
