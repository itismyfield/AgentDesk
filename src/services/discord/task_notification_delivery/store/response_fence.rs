//! Durable per-turn card-before-response claims (#4055).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sqlx::{PgPool, Row};

use super::super::TaskCardScope;
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
    pub(in super::super) owner_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ResponseDeliveryClaimOutcome {
    Owned(ResponseDeliveryClaim),
    Wait,
    Delivered { card_message_id: u64 },
}

pub(in super::super) async fn claim_response_delivery(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    response_turn_key: &str,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    validate_turn_key(response_turn_key)?;
    match pool {
        Some(pool) => {
            claim_response_delivery_pg(pool, scope, response_turn_key, card_message_id, owner).await
        }
        None if cfg!(any(test, debug_assertions)) => {
            claim_response_delivery_memory(scope, response_turn_key, card_message_id, owner)
        }
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
) -> Result<Option<(ResponseDeliveryClaimOutcome, u64)>, String> {
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

async fn claim_response_delivery_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    response_turn_key: &str,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    let channel_id = db_id(scope.channel_id, "channel_id")?;
    let card_message_id_db = db_id(card_message_id, "message_id")?;
    let owner_token = uuid::Uuid::new_v4().to_string();
    let inserted = sqlx::query(
        "INSERT INTO task_notification_response_delivery
             (channel_id, provider, session_key, event_key, response_turn_key,
              referenced_card_message_id, delivery_state, owner_kind, owner_token,
              lease_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, 'claimed', $7, $8,
                 NOW() + make_interval(secs => $9))
         ON CONFLICT (channel_id, provider, session_key, response_turn_key) DO NOTHING
         RETURNING id",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(response_turn_key)
    .bind(card_message_id_db)
    .bind(owner.as_str())
    .bind(&owner_token)
    .bind(RESPONSE_LEASE_SECONDS)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("claim task response delivery: {error}"))?;
    if inserted.is_some() {
        return Ok(ResponseDeliveryClaimOutcome::Owned(ResponseDeliveryClaim {
            scope: scope.clone(),
            response_turn_key: response_turn_key.to_string(),
            card_message_id,
            owner_token,
        }));
    }

    let current = sqlx::query(
        "SELECT event_key, referenced_card_message_id, delivery_state,
                lease_expires_at > NOW() AS lease_active
         FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND response_turn_key = $4",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(response_turn_key)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("load task response claim after conflict: {error}"))?;
    let current_event: String = current.get("event_key");
    let current_card: i64 = current.get("referenced_card_message_id");
    if current_event != scope.event_key || current_card != card_message_id_db {
        return Err("task response turn identity conflicts with another event/card".to_string());
    }
    let state: String = current.get("delivery_state");
    if state == "delivered" {
        return Ok(ResponseDeliveryClaimOutcome::Delivered { card_message_id });
    }
    let lease_active: bool = current.get("lease_active");
    if lease_active {
        return Ok(ResponseDeliveryClaimOutcome::Wait);
    }

    let changed = sqlx::query(
        "UPDATE task_notification_response_delivery
         SET owner_kind = $7, owner_token = $8,
             lease_expires_at = NOW() + make_interval(secs => $9), updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5
           AND referenced_card_message_id = $6 AND delivery_state = 'claimed'
           AND lease_expires_at <= NOW()",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(response_turn_key)
    .bind(card_message_id_db)
    .bind(owner.as_str())
    .bind(&owner_token)
    .bind(RESPONSE_LEASE_SECONDS)
    .execute(pool)
    .await
    .map_err(|error| format!("take over expired task response claim: {error}"))?
    .rows_affected();
    if changed == 1 {
        Ok(ResponseDeliveryClaimOutcome::Owned(ResponseDeliveryClaim {
            scope: scope.clone(),
            response_turn_key: response_turn_key.to_string(),
            card_message_id,
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
) -> Result<Option<(ResponseDeliveryClaimOutcome, u64)>, String> {
    let current = sqlx::query(
        "SELECT event_key, referenced_card_message_id
         FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND response_turn_key = $4",
    )
    .bind(db_id(lookup_scope.channel_id, "channel_id")?)
    .bind(&lookup_scope.provider)
    .bind(&lookup_scope.session_key)
    .bind(response_turn_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("find existing task response claim: {error}"))?;
    let Some(current) = current else {
        return Ok(None);
    };
    let event_key: String = current.get("event_key");
    let card_message_id = message_id(Some(current.get("referenced_card_message_id")))?;
    let scope = TaskCardScope::new(
        lookup_scope.channel_id,
        lookup_scope.provider.clone(),
        lookup_scope.session_key.clone(),
        event_key,
    );
    let outcome =
        claim_response_delivery_pg(pool, &scope, response_turn_key, card_message_id, owner).await?;
    Ok(Some((outcome, card_message_id)))
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
           AND delivery_state = 'claimed'",
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
             lease_expires_at = NULL, delivered_at = NOW(), updated_at = NOW()
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
    .map_err(|error| format!("commit exact task response delivery: {error}"))?
    .rows_affected();
    exact_claim_change(changed, "commit exact task response delivery")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryResponseState {
    Claimed,
    Delivered,
}

#[derive(Clone, Debug)]
struct MemoryResponseRow {
    event_key: String,
    card_message_id: u64,
    state: MemoryResponseState,
    owner_token: Option<String>,
    lease_expires_at: Option<Instant>,
}

type MemoryResponseKey = (u64, String, String, String);

static MEMORY_RESPONSES: LazyLock<Mutex<HashMap<MemoryResponseKey, MemoryResponseRow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
    card_message_id: u64,
    _owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let key = memory_key(scope, response_turn_key);
    let owner_token = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    match rows.get_mut(&key) {
        None => {
            rows.insert(
                key,
                MemoryResponseRow {
                    event_key: scope.event_key.clone(),
                    card_message_id,
                    state: MemoryResponseState::Claimed,
                    owner_token: Some(owner_token.clone()),
                    lease_expires_at: Some(
                        now + Duration::from_secs(RESPONSE_LEASE_SECONDS as u64),
                    ),
                },
            );
        }
        Some(row) if row.event_key != scope.event_key || row.card_message_id != card_message_id => {
            return Err(
                "task response turn identity conflicts with another event/card".to_string(),
            );
        }
        Some(row) if row.state == MemoryResponseState::Delivered => {
            return Ok(ResponseDeliveryClaimOutcome::Delivered { card_message_id });
        }
        Some(row) if row.lease_expires_at.is_some_and(|expiry| expiry > now) => {
            return Ok(ResponseDeliveryClaimOutcome::Wait);
        }
        Some(row) => {
            row.owner_token = Some(owner_token.clone());
            row.lease_expires_at = Some(now + Duration::from_secs(RESPONSE_LEASE_SECONDS as u64));
        }
    }
    Ok(ResponseDeliveryClaimOutcome::Owned(ResponseDeliveryClaim {
        scope: scope.clone(),
        response_turn_key: response_turn_key.to_string(),
        card_message_id,
        owner_token,
    }))
}

fn claim_existing_response_delivery_memory(
    lookup_scope: &TaskCardScope,
    response_turn_key: &str,
    owner: ResponseDeliveryOwner,
) -> Result<Option<(ResponseDeliveryClaimOutcome, u64)>, String> {
    let existing = {
        let rows = MEMORY_RESPONSES
            .lock()
            .map_err(|_| "task response memory store poisoned".to_string())?;
        rows.get(&memory_key(lookup_scope, response_turn_key))
            .map(|row| (row.event_key.clone(), row.card_message_id))
    };
    let Some((event_key, card_message_id)) = existing else {
        return Ok(None);
    };
    let scope = TaskCardScope::new(
        lookup_scope.channel_id,
        lookup_scope.provider.clone(),
        lookup_scope.session_key.clone(),
        event_key,
    );
    let outcome =
        claim_response_delivery_memory(&scope, response_turn_key, card_message_id, owner)?;
    Ok(Some((outcome, card_message_id)))
}

fn renew_response_delivery_memory(claim: &ResponseDeliveryClaim) -> Result<(), String> {
    let mut rows = MEMORY_RESPONSES
        .lock()
        .map_err(|_| "task response memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(&claim.scope, &claim.response_turn_key))
        .ok_or_else(|| "task response memory claim disappeared".to_string())?;
    if row.state != MemoryResponseState::Claimed
        || row.owner_token.as_deref() != Some(claim.owner_token.as_str())
    {
        return Err("task response memory claim ownership changed".to_string());
    }
    row.lease_expires_at =
        Some(Instant::now() + Duration::from_secs(RESPONSE_LEASE_SECONDS as u64));
    Ok(())
}

fn mark_response_delivered_memory(claim: &ResponseDeliveryClaim) -> Result<(), String> {
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
    row.state = MemoryResponseState::Delivered;
    row.owner_token = None;
    row.lease_expires_at = None;
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
