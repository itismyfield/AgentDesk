//! Immutable admission provenance and an owner lock held by the actual output request.
use super::{OwnerIdentity, custody};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::future::Future;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub(crate) struct Origin {
    pub(super) delivery: i64,
    pub(super) provider: String,
    pub(super) channel: String,
    pub(super) executor: String,
    pub(super) generation: i64,
    pub(super) receipt: i64,
    pub(super) nonce: String,
    pub(super) incarnation: String,
    pub(super) binding: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    origin: Origin,
    payload_hash: String,
}

pub(crate) fn hash(payload: &str) -> String {
    hex::encode(Sha256::digest(payload.as_bytes()))
}

impl Origin {
    /// Called when the output artifact is created, never by a recovery sender.
    pub(crate) fn seal(&self, payload: &str) -> Artifact {
        Artifact {
            origin: self.clone(),
            payload_hash: hash(payload),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Scope {
    pub(crate) pool: PgPool,
    pub(crate) provider: String,
}

pub(crate) struct EffectGuard(Option<Transaction<'static, Postgres>>);

impl Scope {
    pub(crate) async fn acquire(
        &self,
        channel: &str,
        payload: &str,
        artifact: Option<&Artifact>,
    ) -> Result<EffectGuard, String> {
        let id = OwnerIdentity::new(&self.provider, channel);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("SET LOCAL idle_in_transaction_session_timeout='65s'")
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        custody::lock(&mut tx, &id)
            .await
            .map_err(|e| e.to_string())?;
        match artifact {
            Some(artifact) => {
                let o = &artifact.origin;
                if o.provider != id.provider()
                    || o.channel != id.raw_channel_id()
                    || artifact.payload_hash != hash(payload)
                    || !o.binding.is_object()
                {
                    return Err("output origin scope or payload changed".into());
                }
                let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id JOIN intake_session_owners o ON o.provider=d.provider AND o.raw_channel_id=d.channel_id AND o.owner_instance_id=d.owner_instance_id AND o.generation=d.owner_generation AND o.status='active' WHERE d.id=$1 AND d.provider=$2 AND d.channel_id=$3 AND d.routing_state='admitted' AND d.owner_instance_id=$4 AND d.owner_generation=$5 AND r.id=$6 AND r.replay_episode_nonce=$7 AND r.replay_owner_incarnation=$8 AND r.replay_disposition IN ('started_unclassified','classified_normal') AND r.replay_preserved #> '{projection,binding}'=$9::jsonb)")
                    .bind(o.delivery).bind(&o.provider).bind(&o.channel).bind(&o.executor)
                    .bind(o.generation).bind(o.receipt).bind(&o.nonce).bind(&o.incarnation)
                    .bind(&o.binding).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
                if !valid {
                    return Err("output owner or original source revoked".into());
                }
            }
            None => {
                let managed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_receipt_id IS NOT NULL)")
                    .bind(id.provider()).bind(id.raw_channel_id()).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
                if managed {
                    return Err("managed output has no origin stamp".into());
                }
            }
        }
        Ok(EffectGuard(Some(tx)))
    }
}

impl Artifact {
    pub(crate) fn provider(&self) -> &str {
        &self.origin.provider
    }
}

pub(crate) async fn enqueue_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    message: crate::services::message_outbox::OutboxMessage<'_>,
    origin: &Artifact,
) -> Result<i64, crate::services::message_outbox::OutboxEnqueueError> {
    crate::services::message_outbox::enqueue_outbox_with_origin_on_tx(
        tx,
        message,
        None,
        Some(origin),
    )
    .await
}

impl EffectGuard {
    /// Move this future, including its guard, into any task that owns the raw request.
    pub(crate) async fn run<T>(mut self, request: impl Future<Output = T>) -> Result<T, String> {
        if self.0.is_none() {
            return Ok(request.await);
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(60), request)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(tx) = self.0.take()
            && let Err(error) = tx.rollback().await
        {
            tracing::warn!(%error,"output ACK retained after guard release failed");
        }
        Ok(result)
    }
}

pub(crate) async fn acquire(
    scope: Option<&Scope>,
    channel: &str,
    payload: &str,
    artifact: Option<&Artifact>,
) -> Result<EffectGuard, String> {
    match scope {
        Some(scope) => scope.acquire(channel, payload, artifact).await,
        None if artifact.is_some() => Err("stamped output has no owner PG scope".into()),
        None => Ok(EffectGuard(None)),
    }
}

/// Restores the immutable admission, not the channel's current owner.
pub(crate) async fn source_artifact(
    pool: Option<&PgPool>,
    provider: &str,
    channel: &str,
    message: &str,
    delivery: Option<i64>,
    payload: &str,
) -> Result<Option<Artifact>, String> {
    let Some(pool) = pool else {
        return if delivery.is_some() {
            Err("original output PG unavailable".into())
        } else {
            Ok(None)
        };
    };
    let rows: Vec<Origin> = sqlx::query_as(
        "SELECT d.id AS delivery,d.provider,d.channel_id AS channel,d.owner_instance_id AS executor,d.owner_generation AS generation,r.id AS receipt,r.replay_episode_nonce AS nonce,r.replay_owner_incarnation AS incarnation,r.replay_preserved #> '{projection,binding}' AS binding FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.provider=$1 AND d.channel_id=$2 AND (($3::bigint IS NOT NULL AND d.id=$3) OR ($3 IS NULL AND (d.user_msg_id=$4 OR $4=ANY(r.replay_source_message_ids)))) AND r.replay_disposition IN ('started_unclassified','classified_normal','withheld')")
        .bind(provider).bind(channel).bind(delivery).bind(message).fetch_all(pool).await.map_err(|e|e.to_string())?;
    if rows.len() > 1 {
        return Err("ambiguous original output admission".into());
    }
    Ok(rows.into_iter().next().map(|origin| origin.seal(payload)))
}

pub(crate) async fn reject_unstamped_managed(pool: &PgPool, channel: &str) -> Result<(), String> {
    let managed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE channel_id=$1 AND routing_receipt_id IS NOT NULL)")
        .bind(channel).fetch_one(pool).await.map_err(|e| e.to_string())?;
    if managed {
        Err("managed output has no origin stamp".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "output_tests.rs"]
pub(crate) mod tests;
