//! A refused adoption's body is discarded with one durable, body-free reason: the intent is
//! persisted first, then the row, snapshot or debt goes, then the reason is posted at most once.

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

/// Persists a refused row's reason intent; nothing of its body may go before this succeeds.
pub(in crate::services::discord) async fn persist(
    pool: &PgPool,
    id: &Identity,
) -> Result<(), String> {
    disposition::persist_intent(pool, id, Reason::AdoptionRefused).await
}

/// Discards a refused channel's retained snapshots, each only after its intent and debt deletion
/// committed together; a snapshot whose debt is still in flight stays.
pub(in crate::services::discord) async fn discard_retained(
    pool: &PgPool,
    provider: &ProviderKind,
    channel: u64,
) -> Vec<Identity> {
    let mut discarded = Vec::new();
    for snapshot in inflight::herdr_body_debt::retained(provider, channel) {
        let Some(id) = identity(provider, &snapshot.row) else {
            tracing::warn!(channel, "refused body snapshot has no turn nonce and stays");
            continue;
        };
        let committed = discard_episode(pool, provider.as_str(), channel, &snapshot.episode, &id);
        let removed = match committed.await {
            Ok(()) => snapshot.remove(),
            Err(error) => Err(error),
        };
        match removed {
            Ok(()) => discarded.push(id),
            Err(error) => tracing::warn!(channel, %error, "refused body snapshot stays"),
        }
    }
    discarded
}

/// One custody-locked transaction: the intent, then the episode's debt in foreign-key order. An
/// effect that is not `ready` or has an attempt other than `no_effect` keeps the whole episode.
pub(in crate::services::discord) async fn discard_episode(
    pool: &PgPool,
    provider: &str,
    channel: u64,
    episode: &str,
    id: &Identity,
) -> Result<(), String> {
    let error = |e: sqlx::Error| e.to_string();
    let channel = channel.to_string();
    let mut tx = pool.begin().await.map_err(error)?;
    let owner = crate::domain::intake_identity::OwnerIdentity::new(provider, &channel);
    crate::db::channel_custody_lock::lock(&mut tx, &owner)
        .await
        .map_err(error)?;
    disposition::persist_intent_in(&mut tx, id, Reason::AdoptionRefused).await?;
    let states: Vec<(String, bool)> = sqlx::query_as(
        "SELECT e.state, EXISTS(SELECT 1 FROM o_herdr_debt_attempts a WHERE a.effect_id=e.id AND a.phase<>'no_effect') FROM o_herdr_debt_effects e WHERE e.provider=$1 AND e.channel_id=$2 AND e.episode=$3 FOR UPDATE OF e")
        .bind(provider).bind(&channel).bind(episode).fetch_all(&mut *tx).await.map_err(error)?;
    if states.iter().any(|(state, open)| state != "ready" || *open) {
        return Err("refused body debt is reserved, confirmed or has an unsettled attempt".into());
    }
    let steps = [
        "UPDATE o_herdr_debt_effects SET attempt_id=NULL WHERE provider=$1 AND channel_id=$2 AND episode=$3",
        "DELETE FROM o_herdr_debt_attempts WHERE effect_id IN (SELECT id FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3)",
        "DELETE FROM o_herdr_debt_grants WHERE effect_id IN (SELECT id FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3)",
        "DELETE FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3",
    ];
    for step in steps {
        sqlx::query(step)
            .bind(provider)
            .bind(&channel)
            .bind(episode)
            .execute(&mut *tx)
            .await
            .map_err(error)?;
    }
    tx.commit().await.map_err(error)
}

/// Posts each refused reason once; an intent that already holds an attempt is left to
/// reconciliation, and a failed post leaves the intent for the next start.
pub(in crate::services::discord) async fn post(
    http: &Arc<serenity::Http>,
    pool: &PgPool,
    ids: &[Identity],
) {
    let intents: Vec<_> = ids
        .iter()
        .map(|id| (id.clone(), Reason::AdoptionRefused))
        .collect();
    post_intents(http, pool, &intents).await;
}

/// Boot: discards the refused channels' retained snapshots, then resumes every persisted intent
/// that no attempt holds; the row, body and debt may already be gone, so the intent alone drives
/// its first post.
pub(in crate::services::discord) fn resume(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    refused: Vec<u64>,
) {
    let (Some(pool), Some(http)) = (
        shared.pg_pool.clone(),
        shared.serenity_http_or_token_fallback(),
    ) else {
        return;
    };
    let provider = provider.clone();
    crate::services::discord::task_supervisor::spawn_observed("tui_rejection_resume", async move {
        for channel in refused {
            discard_retained(&pool, &provider, channel).await;
        }
        match disposition::unposted(&pool).await {
            Ok(intents) => post_intents(&http, &pool, &intents).await,
            Err(error) => tracing::warn!(%error, "tui rejection intents stay for the next start"),
        }
    });
}

async fn post_intents(http: &Arc<serenity::Http>, pool: &PgPool, intents: &[(Identity, Reason)]) {
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
                channel = id.channel,
                message = receipt.message(),
                reason = ?receipt.reason(),
                key = receipt.key(),
                template = receipt.template_version(),
                "tui rejection notice confirmed"
            ),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(channel = id.channel, %error, "tui rejection notice not confirmed")
            }
        }
    }
}

#[cfg(test)]
#[path = "refused_discard_tests.rs"]
mod tests;
