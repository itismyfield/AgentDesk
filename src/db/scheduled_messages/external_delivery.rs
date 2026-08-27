//! Durable provider-neutral delivery handoff for scheduled push fan-out.

use std::fmt;

use chrono::{DateTime, Utc};
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use super::ScheduledMessagePatch;

// These records contain message content and raw provider targets. Keep their
// ubiquitous Debug representation metadata-only so future diagnostics cannot
// accidentally emit Kakao friend UUIDs.
impl fmt::Debug for super::ScheduledMessageRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledMessageRow")
            .field("id", &self.id)
            .field("delivery_kind", &self.delivery_kind)
            .field("status", &self.status)
            .field("scheduled_at", &self.scheduled_at)
            .field("content_chars", &self.content.chars().count())
            .field("has_discord_target", &self.target_channel_id.is_some())
            .field("has_provider_targets", &self.provider_targets.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for super::ClaimedFire {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaimedFire")
            .field("message", &self.message)
            .field("delivery_id", &self.delivery_id)
            .field("fire_scheduled_at", &self.fire_scheduled_at)
            .field("retry_count", &self.retry_count)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for super::NewScheduledMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewScheduledMessage")
            .field("delivery_kind", &self.delivery_kind)
            .field("scheduled_at", &self.scheduled_at)
            .field("content_chars", &self.content.chars().count())
            .field("has_discord_target", &self.target_channel_id.is_some())
            .field("has_provider_targets", &self.provider_targets.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for super::ScheduledMessagePatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledMessagePatch")
            .field("changes_content", &self.content.is_some())
            .field("changes_discord_target", &self.target_channel_id.is_some())
            .field("changes_provider_targets", &self.provider_targets.is_some())
            .field("changes_scheduled_at", &self.scheduled_at.is_some())
            .finish_non_exhaustive()
    }
}

pub(super) fn apply_definition_patch<'args>(
    builder: &mut QueryBuilder<'args, Postgres>,
    patch: &'args ScheduledMessagePatch,
) {
    if let Some(provider_targets) = &patch.provider_targets {
        builder
            .push(", provider_targets = ")
            .push_bind(provider_targets);
    }
    if let Some(summary) = &patch.provider_target_summary {
        builder
            .push(", provider_target_summary = ")
            .push_bind(summary);
    }
}

#[derive(Clone)]
pub struct NewExternalDelivery {
    pub id: Uuid,
    pub scheduled_delivery_id: String,
    pub provider: String,
    pub audience: String,
    pub account_id: String,
    pub payload: JsonValue,
    pub requested_count: i16,
    pub deliver_before: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, sqlx::FromRow)]
pub struct ClaimedExternalDelivery {
    pub id: Uuid,
    pub scheduled_delivery_id: String,
    pub provider: String,
    pub audience: String,
    pub account_id: String,
    pub payload: JsonValue,
    pub requested_count: i16,
    pub claim_token: Uuid,
    pub retry_count: i16,
    pub deliver_before: DateTime<Utc>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ExternalDeliveryRow {
    pub id: Uuid,
    pub scheduled_delivery_id: String,
    pub provider: String,
    pub audience: String,
    pub account_id: String,
    pub requested_count: i16,
    pub status: String,
    pub retry_count: i16,
    pub successful_count: Option<i16>,
    pub failed_count: Option<i16>,
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl ExternalDeliveryRow {
    pub fn to_api_json(&self) -> JsonValue {
        json!({
            "id": self.id.to_string(),
            "scheduledDeliveryId": self.scheduled_delivery_id,
            "provider": self.provider,
            "audience": self.audience,
            "accountId": self.account_id,
            "requestedCount": self.requested_count,
            "status": self.status,
            "retryCount": self.retry_count,
            "successfulCount": self.successful_count,
            "failedCount": self.failed_count,
            "errorCode": self.error_code,
            "createdAt": self.created_at.to_rfc3339(),
            "finishedAt": self.finished_at.map(|value| value.to_rfc3339()),
        })
    }
}

pub async fn enqueue_external_delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    delivery: &NewExternalDelivery,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO scheduled_external_delivery_outbox
            (id, scheduled_delivery_id, provider, audience, account_id, payload,
             requested_count, deliver_before, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (scheduled_delivery_id, provider, audience) DO NOTHING",
    )
    .bind(delivery.id)
    .bind(&delivery.scheduled_delivery_id)
    .bind(&delivery.provider)
    .bind(&delivery.audience)
    .bind(&delivery.account_id)
    .bind(&delivery.payload)
    .bind(delivery.requested_count)
    .bind(delivery.deliver_before)
    .bind(delivery.created_at)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn recover_external_delivery_leases_pg(
    pool: &PgPool,
    now: DateTime<Utc>,
    max_retries: i16,
) -> Result<u64, sqlx::Error> {
    let max_retries = max_retries.clamp(0, 100);
    let unknown = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET status = 'unknown', payload = NULL, claim_owner = NULL,
             claim_token = NULL, claimed_at = NULL, lease_expires_at = NULL,
             error_code = 'worker_lost_after_dispatch', finished_at = $1,
             updated_at = $1
         WHERE status = 'processing' AND lease_expires_at <= $1
           AND dispatch_started_at IS NOT NULL",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    let expired = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET status = 'failed', payload = NULL, claim_owner = NULL,
             claim_token = NULL, claimed_at = NULL, lease_expires_at = NULL,
             successful_count = 0, failed_count = requested_count,
             error_code = 'delivery_window_expired', finished_at = $1,
             updated_at = $1
         WHERE status IN ('pending', 'processing') AND deliver_before <= $1
           AND dispatch_started_at IS NULL",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    let exhausted = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET status = 'failed', payload = NULL, claim_owner = NULL,
             claim_token = NULL, claimed_at = NULL, lease_expires_at = NULL,
             successful_count = 0, failed_count = requested_count,
             error_code = 'worker_loss_retry_exhausted', finished_at = $1,
             updated_at = $1
         WHERE status = 'processing' AND lease_expires_at <= $1
           AND dispatch_started_at IS NULL AND deliver_before > $1
           AND retry_count >= $2",
    )
    .bind(now)
    .bind(max_retries)
    .execute(pool)
    .await?
    .rows_affected();
    let recovered = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET status = 'pending', claim_owner = NULL, claim_token = NULL,
             claimed_at = NULL, lease_expires_at = NULL,
             retry_count = retry_count + 1, next_attempt_at = $1,
             error_code = 'worker_lost_before_dispatch', updated_at = $1
         WHERE status = 'processing' AND lease_expires_at <= $1
           AND dispatch_started_at IS NULL AND deliver_before > $1
           AND retry_count < $2",
    )
    .bind(now)
    .bind(max_retries)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(unknown + expired + exhausted + recovered)
}

pub async fn claim_external_deliveries_pg(
    pool: &PgPool,
    claim_owner: &str,
    batch: i64,
    lease_secs: i64,
    now: DateTime<Utc>,
) -> Result<Vec<ClaimedExternalDelivery>, sqlx::Error> {
    let claim_token = Uuid::new_v4();
    sqlx::query_as::<_, ClaimedExternalDelivery>(
        "WITH candidates AS (
             SELECT id
             FROM scheduled_external_delivery_outbox
             WHERE status = 'pending' AND next_attempt_at <= $1
               AND deliver_before > $1
             ORDER BY next_attempt_at, created_at
             LIMIT $2
             FOR UPDATE SKIP LOCKED
         )
         UPDATE scheduled_external_delivery_outbox AS delivery
         SET status = 'processing', claim_owner = $3, claim_token = $4,
             claimed_at = $1,
             lease_expires_at = $1 + ($5::bigint * INTERVAL '1 second'),
             error_code = NULL, updated_at = $1
         FROM candidates
         WHERE delivery.id = candidates.id
         RETURNING delivery.id, delivery.scheduled_delivery_id,
                   delivery.provider, delivery.audience, delivery.account_id,
                   delivery.payload, delivery.requested_count,
                   delivery.claim_token, delivery.retry_count,
                   delivery.deliver_before",
    )
    .bind(now)
    .bind(batch.clamp(1, 100))
    .bind(claim_owner)
    .bind(claim_token)
    .bind(lease_secs)
    .fetch_all(pool)
    .await
}

pub async fn mark_external_dispatch_started_pg(
    pool: &PgPool,
    id: Uuid,
    claim_token: Uuid,
    lease_secs: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET dispatch_started_at = NOW(),
             lease_expires_at = NOW() + ($3::bigint * INTERVAL '1 second'),
             updated_at = NOW()
         WHERE id = $1 AND claim_token = $2 AND status = 'processing'
           AND dispatch_started_at IS NULL",
    )
    .bind(id)
    .bind(claim_token)
    .bind(lease_secs)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[allow(clippy::too_many_arguments)]
pub async fn finish_external_delivery_pg(
    pool: &PgPool,
    id: Uuid,
    claim_token: Uuid,
    status: &str,
    successful_count: Option<i16>,
    failed_count: Option<i16>,
    error_code: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET status = $3, payload = NULL, claim_owner = NULL,
             claim_token = NULL, claimed_at = NULL, lease_expires_at = NULL,
             successful_count = $4, failed_count = $5, error_code = $6,
             finished_at = NOW(), updated_at = NOW()
         WHERE id = $1 AND claim_token = $2 AND status = 'processing'",
    )
    .bind(id)
    .bind(claim_token)
    .bind(status)
    .bind(successful_count)
    .bind(failed_count)
    .bind(error_code)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn retry_external_delivery_pg(
    pool: &PgPool,
    id: Uuid,
    claim_token: Uuid,
    retry_count: i16,
    next_attempt_at: DateTime<Utc>,
    error_code: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE scheduled_external_delivery_outbox
         SET status = 'pending', claim_owner = NULL, claim_token = NULL,
             claimed_at = NULL, lease_expires_at = NULL,
             retry_count = $3, next_attempt_at = $4, error_code = $5,
             updated_at = NOW()
         WHERE id = $1 AND claim_token = $2 AND status = 'processing'
           AND dispatch_started_at IS NULL",
    )
    .bind(id)
    .bind(claim_token)
    .bind(retry_count)
    .bind(next_attempt_at)
    .bind(error_code)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn list_external_deliveries_pg(
    pool: &PgPool,
    scheduled_delivery_ids: &[String],
) -> Result<Vec<ExternalDeliveryRow>, sqlx::Error> {
    if scheduled_delivery_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ExternalDeliveryRow>(
        "SELECT id, scheduled_delivery_id, provider, audience, account_id,
                requested_count, status, retry_count, successful_count,
                failed_count, error_code, created_at, finished_at
         FROM scheduled_external_delivery_outbox
         WHERE scheduled_delivery_id = ANY($1)
         ORDER BY created_at, provider, audience",
    )
    .bind(scheduled_delivery_ids)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_shape_never_contains_raw_payload_or_friend_identifiers() {
        let private_uuid = "private-friend-uuid";
        let row = ExternalDeliveryRow {
            id: Uuid::new_v4(),
            scheduled_delivery_id: "smdel-safe".to_string(),
            provider: "kakao".to_string(),
            audience: "friends".to_string(),
            account_id: "default".to_string(),
            requested_count: 1,
            status: "pending".to_string(),
            retry_count: 0,
            successful_count: None,
            failed_count: None,
            error_code: None,
            created_at: Utc::now(),
            finished_at: None,
        };
        let rendered = row.to_api_json().to_string();
        assert!(!rendered.contains(private_uuid));
        assert!(!rendered.contains("payload"));
    }
}
