mod native_stop;
use poise::serenity_prelude as serenity;
use std::sync::Arc;

use super::super::router::LocalAdmissionPermit;
use super::super::turn_bridge::CommandStop;
use super::super::*;
use super::command_policy::{TextCommand, TextCommandMatch, match_text_command};

#[cfg(test)]
#[path = "text_commands/clear_refusal_reply_tests.rs"]
mod clear_refusal_reply_tests;

/// A text stop judges the turn, by its inflight row when the token has no name, before any write.
async fn cancel_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    reason: &str,
) -> CommandStop {
    super::super::turn_bridge::begin_user_stop(shared, provider, channel_id, true, reason).await
}

/// After `!stop`, kicks the deferred idle-queue drain so input preserved across the cancel
/// starts without waiting for the next message, as the REST cancel path does.
pub(super) fn schedule_text_stop_pending_queue_drain(
    shared: &Arc<SharedData>,
    provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    stop_source: &'static str,
) {
    let shared = shared.clone();
    let provider = provider.clone();
    tokio::spawn(async move {
        if let Some(registry) = shared.health_registry() {
            let _ = super::super::health::schedule_pending_queue_drain_after_cancel(
                registry.as_ref(),
                provider.as_str(),
                channel_id,
                stop_source,
            )
            .await;
            return;
        }

        let snapshot = shared
            .mailbox(channel_id)
            .snapshot()
            .await
            .intervention_queue
            .len();
        if snapshot == 0 {
            return;
        }
        super::super::schedule_deferred_idle_queue_kickoff(
            shared,
            provider,
            channel_id,
            stop_source,
        );
    });
}

/// Runs `!clear`, `!restart` and `!stop` and answers a removed command name with its
/// replacement. `Ok(false)` means the text is not a command and is ordinary chat.
pub(in crate::services::discord) async fn handle_text_command_with_uploads(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
    _preloaded_uploads: &[crate::services::cluster::attachment_transfer::uploads::Upload],
    _admitted_attachment_permit: &mut Option<LocalAdmissionPermit>,
) -> Result<bool, Error> {
    #[cfg(unix)]
    if super::super::router::herdr_consent::retry(ctx, msg, data, channel_id, text).await? {
        return Ok(true);
    }
    let command = match match_text_command(text) {
        Some(TextCommandMatch::Kept(command)) => command,
        Some(TextCommandMatch::Retired(name, replacement)) => {
            tracing::info!(
                event = "discord_text_command_retired",
                channel_id = channel_id.get(),
                user_id = msg.author.id.get(),
                command = name,
                "discord_text_command_retired"
            );
            let reply = match replacement {
                Some(slash) => {
                    format!("`{name}` 텍스트 명령은 없어졌어요. `{slash}` 커맨드를 쓰세요.")
                }
                None => format!("`{name}` 텍스트 명령은 없어졌어요."),
            };
            let _ = msg.reply(&ctx.http, reply).await;
            return Ok(true);
        }
        Some(TextCommandMatch::ConsentRetry) | None => return Ok(false),
    };

    match command {
        TextCommand::Stop => native_stop::handle(ctx, msg, data, channel_id).await,
        TextCommand::Clear => {
            let cleared = async {
                let report = super::clear_channel_session_state(
                    &ctx.http,
                    &data.shared,
                    &data.provider,
                    channel_id,
                    "!clear",
                )
                .await?;
                let _ = msg.reply(&ctx.http, report.excluding(msg.id).reply()).await;
                anyhow::Ok(())
            };
            // A failed clear answers with its reason, as `/clear` does: an error returned from
            // this handler reaches only the event-handler log, never the channel.
            if let Err(error) = cleared.await {
                tracing::warn!(
                    event = "discord_command_failed",
                    channel_id = channel_id.get(),
                    user_id = msg.author.id.get(),
                    user_name = %msg.author.name,
                    command = "!clear",
                    reason = %error,
                    "discord_command_failed"
                );
                let _ = msg.reply(&ctx.http, error.to_string()).await;
            }
            Ok(true)
        }
        TextCommand::Restart => {
            super::restart::run_text_restart(ctx, msg, data, channel_id).await?;
            Ok(true)
        }
    }
}
