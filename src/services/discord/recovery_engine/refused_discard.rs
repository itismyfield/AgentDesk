//! A refused adoption's body is discarded with one durable, body-free reason: the intent is
//! persisted first, then the held body goes, then the reason is posted at most once.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use super::*;
use crate::db::tui_output_disposition::{self as disposition, Identity, Reason};
use crate::services::discord::outbound::tui_rejection_notice::{self as notice, GatewayNoticePort};

/// The canonical identity of `row`'s body, or none when it has no turn nonce.
pub(in crate::services::discord) fn identity(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Option<Identity> {
    let key = crate::services::tui_o::channel_policy::episode_key::identity;
    key(
        provider.as_str(),
        row.channel_id,
        row.turn_nonce.as_deref()?,
    )
}

/// The first reason `row`'s episode already holds, as an intent or a receipt. It closes the
/// episode: the body is discarded reusing it and never copied, whatever the adoption reads now.
pub(in crate::services::discord) async fn closed(
    pool: &PgPool,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Result<Option<(Identity, Reason)>, String> {
    let Some(id) = identity(provider, row) else {
        return Ok(None);
    };
    let stored = disposition::record(pool, &id).await?;
    Ok(stored.map(|(reason, _)| (id, reason)))
}

/// A row with no turn nonce has no canonical key: its own kind-tagged one, from its anchor
/// message and start, is the one place a second reason remains possible.
fn row_key(provider: &ProviderKind, row: &inflight::InflightTurnState) -> Option<Identity> {
    let digest = crate::services::tui_o::channel_policy::first_prompt_digest;
    (row.channel_id != 0 && !row.started_at.trim().is_empty()).then(|| Identity {
        channel: row.channel_id,
        input: row.user_msg_id,
        nonce: digest(&row.started_at),
        execution: provider.as_str().to_ascii_lowercase(),
        source: "row:v1".into(),
    })
}

/// Discards a held body with one reason: a refused adoption's, or a waiting one's whose episode
/// already holds a reason. True once the intent is durable; only then may the caller release it.
pub(in crate::services::discord) async fn discard_held(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    held: crate::services::tui_o::cutover::IdentityError,
) -> bool {
    use crate::services::tui_o::cutover::IdentityError::{AdoptionRefused, AwaitingAdoption};
    let (true, Some(pool)) = (
        matches!(held, AdoptionRefused | AwaitingAdoption),
        shared.pg_pool.clone(),
    ) else {
        return false;
    };
    let channel_id = row.channel_id;
    let intent = match (closed(&pool, provider, row).await, held) {
        (Ok(Some(first)), _) => Ok(first),
        (Ok(None), AdoptionRefused) => match identity(provider, row).or(row_key(provider, row)) {
            Some(id) => {
                let reason = Reason::AdoptionRefused;
                let persisted = disposition::persist_intent(&pool, &id, reason).await;
                persisted.map(|()| (id, reason))
            }
            None => Err("the refused body has no key".into()),
        },
        (Ok(None), _) => return false,
        (Err(error), _) => Err(error),
    };
    let intent = match intent {
        Ok(intent) => intent,
        Err(error) => {
            tracing::warn!(channel_id, %error, "refused body stays: its reason is not durable");
            return false;
        }
    };
    // The durable intent drives the post on its own; one lost here is posted on the next start.
    if let Some(http) = shared.serenity_http_or_token_fallback() {
        let notice = async move { post(&http, &pool, &[intent]).await };
        crate::services::discord::task_supervisor::spawn_observed("tui_rejection_notice", notice);
    }
    true
}

/// Boot: posts every intent no attempt holds; the row and body may be gone, so the intent
/// alone drives its first post.
pub(in crate::services::discord) fn resume(shared: &Arc<SharedData>) {
    let (Some(pool), Some(http)) = (
        shared.pg_pool.clone(),
        shared.serenity_http_or_token_fallback(),
    ) else {
        return;
    };
    crate::services::discord::task_supervisor::spawn_observed("tui_rejection_resume", async move {
        match disposition::unposted(&pool).await {
            Ok(intents) => post(&http, &pool, &intents).await,
            Err(error) => tracing::warn!(%error, "tui rejection intents stay for the next start"),
        }
    });
}

/// Posts each stored reason once. An intent that already holds an attempt keeps it and is not
/// posted again; only a proven no-effect frees it for the next start.
pub(in crate::services::discord) async fn post(
    http: &Arc<serenity::Http>,
    pool: &PgPool,
    intents: &[(Identity, Reason)],
) {
    if intents.is_empty() {
        return;
    }
    let bot = tokio::time::timeout(Duration::from_secs(3), http.get_current_user()).await;
    let Ok(Ok(bot)) = bot else {
        tracing::warn!("tui rejection notices wait for the next start: bot identity unread");
        return;
    };
    let port = GatewayNoticePort::new(Arc::clone(http), bot.id.get());
    for (id, reason) in intents {
        match notice::deliver(pool, id, *reason, &port).await {
            Ok(Some(receipt)) => tracing::info!(
                channel_id = id.channel,
                message = receipt.message(),
                reason = ?receipt.reason(),
                key = receipt.key(),
                template = receipt.template_version(),
                "tui rejection notice confirmed"
            ),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(channel_id = id.channel, %error, "tui rejection notice not confirmed")
            }
        }
    }
}

#[cfg(test)]
#[path = "refused_discard_tests.rs"]
mod tests;
