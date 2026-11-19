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
                // The fresh normalization's own reason when it rejected this turn's output.
                let fresh = row.turn_nonce.as_deref().and_then(|nonce| {
                    crate::services::tui_o::cutover::codex_first_rejection(channel_id, nonce)
                });
                let reason = fresh.unwrap_or(Reason::AdoptionRefused);
                let closure = closure::of(provider, row).await;
                if closure.is_none() && matches!(provider, ProviderKind::Codex) {
                    tracing::warn!(
                        channel_id,
                        "refused body's turn has no raw position to close"
                    );
                }
                let persist = disposition::closure::persist(&pool, &id, reason, closure.as_ref());
                persist.await.map(|()| (id, reason))
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

/// Closes the uncovered turns a restarted takeover finds before a closed one with their own
/// reason: intent and closure commit together, then the notice posts once. The key names the
/// rollout range, so a repeated attempt reuses it.
pub(in crate::services::discord) async fn close_unplaced(
    pool: &PgPool,
    http: Option<Arc<serenity::Http>>,
    channel: u64,
    closed: &disposition::closure::Closure,
) -> Result<(), String> {
    let range = (closed.from, closed.until.unwrap_or(closed.from));
    let (session, file) = (&closed.session_id, (closed.dev, closed.ino));
    let id = Identity {
        channel,
        input: 0,
        nonce: format!("{session}:{}:{}:{}:{}", file.0, file.1, range.0, range.1),
        execution: "codex".into(),
        source: "unplaced:v1".into(),
    };
    disposition::closure::persist(pool, &id, Reason::Unplaced, Some(closed)).await?;
    if let Some(http) = http {
        let (pool, intent) = (pool.clone(), (id, Reason::Unplaced));
        let notice = async move { post(&http, &pool, &[intent]).await };
        crate::services::discord::task_supervisor::spawn_observed("tui_rejection_notice", notice);
    }
    Ok(())
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
) -> Vec<(Identity, Reason)> {
    let mut discarded = Vec::new();
    for snapshot in inflight::herdr_body_debt::retained(provider, channel) {
        let Some(id) = identity(provider, &snapshot.row) else {
            tracing::warn!(
                channel_id = channel,
                "refused body snapshot has no turn nonce and stays"
            );
            continue;
        };
        let committed = discard_episode(pool, provider.as_str(), channel, &snapshot.episode, &id);
        let removed = match committed.await {
            Ok(()) => snapshot.remove(),
            Err(error) => Err(error),
        };
        match removed {
            Ok(()) => discarded.push((id, Reason::AdoptionRefused)),
            Err(error) => {
                tracing::warn!(channel_id = channel, %error, "refused body snapshot stays")
            }
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

/// Boot: discards the refused channels' snapshots, then posts every intent no attempt holds;
/// the row, body and debt may be gone, so the intent alone drives its first post.
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

#[path = "refused_discard_closure.rs"]
mod closure;

#[cfg(test)]
#[path = "refused_discard_tests.rs"]
mod tests;
