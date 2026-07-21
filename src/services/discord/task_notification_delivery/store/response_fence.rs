//! Durable per-turn card-before-response claims (#4055).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sqlx::{PgPool, Row};

use super::super::TaskCardScope;
use super::response_identity::{ResponseTurnCoordinates, TurnRelation, parse_turn_started_at};
use super::{db_id, memory_fallback_unavailable, message_id};
const RESPONSE_LEASE_SECONDS: i64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ResponseDeliveryOwner {
    Sink,
    Watcher,
}

impl ResponseDeliveryOwner {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sink => "sink",
            Self::Watcher => "watcher",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ResponseDeliveryClaim {
    pub(in super::super) scope: TaskCardScope,
    pub(in super::super) response_turn_key: String,
    pub(in super::super) card_message_id: u64,
    pub(in super::super) response_generation: i32,
    pub(in super::super) owner_token: String,
}

impl ResponseDeliveryClaim {
    pub(in crate::services::discord) fn response_turn_key(&self) -> &str {
        &self.response_turn_key
    }
    pub(in crate::services::discord) fn response_generation(&self) -> i32 {
        self.response_generation
    }
    pub(in crate::services::discord) fn card_message_id(&self) -> u64 {
        self.card_message_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ResponseDeliveryClaimOutcome {
    Owned(ResponseDeliveryClaim),
    Wait,
    SentUncommitted { card_message_id: u64 },
    Delivered { card_message_id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ExistingResponseDelivery {
    pub(in crate::services::discord) outcome: ResponseDeliveryClaimOutcome,
    pub(in crate::services::discord) card_message_id: u64,
    pub(in crate::services::discord) event_key: String,
    pub(in crate::services::discord) response_turn_key: String,
    pub(in crate::services::discord) card_bot_key: String,
}
#[allow(clippy::too_many_arguments)]
pub(in super::super) async fn claim_response_delivery(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    response_turn_key: &str,
    recovery_turn_key: Option<&str>,
    turn_started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    turn_end_offset: Option<u64>,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    validate_turn_key(response_turn_key)?;
    if let Some(recovery_turn_key) = recovery_turn_key {
        validate_turn_key(recovery_turn_key)?;
    }
    parse_turn_started_at(turn_started_at)?;
    let coordinates = ResponseTurnCoordinates::try_new(turn_start_offset, turn_end_offset)?;
    match pool {
        Some(pool) => {
            claim_response_delivery_pg(
                pool,
                scope,
                response_turn_key,
                recovery_turn_key,
                coordinates,
                card_message_id,
                owner,
            )
            .await
        }
        None if cfg!(any(test, debug_assertions)) => claim_response_delivery_memory(
            scope,
            response_turn_key,
            recovery_turn_key,
            coordinates,
            card_message_id,
            owner,
        ),
        None => Err(memory_fallback_unavailable()),
    }
}

/// Resume a response cycle using only its durable turn identity. This is the
/// watcher recovery path when the original provider envelope (and therefore
/// the semantic event key) is no longer available after a restart.
pub(in super::super) async fn claim_existing_response_delivery(
    pool: Option<&PgPool>,
    lookup_scope: &TaskCardScope,
    response_turn_key: &str,
    owner: ResponseDeliveryOwner,
) -> Result<Option<ExistingResponseDelivery>, String> {
    validate_turn_key(response_turn_key)?;
    match pool {
        Some(pool) => {
            claim_existing_response_delivery_pg(pool, lookup_scope, response_turn_key, owner).await
        }
        None if cfg!(any(test, debug_assertions)) => {
            claim_existing_response_delivery_memory(lookup_scope, response_turn_key, owner)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn renew_response_delivery(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    match pool {
        Some(pool) => renew_response_delivery_pg(pool, claim).await,
        None if cfg!(any(test, debug_assertions)) => renew_response_delivery_memory(claim),
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn mark_response_delivered(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    match pool {
        Some(pool) => {
            mark_response_delivered_pg(pool, claim).await?;
            super::cleanup_old_rows_pg(pool).await;
            Ok(())
        }
        None if cfg!(any(test, debug_assertions)) => mark_response_delivered_memory(claim),
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn mark_response_sent(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_response_sent_pg(pool, claim).await,
        None if cfg!(any(test, debug_assertions)) => mark_response_sent_memory(claim),
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn rebind_response_card(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    replacement_card_message_id: u64,
) -> Result<ResponseDeliveryClaim, String> {
    match pool {
        Some(pool) => rebind_response_card_pg(pool, claim, replacement_card_message_id).await,
        None if cfg!(any(test, debug_assertions)) => {
            rebind_response_card_memory(claim, replacement_card_message_id)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

async fn claim_response_delivery_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    response_turn_key: &str,
    recovery_turn_key: Option<&str>,
    coordinates: ResponseTurnCoordinates,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    let channel_id = db_id(scope.channel_id, "channel_id")?;
    let card_message_id_db = db_id(card_message_id, "message_id")?;
    let owner_token = uuid::Uuid::new_v4().to_string();
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO task_notification_response_delivery
             (channel_id, provider, session_key, event_key, response_turn_key, recovery_turn_key,
              turn_start_offset, turn_end_offset,
              referenced_card_message_id, delivery_state, owner_kind, owner_token,
              lease_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'claimed', $10, $11,
                 NOW() + make_interval(secs => $12))
         ON CONFLICT DO NOTHING
         RETURNING id",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(response_turn_key)
    .bind(recovery_turn_key)
    .bind(coordinates.start_offset)
    .bind(coordinates.end_offset)
    .bind(card_message_id_db)
    .bind(owner.as_str())
    .bind(&owner_token)
    .bind(RESPONSE_LEASE_SECONDS)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("claim task response delivery: {error}"))?;
    if let Some(inserted_id) = inserted {
        let prior_rows = sqlx::query(
            "SELECT turn_start_offset, turn_end_offset
             FROM task_notification_response_delivery
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3
               AND event_key = $4 AND id <> $5 AND delivery_state = 'delivered'
             ORDER BY delivered_at DESC",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&scope.session_key)
        .bind(&scope.event_key)
        .bind(inserted_id)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("check prior task response turns: {error}"))?;
        let mut relation = TurnRelation::Distinct;
        for row in prior_rows {
            let persisted = ResponseTurnCoordinates {
                start_offset: row.get("turn_start_offset"),
                end_offset: row.get("turn_end_offset"),
            };
            if relation.absorb(coordinates.relation(persisted)) {
                break;
            }
        }
        if relation != TurnRelation::Distinct {
            let deleted = sqlx::query(
                "DELETE FROM task_notification_response_delivery
                 WHERE id = $1 AND owner_token = $2 AND delivery_state = 'claimed'",
            )
            .bind(inserted_id)
            .bind(&owner_token)
            .execute(pool)
            .await
            .map_err(|error| format!("discard delayed cross-actor task response: {error}"))?
            .rows_affected();
            if deleted != 1 {
                return Err("delayed task response frame lost its provisional claim".to_string());
            }
            return Ok(if relation == TurnRelation::Same {
                ResponseDeliveryClaimOutcome::Delivered { card_message_id }
            } else {
                ResponseDeliveryClaimOutcome::Wait
            });
        }
        return Ok(ResponseDeliveryClaimOutcome::Owned(ResponseDeliveryClaim {
            scope: scope.clone(),
            response_turn_key: response_turn_key.to_string(),
            card_message_id,
            response_generation: 1,
            owner_token,
        }));
    }
    let mut rows = sqlx::query(
        "SELECT event_key, response_turn_key, recovery_turn_key,
                turn_start_offset, turn_end_offset, response_generation,
                referenced_card_message_id, delivery_state,
                lease_expires_at > NOW() AS lease_active
         FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND (response_turn_key = $4
                OR ($5::text IS NOT NULL AND recovery_turn_key = $5))",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(response_turn_key)
    .bind(recovery_turn_key)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load task response claim after conflict: {error}"))?;
    let matched_by_event = rows.is_empty();
    if matched_by_event {
        rows = sqlx::query(
            "SELECT event_key, response_turn_key, recovery_turn_key,
                    turn_start_offset, turn_end_offset, response_generation,
                    referenced_card_message_id, delivery_state,
                    lease_expires_at > NOW() AS lease_active
             FROM task_notification_response_delivery
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3
               AND event_key = $4
               AND delivery_state IN ('claimed', 'sent')",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&scope.session_key)
        .bind(&scope.event_key)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load active task response by event: {error}"))?;
    }
    if rows.len() != 1 {
        return Err(format!(
            "task response canonical/recovery identity matched {} rows",
            rows.len()
        ));
    }
    let current = &rows[0];
    let current_event: String = current.get("event_key");
    let canonical_turn_key: String = current.get("response_turn_key");
    let current_recovery_key: Option<String> = current.get("recovery_turn_key");
    let current_coordinates = ResponseTurnCoordinates {
        start_offset: current.get("turn_start_offset"),
        end_offset: current.get("turn_end_offset"),
    };
    let current_card: i64 = current.get("referenced_card_message_id");
    if current_event != scope.event_key || (!matched_by_event && current_card != card_message_id_db)
    {
        return Err("task response turn identity conflicts with another event/card".to_string());
    }
    if !matched_by_event
        && recovery_turn_key.is_some()
        && current_recovery_key.as_deref() != recovery_turn_key
    {
        return Err(
            "task response recovery identity conflicts with the persisted alias".to_string(),
        );
    }
    let same_event_turn =
        !matched_by_event || coordinates.relation(current_coordinates) == TurnRelation::Same;
    let blocked_active_turn = !same_event_turn || current_card != card_message_id_db;
    let state: String = current.get("delivery_state");
    if state == "delivered" {
        return Ok(ResponseDeliveryClaimOutcome::Delivered { card_message_id });
    }
    let lease_active: bool = current.get("lease_active");
    if state == "sent" {
        if lease_active {
            return Ok(if blocked_active_turn {
                ResponseDeliveryClaimOutcome::Wait
            } else {
                ResponseDeliveryClaimOutcome::SentUncommitted { card_message_id }
            });
        }
        let finalized = sqlx::query(
            "UPDATE task_notification_response_delivery
             SET delivery_state = 'delivered', owner_kind = NULL, owner_token = NULL,
                 lease_expires_at = NULL, delivered_at = NOW(), updated_at = NOW()
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3
               AND event_key = $4 AND response_turn_key = $5
               AND referenced_card_message_id = $6 AND delivery_state = 'sent'
               AND lease_expires_at <= NOW()",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&scope.session_key)
        .bind(&scope.event_key)
        .bind(&canonical_turn_key)
        .bind(current_card)
        .execute(pool)
        .await
        .map_err(|error| format!("finalize expired sent task response: {error}"))?
        .rows_affected();
        if finalized == 1 {
            super::cleanup_old_rows_pg(pool).await;
            return Ok(if blocked_active_turn {
                ResponseDeliveryClaimOutcome::Wait
            } else {
                ResponseDeliveryClaimOutcome::Delivered { card_message_id }
            });
        }
        return Ok(if blocked_active_turn {
            ResponseDeliveryClaimOutcome::Wait
        } else {
            ResponseDeliveryClaimOutcome::SentUncommitted { card_message_id }
        });
    }
    if !same_event_turn {
        return Ok(ResponseDeliveryClaimOutcome::Wait);
    }
    if lease_active {
        return Ok(ResponseDeliveryClaimOutcome::Wait);
    }
    let takeover = sqlx::query(
        "UPDATE task_notification_response_delivery
         SET response_generation = response_generation
                 + CASE WHEN referenced_card_message_id = $6 THEN 0 ELSE 1 END,
             referenced_card_message_id = $6, owner_kind = $7, owner_token = $8,
             lease_expires_at = NOW() + make_interval(secs => $9), updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5
           AND referenced_card_message_id = $10 AND delivery_state = 'claimed'
           AND lease_expires_at <= NOW()
         RETURNING response_generation",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(&canonical_turn_key)
    .bind(card_message_id_db)
    .bind(owner.as_str())
    .bind(&owner_token)
    .bind(RESPONSE_LEASE_SECONDS)
    .bind(current_card)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("take over expired task response claim: {error}"))?;
    if let Some(takeover) = takeover {
        Ok(ResponseDeliveryClaimOutcome::Owned(ResponseDeliveryClaim {
            scope: scope.clone(),
            response_turn_key: canonical_turn_key,
            card_message_id,
            response_generation: takeover.get("response_generation"),
            owner_token,
        }))
    } else {
        Ok(ResponseDeliveryClaimOutcome::Wait)
    }
}

async fn claim_existing_response_delivery_pg(
    pool: &PgPool,
    lookup_scope: &TaskCardScope,
    response_turn_key: &str,
    owner: ResponseDeliveryOwner,
) -> Result<Option<ExistingResponseDelivery>, String> {
    let rows = sqlx::query(
        "SELECT response.event_key, response.response_turn_key,
                response.recovery_turn_key, response.referenced_card_message_id,
                card.bot_key
         FROM task_notification_response_delivery AS response
         JOIN task_notification_card_state AS card
           ON card.channel_id = response.channel_id
          AND card.provider = response.provider
          AND card.session_key = response.session_key
          AND card.event_key = response.event_key
         WHERE response.channel_id = $1 AND response.provider = $2
           AND response.session_key = $3
           AND (response.response_turn_key = $4 OR response.recovery_turn_key = $4)",
    )
    .bind(db_id(lookup_scope.channel_id, "channel_id")?)
    .bind(&lookup_scope.provider)
    .bind(&lookup_scope.session_key)
    .bind(response_turn_key)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("find existing task response claim: {error}"))?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(format!(
            "task response recovery identity matched {} rows",
            rows.len()
        ));
    }
    let current = &rows[0];
    let event_key: String = current.get("event_key");
    let canonical_turn_key: String = current.get("response_turn_key");
    let recovery_turn_key: Option<String> = current.get("recovery_turn_key");
    let card_bot_key: String = current.get("bot_key");
    let card_message_id = message_id(Some(current.get("referenced_card_message_id")))?;
    let scope = TaskCardScope::new(
        lookup_scope.channel_id,
        lookup_scope.provider.clone(),
        lookup_scope.session_key.clone(),
        event_key.clone(),
    );
    let outcome = claim_response_delivery_pg(
        pool,
        &scope,
        &canonical_turn_key,
        recovery_turn_key.as_deref(),
        ResponseTurnCoordinates {
            start_offset: None,
            end_offset: None,
        },
        card_message_id,
        owner,
    )
    .await?;
    Ok(Some(ExistingResponseDelivery {
        outcome,
        card_message_id,
        event_key,
        response_turn_key: canonical_turn_key,
        card_bot_key,
    }))
}

async fn renew_response_delivery_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    let changed = sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() + make_interval(secs => $7), updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5 AND owner_token = $6
           AND delivery_state IN ('claimed', 'sent')",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(&claim.owner_token)
    .bind(RESPONSE_LEASE_SECONDS)
    .execute(pool)
    .await
    .map_err(|error| format!("renew task response claim: {error}"))?
    .rows_affected();
    exact_claim_change(changed, "renew task response claim")
}

async fn mark_response_delivered_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    let changed = sqlx::query(
        "UPDATE task_notification_response_delivery
         SET delivery_state = 'delivered', owner_kind = NULL, owner_token = NULL,
             lease_expires_at = NULL, sent_at = COALESCE(sent_at, NOW()),
             delivered_at = NOW(), updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5
           AND referenced_card_message_id = $6 AND owner_token = $7
           AND delivery_state IN ('claimed', 'sent')",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(&claim.owner_token)
    .execute(pool)
    .await
    .map_err(|error| format!("commit exact task response delivery: {error}"))?
    .rows_affected();
    exact_claim_change(changed, "commit exact task response delivery")
}

async fn mark_response_sent_pg(pool: &PgPool, claim: &ResponseDeliveryClaim) -> Result<(), String> {
    let changed = sqlx::query(
        "UPDATE task_notification_response_delivery
         SET delivery_state = 'sent', sent_at = COALESCE(sent_at, NOW()), updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5
           AND referenced_card_message_id = $6 AND owner_token = $7
           AND delivery_state = 'claimed'",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(&claim.owner_token)
    .execute(pool)
    .await
    .map_err(|error| format!("record exact task response send: {error}"))?
    .rows_affected();
    exact_claim_change(changed, "record exact task response send")
}

async fn rebind_response_card_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
    replacement_card_message_id: u64,
) -> Result<ResponseDeliveryClaim, String> {
    let row = sqlx::query(
        "UPDATE task_notification_response_delivery
         SET referenced_card_message_id = $8,
             response_generation = response_generation + 1,
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5
           AND referenced_card_message_id = $6 AND owner_token = $7
           AND delivery_state = 'claimed'
         RETURNING response_generation",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(&claim.owner_token)
    .bind(db_id(replacement_card_message_id, "message_id")?)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("rebind exact task response card: {error}"))?
    .ok_or_else(|| "rebind exact task response card lost exact claim ownership".to_string())?;
    let mut rebound = claim.clone();
    rebound.card_message_id = replacement_card_message_id;
    rebound.response_generation = row.get("response_generation");
    Ok(rebound)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryResponseState {
    Claimed,
    Sent,
    Delivered,
}
#[derive(Clone, Debug)]
struct MemoryResponseRow {
    event_key: String,
    recovery_turn_key: Option<String>,
    card_message_id: u64,
    response_generation: i32,
    state: MemoryResponseState,
    owner_token: Option<String>,
    lease_expires_at: Option<Instant>,
    coordinates: ResponseTurnCoordinates,
    delivered_at: Option<chrono::DateTime<chrono::Utc>>,
}

type MemoryResponseKey = (u64, String, String, String);

static MEMORY_RESPONSES: LazyLock<Mutex<HashMap<MemoryResponseKey, MemoryResponseRow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static FORCED_DELIVER_FAILURES: LazyLock<Mutex<HashMap<MemoryResponseKey, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
pub(in super::super) fn force_response_deliver_failures(
    claim: &ResponseDeliveryClaim,
    attempts: usize,
) {
    FORCED_DELIVER_FAILURES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(memory_key(&claim.scope, &claim.response_turn_key), attempts);
}

fn memory_key(scope: &TaskCardScope, response_turn_key: &str) -> MemoryResponseKey {
    (
        scope.channel_id,
        scope.provider.clone(),
        scope.session_key.clone(),
        response_turn_key.to_string(),
    )
}

fn claim_response_delivery_memory(
    scope: &TaskCardScope,
    response_turn_key: &str,
    recovery_turn_key: Option<&str>,
    coordinates: ResponseTurnCoordinates,
    card_message_id: u64,
    _owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let mut matching_keys: Vec<MemoryResponseKey> = rows
        .iter()
        .filter(|((channel_id, provider, session_key, canonical), row)| {
            *channel_id == scope.channel_id
                && provider == &scope.provider
                && session_key == &scope.session_key
                && (canonical == response_turn_key
                    || row.recovery_turn_key.as_deref() == Some(response_turn_key)
                    || recovery_turn_key.is_some_and(|recovery| {
                        canonical == recovery || row.recovery_turn_key.as_deref() == Some(recovery)
                    }))
        })
        .map(|(key, _)| key.clone())
        .collect();
    let matched_by_event = matching_keys.is_empty();
    if matched_by_event {
        matching_keys = rows
            .iter()
            .filter(|((channel_id, provider, session_key, _), row)| {
                *channel_id == scope.channel_id
                    && provider == &scope.provider
                    && session_key == &scope.session_key
                    && row.event_key == scope.event_key
                    && row.state != MemoryResponseState::Delivered
            })
            .map(|(key, _)| key.clone())
            .collect();
    }
    if matching_keys.len() > 1 {
        return Err(
            "task response canonical/recovery identity matched multiple memory rows".into(),
        );
    }
    let key = matching_keys
        .into_iter()
        .next()
        .unwrap_or_else(|| memory_key(scope, response_turn_key));
    let canonical_turn_key = key.3.clone();
    let owner_token = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    if !rows.contains_key(&key) {
        let mut relation = TurnRelation::Distinct;
        for (_, row) in rows.iter().filter(|(key, row)| {
            key.0 == scope.channel_id
                && key.1 == scope.provider
                && key.2 == scope.session_key
                && row.event_key == scope.event_key
                && row.state == MemoryResponseState::Delivered
        }) {
            if relation.absorb(coordinates.relation(row.coordinates)) {
                break;
            }
        }
        if relation == TurnRelation::Same {
            return Ok(ResponseDeliveryClaimOutcome::Delivered { card_message_id });
        }
        if relation == TurnRelation::Unknown {
            return Ok(ResponseDeliveryClaimOutcome::Wait);
        }
    }
    let same_event_turn = !matched_by_event
        || !rows.contains_key(&key)
        || rows
            .get(&key)
            .is_some_and(|row| coordinates.relation(row.coordinates) == TurnRelation::Same);
    let blocked_sent = !same_event_turn
        || rows
            .get(&key)
            .is_some_and(|row| row.card_message_id != card_message_id);
    if blocked_sent
        && rows
            .get(&key)
            .is_some_and(|row| row.state == MemoryResponseState::Sent)
    {
        if let Some(row) = rows.get_mut(&key)
            && row.lease_expires_at.is_some_and(|expiry| expiry <= now)
        {
            row.state = MemoryResponseState::Delivered;
            row.owner_token = None;
            row.lease_expires_at = None;
            row.delivered_at = Some(chrono::Utc::now());
        }
        return Ok(ResponseDeliveryClaimOutcome::Wait);
    }
    if !same_event_turn {
        return Ok(ResponseDeliveryClaimOutcome::Wait);
    }
    match rows.get_mut(&key) {
        None => {
            rows.insert(
                key.clone(),
                MemoryResponseRow {
                    event_key: scope.event_key.clone(),
                    recovery_turn_key: recovery_turn_key.map(str::to_string),
                    card_message_id,
                    response_generation: 1,
                    state: MemoryResponseState::Claimed,
                    owner_token: Some(owner_token.clone()),
                    lease_expires_at: Some(
                        now + Duration::from_secs(RESPONSE_LEASE_SECONDS as u64),
                    ),
                    coordinates,
                    delivered_at: None,
                },
            );
        }
        Some(row)
            if row.event_key != scope.event_key
                || (!matched_by_event && row.card_message_id != card_message_id) =>
        {
            return Err(
                "task response turn identity conflicts with another event/card".to_string(),
            );
        }
        Some(row)
            if !matched_by_event
                && recovery_turn_key.is_some()
                && row.recovery_turn_key.as_deref() != recovery_turn_key =>
        {
            return Err(
                "task response recovery identity conflicts with the persisted alias".to_string(),
            );
        }
        Some(row) if row.state == MemoryResponseState::Delivered => {
            return Ok(ResponseDeliveryClaimOutcome::Delivered { card_message_id });
        }
        Some(row)
            if row.state == MemoryResponseState::Sent
                && row.lease_expires_at.is_some_and(|expiry| expiry > now) =>
        {
            return Ok(ResponseDeliveryClaimOutcome::SentUncommitted { card_message_id });
        }
        Some(row) if row.state == MemoryResponseState::Sent => {
            row.state = MemoryResponseState::Delivered;
            row.owner_token = None;
            row.lease_expires_at = None;
            row.delivered_at = Some(chrono::Utc::now());
            return Ok(ResponseDeliveryClaimOutcome::Delivered { card_message_id });
        }
        Some(row) if row.lease_expires_at.is_some_and(|expiry| expiry > now) => {
            return Ok(ResponseDeliveryClaimOutcome::Wait);
        }
        Some(row) => {
            if row.card_message_id != card_message_id {
                row.response_generation += 1;
            }
            row.card_message_id = card_message_id;
            row.owner_token = Some(owner_token.clone());
            row.lease_expires_at = Some(now + Duration::from_secs(RESPONSE_LEASE_SECONDS as u64));
        }
    }
    Ok(ResponseDeliveryClaimOutcome::Owned(ResponseDeliveryClaim {
        scope: scope.clone(),
        response_turn_key: canonical_turn_key,
        card_message_id,
        response_generation: rows
            .get(&key)
            .map(|row| row.response_generation)
            .unwrap_or(1),
        owner_token,
    }))
}

fn claim_existing_response_delivery_memory(
    lookup_scope: &TaskCardScope,
    response_turn_key: &str,
    owner: ResponseDeliveryOwner,
) -> Result<Option<ExistingResponseDelivery>, String> {
    let existing = {
        let rows = MEMORY_RESPONSES
            .lock()
            .map_err(|_| "task response memory store poisoned".to_string())?;
        let mut matches =
            rows.iter()
                .filter(|((channel_id, provider, session_key, canonical), row)| {
                    *channel_id == lookup_scope.channel_id
                        && provider == &lookup_scope.provider
                        && session_key == &lookup_scope.session_key
                        && (canonical == response_turn_key
                            || row.recovery_turn_key.as_deref() == Some(response_turn_key))
                });
        let first = matches.next().map(|((_, _, _, canonical), row)| {
            (
                canonical.clone(),
                row.recovery_turn_key.clone(),
                row.event_key.clone(),
                row.card_message_id,
            )
        });
        if matches.next().is_some() {
            return Err("task response recovery identity matched multiple memory rows".to_string());
        }
        first
    };
    let Some((canonical_turn_key, recovery_turn_key, event_key, card_message_id)) = existing else {
        return Ok(None);
    };
    let scope = TaskCardScope::new(
        lookup_scope.channel_id,
        lookup_scope.provider.clone(),
        lookup_scope.session_key.clone(),
        event_key.clone(),
    );
    let outcome = claim_response_delivery_memory(
        &scope,
        &canonical_turn_key,
        recovery_turn_key.as_deref(),
        ResponseTurnCoordinates {
            start_offset: None,
            end_offset: None,
        },
        card_message_id,
        owner,
    )?;
    Ok(Some(ExistingResponseDelivery {
        outcome,
        card_message_id,
        event_key,
        response_turn_key: canonical_turn_key,
        card_bot_key: format!("provider:{}", lookup_scope.provider),
    }))
}

fn renew_response_delivery_memory(claim: &ResponseDeliveryClaim) -> Result<(), String> {
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(&claim.scope, &claim.response_turn_key))
        .ok_or_else(|| "task response memory claim disappeared".to_string())?;
    if !matches!(
        row.state,
        MemoryResponseState::Claimed | MemoryResponseState::Sent
    ) || row.owner_token.as_deref() != Some(claim.owner_token.as_str())
    {
        return Err("task response memory claim ownership changed".to_string());
    }
    row.lease_expires_at =
        Some(Instant::now() + Duration::from_secs(RESPONSE_LEASE_SECONDS as u64));
    Ok(())
}

fn mark_response_sent_memory(claim: &ResponseDeliveryClaim) -> Result<(), String> {
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(&claim.scope, &claim.response_turn_key))
        .ok_or_else(|| "task response memory claim disappeared".to_string())?;
    if row.state != MemoryResponseState::Claimed
        || row.owner_token.as_deref() != Some(claim.owner_token.as_str())
        || row.card_message_id != claim.card_message_id
    {
        return Err("task response memory claim ownership changed".to_string());
    }
    row.state = MemoryResponseState::Sent;
    Ok(())
}

fn rebind_response_card_memory(
    claim: &ResponseDeliveryClaim,
    replacement_card_message_id: u64,
) -> Result<ResponseDeliveryClaim, String> {
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(&claim.scope, &claim.response_turn_key))
        .ok_or_else(|| "task response memory claim disappeared".to_string())?;
    if row.state != MemoryResponseState::Claimed
        || row.owner_token.as_deref() != Some(claim.owner_token.as_str())
        || row.card_message_id != claim.card_message_id
    {
        return Err("task response memory claim ownership changed".to_string());
    }
    row.card_message_id = replacement_card_message_id;
    row.response_generation += 1;
    let mut rebound = claim.clone();
    rebound.card_message_id = replacement_card_message_id;
    rebound.response_generation = row.response_generation;
    Ok(rebound)
}

fn mark_response_delivered_memory(claim: &ResponseDeliveryClaim) -> Result<(), String> {
    #[cfg(test)]
    {
        let key = memory_key(&claim.scope, &claim.response_turn_key);
        let mut failures = FORCED_DELIVER_FAILURES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(remaining) = failures.get_mut(&key)
            && *remaining > 0
        {
            *remaining -= 1;
            return Err("forced final task response delivery CAS failure".to_string());
        }
        failures.remove(&key);
    }
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(&claim.scope, &claim.response_turn_key))
        .ok_or_else(|| "task response memory claim disappeared".to_string())?;
    if !matches!(
        row.state,
        MemoryResponseState::Claimed | MemoryResponseState::Sent
    ) || row.owner_token.as_deref() != Some(claim.owner_token.as_str())
        || row.card_message_id != claim.card_message_id
    {
        return Err("task response memory claim ownership changed".to_string());
    }
    row.state = MemoryResponseState::Delivered;
    row.owner_token = None;
    row.lease_expires_at = None;
    row.delivered_at = Some(chrono::Utc::now());
    Ok(())
}

fn validate_turn_key(response_turn_key: &str) -> Result<(), String> {
    (response_turn_key.len() == 64)
        .then_some(())
        .ok_or_else(|| "task response turn key must be a 64-character fingerprint".to_string())
}

fn exact_claim_change(changed: u64, action: &str) -> Result<(), String> {
    (changed == 1)
        .then_some(())
        .ok_or_else(|| format!("{action} changed {changed} rows; exact claim ownership was lost"))
}
