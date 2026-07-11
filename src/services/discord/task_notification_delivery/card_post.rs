//! Durable network-boundary handling for task-card POSTs.

use sqlx::PgPool;

use super::{
    CardBot, CardEnsureError, CardPostReconcile, ClaimedCard, TaskCardTransport,
    map_transport_error, store,
};

const CARD_NONCE_RETRY_TTL_SECONDS: i64 = 120;
const CARD_NONCE_CLOCK_SKEW_SECONDS: i64 = 5;

fn nonce_retry_allowed(
    started_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    now >= started_at - chrono::Duration::seconds(CARD_NONCE_CLOCK_SKEW_SECONDS)
        && now - started_at < chrono::Duration::seconds(CARD_NONCE_RETRY_TTL_SECONDS)
}

pub(super) async fn deliver_card_post_claim<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    transport: &T,
    bot: &CardBot,
    claimed: &ClaimedCard,
    content: &str,
    hash: &str,
) -> Result<u64, CardEnsureError> {
    let attempt = store::begin_card_post(pool, claimed)
        .await
        .map_err(CardEnsureError::Store)?;
    if attempt.resumed && !nonce_retry_allowed(attempt.started_at, chrono::Utc::now()) {
        match transport
            .reconcile_card_post(
                bot,
                claimed.scope.channel_id,
                &claimed.discord_nonce,
                hash,
                attempt.started_at,
            )
            .await
        {
            CardPostReconcile::Found(message_id) => {
                store::mark_posted(pool, claimed, message_id, content, hash)
                    .await
                    .map_err(CardEnsureError::Store)?;
                return Ok(message_id);
            }
            CardPostReconcile::Ambiguous(reason)
            | CardPostReconcile::Transient(reason)
            | CardPostReconcile::Permanent(reason) => {
                store::mark_card_post_ambiguous(pool, claimed, &reason)
                    .await
                    .map_err(CardEnsureError::Store)?;
                crate::services::observability::record_invariant_check(
                    false,
                    crate::services::observability::InvariantViolation {
                        provider: Some(claimed.scope.provider.as_str()),
                        channel_id: Some(claimed.scope.channel_id),
                        dispatch_id: None,
                        session_key: Some(claimed.scope.session_key.as_str()),
                        turn_id: Some(claimed.scope.event_key.as_str()),
                        invariant: "task_card_post_delivery_ambiguous",
                        code_location: "src/services/discord/task_notification_delivery/card_post.rs:deliver_card_post_claim",
                        message: "task card crossed the POST boundary but cannot be reconciled without duplicate risk",
                        details: serde_json::json!({
                            "revision": claimed.revision,
                            "nonce": claimed.discord_nonce,
                            "post_started_at": attempt.started_at,
                            "reason": reason,
                        }),
                    },
                );
                tracing::error!(
                    channel_id = claimed.scope.channel_id,
                    provider = %claimed.scope.provider,
                    session_key = %claimed.scope.session_key,
                    event_key = %claimed.scope.event_key,
                    revision = claimed.revision,
                    error = %reason,
                    "task card POST is outside the nonce window and ambiguous; quarantined without repost"
                );
                return Err(CardEnsureError::Ambiguous(reason));
            }
        }
    }
    match transport
        .post_card(
            bot,
            claimed.scope.channel_id,
            content,
            &claimed.discord_nonce,
        )
        .await
    {
        Ok(message_id) => {
            store::mark_posted(pool, claimed, message_id, content, hash)
                .await
                .map_err(CardEnsureError::Store)?;
            Ok(message_id)
        }
        Err(error) => {
            store::mark_post_failure(pool, claimed, &error.to_string())
                .await
                .map_err(CardEnsureError::Store)?;
            Err(map_transport_error(error))
        }
    }
}
