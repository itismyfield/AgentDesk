//! Interaction handler for the `[새 세션 시작]` button on idle-recap cards
//! (PR #3c).
//!
//! The button's `custom_id` looks like `idle_recap:clear:<message_id>`. We
//! resolve the message id back to a `session_key` via the
//! `sessions.idle_recap_message_id` index, route through the same clear path
//! as `/clear`, and delete the recap card.

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use super::{Data, Error, check_auth};
use crate::services::discord::idle_recap::{
    IDLE_RECAP_CLEAR_BUTTON_PREFIX, clear_recap_pointer, delete_previous_card,
};

struct RecapClearTarget {
    session_key: String,
    channel_matches: bool,
    provider_matches: bool,
    recap_current: bool,
}

/// True if `custom_id` belongs to the idle-recap clear button.
pub(super) fn is_idle_recap_clear_custom_id(custom_id: &str) -> bool {
    custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX)
}

pub(super) async fn handle_idle_recap_clear_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    // Authorise the click. Without this, anyone who can see the recap
    // card (= anyone with read access to the bound Discord channel) could
    // drop the provider session id. Reuses the same auth gate that the
    // `/clear` slash command goes through (see commands::control::clear).
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if !check_auth(user_id, user_name, &data.shared, &data.token).await {
        let _ = component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("Not authorized for this bot.")
                        .ephemeral(true),
                ),
            )
            .await;
        return Ok(());
    }

    let custom_id = &component.data.custom_id;
    let Some(message_id) = parse_message_id(custom_id) else {
        // Unknown / sentinel id ("0") — happens during the brief window
        // before post_recap_card rewrites the placeholder button to the
        // real id. Acknowledge so the client doesn't time out.
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };

    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        let _ = component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("세션 정리 실패: DB 연결 없음.")
                        .ephemeral(true),
                ),
            )
            .await;
        return Ok(());
    };

    let clear_target = match lookup_recap_clear_target(
        &pool,
        message_id,
        component.channel_id.get(),
        data.provider.as_str(),
    )
    .await
    {
        Ok(Some(target)) => target,
        Ok(None) => {
            // Card already cleared (compare-and-clear path won the race
            // with a fresh-cycle post) — silently acknowledge.
            let _ = component
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await;
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                message_id = message_id,
                "idle_recap clear: target lookup failed"
            );
            let _ = component
                .create_response(
                    ctx,
                    serenity::CreateInteractionResponse::Message(
                        serenity::CreateInteractionResponseMessage::new()
                            .content("세션 정리 실패. 잠시 후 다시 시도하세요.")
                            .ephemeral(true),
                    ),
                )
                .await;
            return Ok(());
        }
    };

    let _ = component
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await;

    if !clear_target.channel_matches
        || !clear_target.provider_matches
        || !clear_target.recap_current
    {
        let _ = clear_recap_pointer(&pool, &clear_target.session_key, message_id).await;
        delete_previous_card(&ctx.http, component.channel_id.get(), message_id).await;
        return Ok(());
    }

    // Compare-and-clear the recap pointer for this session, then delete
    // the card. Order matters: clear the pointer first so the
    // user-message hook (intake_gate) doesn't try to delete the same
    // message at the same time.
    let pointer_cleared = clear_recap_pointer(&pool, &clear_target.session_key, message_id)
        .await
        .unwrap_or(false);
    let channel_id = component.channel_id.get();
    if !pointer_cleared {
        delete_previous_card(&ctx.http, channel_id, message_id).await;
        return Ok(());
    }

    // Reuse `/clear` semantics, not just the provider-session-id drop. TUI
    // providers keep live tmux/process state that must be reset too.
    crate::services::discord::commands::clear_channel_session_state(
        &ctx.http,
        &data.shared,
        &data.provider,
        component.channel_id,
        "idle_recap_clear",
        crate::services::discord::commands::SoftClearNotifyMode::Enqueue,
    )
    .await;
    delete_previous_card(&ctx.http, channel_id, message_id).await;

    Ok(())
}

fn parse_message_id(custom_id: &str) -> Option<u64> {
    custom_id
        .strip_prefix(IDLE_RECAP_CLEAR_BUTTON_PREFIX)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id != 0)
}

async fn lookup_recap_clear_target(
    pool: &PgPool,
    message_id: u64,
    channel_id: u64,
    provider: &str,
) -> Result<Option<RecapClearTarget>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, bool, bool, bool)>(
        "SELECT session_key,
                idle_recap_channel_id = $2 AS channel_matches,
                provider = $3 AS provider_matches,
                COALESCE(idle_recap_posted_at >= COALESCE(last_heartbeat, created_at), false) AS recap_current
         FROM sessions
         WHERE idle_recap_message_id = $1
         LIMIT 1",
    )
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(provider)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(session_key, channel_matches, provider_matches, recap_current)| RecapClearTarget {
            session_key,
            channel_matches,
            provider_matches,
            recap_current,
        },
    ))
}
