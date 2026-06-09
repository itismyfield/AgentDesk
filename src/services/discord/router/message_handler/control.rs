use super::*;

pub(in crate::services::discord::router) async fn handle_shell_command_raw(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();
    if cmd_str.is_empty() {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Usage: `!<command>`\nExample: `!ls -la`")
            .await;
        return Ok(());
    }

    let working_dir = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = cmd_str.to_string();
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

    send_long_message_raw(&ctx.http, channel_id, &response, shared).await?;
    Ok(())
}

pub(in crate::services::discord::router) enum TextStopLookup {
    NoActiveTurn,
    AlreadyStopping,
    Stop(Arc<CancelToken>),
}

pub(in crate::services::discord::router) async fn cancel_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    let result = super::super::super::mailbox_cancel_active_turn(shared, channel_id).await;
    match result.token {
        Some(_) if result.already_stopping => TextStopLookup::AlreadyStopping,
        Some(token) => {
            super::super::super::ensure_cancel_token_bound_from_inflight(
                provider,
                channel_id,
                &token,
                "text stop mailbox lookup",
            );
            TextStopLookup::Stop(token)
        }
        None => TextStopLookup::NoActiveTurn,
    }
}

/// #2044 F1: identity-checked variant — cancels active turn ONLY if the
/// current mailbox cancel-token is the same `Arc` as `expected_token`.
///
/// Required by the reaction-remove path: between the mailbox snapshot
/// and the cancel await, the mailbox actor can finish the old turn and
/// start a new one for a queued message, which would otherwise be
/// cancelled here (a stale ⏳-remove cancelling an unrelated follow-up
/// turn). The mailbox's `CancelActiveTurnIfCurrent` does a pointer-eq
/// check, so token identity prevents the wrong turn from being killed.
pub(in crate::services::discord::router) async fn cancel_text_stop_token_mailbox_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    expected_token: Arc<CancelToken>,
    reason: &'static str,
) -> TextStopLookup {
    let result = super::super::super::mailbox_cancel_active_turn_if_current_with_reason(
        shared,
        channel_id,
        expected_token,
        reason,
    )
    .await;
    match result.token {
        Some(_) if result.already_stopping => TextStopLookup::AlreadyStopping,
        Some(token) => {
            super::super::super::ensure_cancel_token_bound_from_inflight(
                provider,
                channel_id,
                &token,
                "text stop mailbox lookup (if_current)",
            );
            TextStopLookup::Stop(token)
        }
        None => TextStopLookup::NoActiveTurn,
    }
}

/// Handle text-based commands (!start, !meeting, !stop, !clear, etc.).
/// Returns true if the command was handled, false otherwise.
pub(in crate::services::discord::router) async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    super::super::super::commands::handle_text_command(ctx, msg, data, channel_id, text).await
}
