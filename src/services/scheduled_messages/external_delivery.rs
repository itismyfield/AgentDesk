//! Durable scheduled delivery to non-Discord providers.
//!
//! A row becomes `dispatch_started` immediately before provider I/O. Losing a
//! worker after that fence produces `unknown`, never an automatic replay that
//! could duplicate a user-visible message.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::scheduled_messages as db;
use crate::db::scheduled_messages::{ClaimedExternalDelivery, NewExternalDelivery};
use crate::services::kakao::{KakaoClient, KakaoDeliverySummary, KakaoError};
use crate::services::kakao_message::KakaoMessage;

use super::provider_targets::{ProviderTargetError, decode_stored};

const PROVIDER: &str = "kakao";
const FRIENDS_AUDIENCE: &str = "friends";
const SELF_AUDIENCE: &str = "self";
const CLAIM_BATCH: i64 = 10;
const CLAIM_LEASE_SECS: i64 = 60;
const MAX_RETRIES: i16 = 5;

#[derive(Debug, Error)]
pub(super) enum ExternalHandoffError {
    #[error(transparent)]
    InvalidTarget(#[from] ProviderTargetError),
    #[error("external delivery payload serialization failed")]
    Serialization,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KakaoOutboxPayload {
    message: KakaoMessage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    friend_uuids: Vec<String>,
}

pub(super) async fn enqueue_for_message_tx(
    tx: &mut Transaction<'_, Postgres>,
    message: &db::ScheduledMessageRow,
    delivery_id: &str,
    deliver_before: DateTime<Utc>,
    created_at: DateTime<Utc>,
) -> Result<usize, ExternalHandoffError> {
    let Some(raw_targets) = message.provider_targets.as_ref() else {
        return Ok(0);
    };
    let (targets, kakao_message) = decode_stored(raw_targets, &message.content)?;
    let mut deliveries = Vec::with_capacity(2);
    if !targets.kakao.friend_uuids.is_empty() {
        deliveries.push(NewExternalDelivery {
            id: stable_outbox_id(delivery_id, FRIENDS_AUDIENCE),
            scheduled_delivery_id: delivery_id.to_string(),
            provider: PROVIDER.to_string(),
            audience: FRIENDS_AUDIENCE.to_string(),
            account_id: targets.kakao.account_id.clone(),
            payload: serde_json::to_value(KakaoOutboxPayload {
                message: kakao_message.clone(),
                friend_uuids: targets.kakao.friend_uuids.clone(),
            })
            .map_err(|_| ExternalHandoffError::Serialization)?,
            requested_count: targets.kakao.friend_uuids.len() as i16,
            deliver_before,
            created_at,
        });
    }
    if targets.kakao.send_to_self {
        deliveries.push(NewExternalDelivery {
            id: stable_outbox_id(delivery_id, SELF_AUDIENCE),
            scheduled_delivery_id: delivery_id.to_string(),
            provider: PROVIDER.to_string(),
            audience: SELF_AUDIENCE.to_string(),
            account_id: targets.kakao.account_id,
            payload: serde_json::to_value(KakaoOutboxPayload {
                message: kakao_message,
                friend_uuids: Vec::new(),
            })
            .map_err(|_| ExternalHandoffError::Serialization)?,
            requested_count: 1,
            deliver_before,
            created_at,
        });
    }
    for delivery in &deliveries {
        db::enqueue_external_delivery_tx(tx, delivery).await?;
    }
    Ok(deliveries.len())
}

fn stable_outbox_id(delivery_id: &str, audience: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("agentdesk:scheduled-external:{delivery_id}:{PROVIDER}:{audience}").as_bytes(),
    )
}

pub(super) async fn tick(pool: &sqlx::PgPool, claim_owner: &str) -> bool {
    let now = Utc::now();
    let mut did_work = match db::recover_external_delivery_leases_pg(pool, now, MAX_RETRIES).await {
        Ok(recovered) => recovered > 0,
        Err(error) => {
            tracing::warn!("[smsg] external delivery recovery failed: {error}");
            false
        }
    };
    let claims = match db::claim_external_deliveries_pg(
        pool,
        claim_owner,
        CLAIM_BATCH,
        CLAIM_LEASE_SECS,
        now,
    )
    .await
    {
        Ok(claims) => claims,
        Err(error) => {
            tracing::warn!("[smsg] external delivery claim failed: {error}");
            return did_work;
        }
    };
    did_work |= !claims.is_empty();
    join_all(claims.into_iter().map(|claim| process_claim(pool, claim))).await;
    did_work
}

async fn process_claim(pool: &sqlx::PgPool, claim: ClaimedExternalDelivery) {
    if claim.provider != PROVIDER {
        finish_failed(pool, &claim, "unsupported_provider").await;
        return;
    }
    let payload: KakaoOutboxPayload = match serde_json::from_value(claim.payload.clone()) {
        Ok(payload) if payload_matches_claim(&claim, &payload) => payload,
        _ => {
            finish_failed(pool, &claim, "payload_invalid").await;
            return;
        }
    };
    let client = match kakao_client(&claim.account_id).await {
        Ok(client) => client,
        Err(error) => {
            retry_pre_dispatch(pool, &claim, kakao_error_code(&error)).await;
            return;
        }
    };
    if let Err(error) = client.prepare().await {
        retry_pre_dispatch(pool, &claim, kakao_error_code(&error)).await;
        return;
    }
    match db::mark_external_dispatch_started_pg(pool, claim.id, claim.claim_token, CLAIM_LEASE_SECS)
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(outbox_id = %claim.id, "[smsg] dispatch fence failed: {error}");
            return;
        }
    }

    let result = match claim.audience.as_str() {
        FRIENDS_AUDIENCE => {
            client
                .send_to_friends(&payload.friend_uuids, &payload.message)
                .await
        }
        SELF_AUDIENCE => client.send_to_self(&payload.message).await,
        _ => Err(KakaoError::InvalidConfiguration(
            "unsupported scheduled Kakao audience",
        )),
    };
    match result {
        Ok(summary) => finish_summary(pool, &claim, summary).await,
        Err(KakaoError::DeliveryUnknown) => finish_unknown(pool, &claim).await,
        Err(error) => finish_failed(pool, &claim, kakao_error_code(&error)).await,
    }
}

fn payload_matches_claim(claim: &ClaimedExternalDelivery, payload: &KakaoOutboxPayload) -> bool {
    match claim.audience.as_str() {
        FRIENDS_AUDIENCE => {
            !payload.friend_uuids.is_empty()
                && payload.friend_uuids.len() == claim.requested_count as usize
                && crate::services::kakao::validate_recipients(&payload.friend_uuids).is_ok()
                && crate::services::kakao_message::validate_message(&payload.message).is_ok()
        }
        SELF_AUDIENCE => {
            payload.friend_uuids.is_empty()
                && claim.requested_count == 1
                && crate::services::kakao_message::validate_message(&payload.message).is_ok()
        }
        _ => false,
    }
}

fn kakao_clients() -> &'static Mutex<HashMap<String, Arc<KakaoClient>>> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<KakaoClient>>>> = OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn kakao_client(account_id: &str) -> Result<Arc<KakaoClient>, KakaoError> {
    let mut clients = kakao_clients().lock().await;
    if let Some(client) = clients.get(account_id) {
        return Ok(client.clone());
    }
    let client = Arc::new(KakaoClient::from_process(Some(account_id))?);
    clients.insert(account_id.to_string(), client.clone());
    Ok(client)
}

async fn retry_pre_dispatch(
    pool: &sqlx::PgPool,
    claim: &ClaimedExternalDelivery,
    error_code: &'static str,
) {
    let retry_count = claim.retry_count.saturating_add(1);
    let next_attempt_at = Utc::now() + retry_delay(retry_count);
    if retry_count <= MAX_RETRIES && next_attempt_at < claim.deliver_before {
        if let Err(error) = db::retry_external_delivery_pg(
            pool,
            claim.id,
            claim.claim_token,
            retry_count,
            next_attempt_at,
            error_code,
        )
        .await
        {
            tracing::warn!(
                outbox_id = %claim.id,
                "[smsg] external retry persistence failed: {error}"
            );
        }
    } else {
        finish_failed(pool, claim, error_code).await;
    }
}

fn retry_delay(retry_count: i16) -> Duration {
    match retry_count {
        0 | 1 => Duration::seconds(15),
        2 => Duration::minutes(1),
        3 => Duration::minutes(5),
        4 => Duration::minutes(15),
        _ => Duration::hours(1),
    }
}

async fn finish_summary(
    pool: &sqlx::PgPool,
    claim: &ClaimedExternalDelivery,
    summary: KakaoDeliverySummary,
) {
    if summary.requested_count != claim.requested_count as usize
        || summary.successful_count + summary.failed_count != summary.requested_count
    {
        finish_unknown(pool, claim).await;
        return;
    }
    let Ok(successful_count) = i16::try_from(summary.successful_count) else {
        finish_unknown(pool, claim).await;
        return;
    };
    let Ok(failed_count) = i16::try_from(summary.failed_count) else {
        finish_unknown(pool, claim).await;
        return;
    };
    let status = if failed_count == 0 {
        "success"
    } else {
        "partial_success"
    };
    finish(
        pool,
        claim,
        status,
        Some(successful_count),
        Some(failed_count),
        (failed_count > 0).then_some("provider_partial"),
    )
    .await;
}

async fn finish_failed(
    pool: &sqlx::PgPool,
    claim: &ClaimedExternalDelivery,
    error_code: &'static str,
) {
    finish(
        pool,
        claim,
        "failed",
        Some(0),
        Some(claim.requested_count),
        Some(error_code),
    )
    .await;
}

async fn finish_unknown(pool: &sqlx::PgPool, claim: &ClaimedExternalDelivery) {
    finish(
        pool,
        claim,
        "unknown",
        None,
        None,
        Some("delivery_result_unknown"),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    pool: &sqlx::PgPool,
    claim: &ClaimedExternalDelivery,
    status: &str,
    successful_count: Option<i16>,
    failed_count: Option<i16>,
    error_code: Option<&str>,
) {
    if let Err(error) = db::finish_external_delivery_pg(
        pool,
        claim.id,
        claim.claim_token,
        status,
        successful_count,
        failed_count,
        error_code,
    )
    .await
    {
        tracing::warn!(outbox_id = %claim.id, "[smsg] external result persistence failed: {error}");
    }
}

fn kakao_error_code(error: &KakaoError) -> &'static str {
    match error {
        KakaoError::Disabled => "connector_disabled",
        KakaoError::InvalidConfiguration(_) => "connector_config_invalid",
        KakaoError::UnknownAccount => "account_not_configured",
        KakaoError::MissingCredentials => "credentials_missing",
        KakaoError::InvalidMessage(_) => "message_invalid",
        KakaoError::InvalidRecipients => "recipients_invalid",
        KakaoError::ReauthorizationRequired => "reauthorization_required",
        KakaoError::ConsentRequired => "consent_required",
        KakaoError::ProviderRejected(_) | KakaoError::ProviderResult(_) => "provider_rejected",
        KakaoError::DeliveryUnknown => "delivery_result_unknown",
        KakaoError::TransportInitialization => "transport_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_ids_are_stable_per_delivery_and_audience() {
        assert_eq!(
            stable_outbox_id("smdel-1", FRIENDS_AUDIENCE),
            stable_outbox_id("smdel-1", FRIENDS_AUDIENCE)
        );
        assert_ne!(
            stable_outbox_id("smdel-1", FRIENDS_AUDIENCE),
            stable_outbox_id("smdel-1", SELF_AUDIENCE)
        );
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1), Duration::seconds(15));
        assert_eq!(retry_delay(3), Duration::minutes(5));
        assert_eq!(retry_delay(5), Duration::hours(1));
    }

    #[test]
    fn delivery_unknown_is_never_classified_as_retryable() {
        assert_eq!(
            kakao_error_code(&KakaoError::DeliveryUnknown),
            "delivery_result_unknown"
        );
    }
}
