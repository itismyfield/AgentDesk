//! PostgreSQL-backed task-card state and lease authority (#4055).

mod card_claim;
mod missing_card_replacement;
mod response_chunks;
mod response_fence;
mod response_identity;
mod retention;
mod terminal_footer;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use super::{TaskCardScope, stable_nonce};

#[cfg(test)]
pub(super) use card_claim::install_new_terminal_claim_race_hook;
use card_claim::{claim_card_pg, find_terminal_delivery_pg};
use retention::cleanup_old_rows_pg;
#[cfg(test)]
pub(super) use retention::cleanup_old_rows_pg_checked;

pub(super) use missing_card_replacement::{
    MissingCardReplacementClaim, claim_missing_card_replacement,
};
use terminal_footer::record_footer_only_pg;

pub(super) use response_chunks::{
    PreparedResponseChunk, ResponseChunkJournal, ResponseChunkPrepareError, confirm_response_chunk,
    mark_response_chunk_ambiguous, mark_response_chunk_posting, prepare_response_chunk,
};
#[cfg(test)]
pub(super) use response_fence::force_response_deliver_failures;
pub(in crate::services::discord) use response_fence::{
    ExistingResponseDelivery, ResponseDeliveryClaim, ResponseDeliveryClaimOutcome,
    ResponseDeliveryOwner,
};
pub(super) use response_fence::{
    claim_existing_response_delivery, claim_response_delivery, mark_response_delivered,
    mark_response_sent, rebind_response_card, renew_response_delivery,
};

const LEASE_SECONDS: i64 = 30;
const RETENTION_DAYS: i64 = 7;
const RETENTION_DELETE_LIMIT: i64 = 64;

fn memory_fallback_unavailable() -> String {
    "task card PostgreSQL authority is unavailable in an optimized release build".to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StoreIntent {
    Observation,
    Promotion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClaimAction {
    Post,
    Edit { message_id: u64 },
}

#[derive(Clone, Debug)]
pub(super) struct ClaimedCard {
    pub(super) scope: TaskCardScope,
    pub(super) lease_owner: String,
    pub(super) bot_key: String,
    pub(super) discord_nonce: String,
    pub(super) revision: i32,
    pub(super) update_count: u64,
    pub(super) rendered_content: String,
    pub(super) action: ClaimAction,
    pub(super) new_terminal_completion: bool,
}

#[derive(Clone, Debug)]
pub(super) enum CardClaim {
    Existing { message_id: u64, bot_key: String },
    Owned(ClaimedCard),
    Busy { bot_key: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CardPostAttempt {
    pub(super) started_at: DateTime<Utc>,
    pub(super) resumed: bool,
}

pub(super) async fn record_footer_only(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => record_footer_only_pg(pool, scope, content, content_hash).await,
        None if cfg!(any(test, debug_assertions)) => {
            record_footer_only_memory(scope, content, content_hash)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn claim_card(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    preferred_bot_key: &str,
    seed_content: &str,
    seed_hash: &str,
    intent: StoreIntent,
) -> Result<CardClaim, String> {
    match pool {
        Some(pool) => {
            claim_card_pg(
                pool,
                scope,
                preferred_bot_key,
                seed_content,
                seed_hash,
                intent,
            )
            .await
        }
        None if cfg!(any(test, debug_assertions)) => {
            claim_card_memory(scope, preferred_bot_key, seed_content, seed_hash, intent)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn mark_posted(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
    message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_posted_pg(pool, claimed, message_id, content, content_hash).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_posted_memory(claimed, message_id, content, content_hash)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn begin_card_post(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
) -> Result<CardPostAttempt, String> {
    match pool {
        Some(pool) => begin_card_post_pg(pool, claimed).await,
        None if cfg!(any(test, debug_assertions)) => begin_card_post_memory(claimed),
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn mark_card_post_ambiguous(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
    error: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_failure_pg(pool, claimed, None, "posting", error).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_failure_memory(claimed, None, MemoryState::Posting, error)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn mark_edited(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
    message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_edited_pg(pool, claimed, message_id, content, content_hash).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_edited_memory(claimed, message_id, content, content_hash)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn mark_post_failure(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
    error: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_failure_pg(pool, claimed, None, "posting", error).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_failure_memory(claimed, None, MemoryState::Posting, error)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn mark_edit_failure(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
    message_id: u64,
    error: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_failure_pg(pool, claimed, Some(message_id), "card_posted", error).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_failure_memory(claimed, Some(message_id), MemoryState::CardPosted, error)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(super) async fn prepare_replacement(
    pool: Option<&PgPool>,
    claimed: &ClaimedCard,
    missing_message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<ClaimedCard, String> {
    match pool {
        Some(pool) => {
            prepare_replacement_pg(pool, claimed, missing_message_id, content, content_hash).await
        }
        None if cfg!(any(test, debug_assertions)) => {
            prepare_replacement_memory(claimed, missing_message_id, content, content_hash)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

fn claimed_from_row(
    scope: &TaskCardScope,
    lease_owner: String,
    row: &sqlx::postgres::PgRow,
    action: ClaimAction,
) -> Result<ClaimedCard, String> {
    let update_count: i64 = row.get("update_count");
    Ok(ClaimedCard {
        scope: scope.clone(),
        lease_owner,
        bot_key: row.get("bot_key"),
        discord_nonce: row.get("discord_nonce"),
        revision: row.get("revision"),
        update_count: u64::try_from(update_count)
            .map_err(|_| format!("invalid task card update_count {update_count}"))?,
        rendered_content: row.get("rendered_content"),
        action,
        new_terminal_completion: false,
    })
}

async fn begin_card_post_pg(
    pool: &PgPool,
    claimed: &ClaimedCard,
) -> Result<CardPostAttempt, String> {
    let existing: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT post_started_at FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $5 AND delivery_state = 'posting'",
    )
    .bind(db_id(claimed.scope.channel_id, "channel_id")?)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&claimed.lease_owner)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load task card POST boundary: {error}"))?
    .ok_or_else(|| "task card POST boundary lost exact lease ownership".to_string())?;
    if let Some(started_at) = existing {
        return Ok(CardPostAttempt {
            started_at,
            resumed: true,
        });
    }
    let started_at: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE task_notification_card_state
         SET post_started_at = NOW(), updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $5 AND delivery_state = 'posting'
           AND post_started_at IS NULL
         RETURNING post_started_at",
    )
    .bind(db_id(claimed.scope.channel_id, "channel_id")?)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&claimed.lease_owner)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("persist task card POST boundary: {error}"))?
    .ok_or_else(|| "task card POST boundary CAS changed zero rows".to_string())?;
    Ok(CardPostAttempt {
        started_at,
        resumed: false,
    })
}

async fn mark_posted_pg(
    pool: &PgPool,
    claimed: &ClaimedCard,
    message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| format!("begin posted task card commit: {error}"))?;
    let changed = sqlx::query(
        "UPDATE task_notification_card_state
         SET surface_owner = 'card', delivery_state = 'card_posted',
             discord_message_id = $6, rendered_content = $7, content_hash = $8,
             lease_owner = NULL, lease_expires_at = NULL, last_error = NULL,
             post_started_at = NULL,
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $5 AND delivery_state = 'posting'",
    )
    .bind(db_id(claimed.scope.channel_id, "channel_id")?)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&claimed.lease_owner)
    .bind(db_id(message_id, "message_id")?)
    .bind(content)
    .bind(content_hash)
    .execute(&mut *transaction)
    .await
    .map_err(|error| format!("commit posted task card: {error}"))?
    .rows_affected();
    exact_change(changed, "commit posted task card")?;
    let journal = if claimed.scope.terminal_delivery_fingerprint.is_some() {
        sqlx::query(
            "INSERT INTO task_notification_terminal_delivery
                 (channel_id, provider, session_key, event_key,
                  terminal_delivery_fingerprint, discord_message_id, bot_key, content_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (channel_id, provider, terminal_delivery_fingerprint)
                 WHERE terminal_delivery_fingerprint IS NOT NULL
             DO UPDATE SET
                 session_key = EXCLUDED.session_key,
                 event_key = EXCLUDED.event_key,
                 discord_message_id = EXCLUDED.discord_message_id,
                 bot_key = EXCLUDED.bot_key,
                 content_hash = EXCLUDED.content_hash,
                 delivered_at = NOW()",
        )
        .bind(db_id(claimed.scope.channel_id, "channel_id")?)
        .bind(&claimed.scope.provider)
        .bind(&claimed.scope.session_key)
        .bind(&claimed.scope.event_key)
        .bind(&claimed.scope.terminal_delivery_fingerprint)
        .bind(db_id(message_id, "message_id")?)
        .bind(&claimed.bot_key)
        .bind(content_hash)
        .execute(&mut *transaction)
        .await
    } else {
        sqlx::query(
            "INSERT INTO task_notification_terminal_delivery
                 (channel_id, provider, session_key, event_key,
                  terminal_delivery_fingerprint, discord_message_id, bot_key, content_hash)
             VALUES ($1, $2, $3, $4, NULL, $5, $6, $7)
             ON CONFLICT (channel_id, provider, session_key, event_key, discord_message_id)
             DO UPDATE SET
                 bot_key = EXCLUDED.bot_key,
                 content_hash = EXCLUDED.content_hash,
                 delivered_at = NOW()",
        )
        .bind(db_id(claimed.scope.channel_id, "channel_id")?)
        .bind(&claimed.scope.provider)
        .bind(&claimed.scope.session_key)
        .bind(&claimed.scope.event_key)
        .bind(db_id(message_id, "message_id")?)
        .bind(&claimed.bot_key)
        .bind(content_hash)
        .execute(&mut *transaction)
        .await
    };
    journal.map_err(|error| format!("journal terminal card delivery: {error}"))?;
    transaction
        .commit()
        .await
        .map_err(|error| format!("commit posted task card transaction: {error}"))?;
    cleanup_old_rows_pg(pool).await;
    Ok(())
}

async fn mark_edited_pg(
    pool: &PgPool,
    claimed: &ClaimedCard,
    message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    let changed = sqlx::query(
        "UPDATE task_notification_card_state
         SET rendered_content = $7, content_hash = $8,
             lease_owner = NULL, lease_expires_at = NULL, last_error = NULL,
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $5 AND delivery_state = 'card_posted'
           AND discord_message_id = $6",
    )
    .bind(db_id(claimed.scope.channel_id, "channel_id")?)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&claimed.lease_owner)
    .bind(db_id(message_id, "message_id")?)
    .bind(content)
    .bind(content_hash)
    .execute(pool)
    .await
    .map_err(|error| format!("commit edited task card: {error}"))?
    .rows_affected();
    exact_change(changed, "commit edited task card")
}

async fn mark_failure_pg(
    pool: &PgPool,
    claimed: &ClaimedCard,
    message_id: Option<u64>,
    expected_state: &str,
    error: &str,
) -> Result<(), String> {
    let message_id = message_id.map(|id| db_id(id, "message_id")).transpose()?;
    let changed = sqlx::query(
        "UPDATE task_notification_card_state
         SET lease_owner = NULL, lease_expires_at = NULL, last_error = $7,
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $5 AND delivery_state = $6
           AND ($8::BIGINT IS NULL OR discord_message_id = $8)",
    )
    .bind(db_id(claimed.scope.channel_id, "channel_id")?)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&claimed.lease_owner)
    .bind(expected_state)
    .bind(error)
    .bind(message_id)
    .execute(pool)
    .await
    .map_err(|db_error| format!("record task card transport failure: {db_error}"))?
    .rows_affected();
    exact_change(changed, "record task card transport failure")
}

async fn prepare_replacement_pg(
    pool: &PgPool,
    claimed: &ClaimedCard,
    missing_message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<ClaimedCard, String> {
    let next_revision = claimed.revision.saturating_add(1);
    let next_nonce = stable_nonce(&claimed.scope, next_revision);
    let row = sqlx::query(
        "UPDATE task_notification_card_state
         SET delivery_state = 'posting', discord_message_id = NULL,
             revision = $7, discord_nonce = $8,
             lease_expires_at = NOW() + make_interval(secs => $9),
             rendered_content = $10, content_hash = $11,
             post_started_at = NULL, last_error = NULL, updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $5 AND delivery_state = 'card_posted'
           AND discord_message_id = $6
         RETURNING bot_key, discord_nonce, revision, update_count, rendered_content",
    )
    .bind(db_id(claimed.scope.channel_id, "channel_id")?)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&claimed.lease_owner)
    .bind(db_id(missing_message_id, "message_id")?)
    .bind(next_revision)
    .bind(next_nonce)
    .bind(LEASE_SECONDS)
    .bind(content)
    .bind(content_hash)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("prepare missing task card replacement: {error}"))?
    .ok_or_else(|| "task card replacement lost its exact message/lease ownership".to_string())?;
    claimed_from_row(
        &claimed.scope,
        claimed.lease_owner.clone(),
        &row,
        ClaimAction::Post,
    )
}

fn db_id(id: u64, field: &str) -> Result<i64, String> {
    i64::try_from(id).map_err(|_| format!("{field} {id} exceeds PostgreSQL BIGINT"))
}

fn message_id(value: Option<i64>) -> Result<u64, String> {
    let value = value.ok_or_else(|| "card_posted row omitted discord_message_id".to_string())?;
    u64::try_from(value).map_err(|_| format!("invalid Discord message id {value}"))
}

fn exact_change(changed: u64, action: &str) -> Result<(), String> {
    (changed == 1)
        .then_some(())
        .ok_or_else(|| format!("{action} changed {changed} rows; exact lease ownership was lost"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryState {
    FooterOnly,
    Posting,
    CardPosted,
}

#[derive(Clone, Debug)]
struct MemoryRow {
    state: MemoryState,
    bot_key: String,
    nonce: String,
    message_id: Option<u64>,
    revision: i32,
    update_count: u64,
    rendered_content: String,
    content_hash: String,
    terminal_delivery_fingerprint: Option<String>,
    lease_owner: Option<String>,
    lease_expires_at: Option<Instant>,
    post_started_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

static MEMORY_STORE: LazyLock<Mutex<HashMap<TaskCardScope, MemoryRow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug)]
struct MemoryTerminalDelivery {
    channel_id: u64,
    provider: String,
    session_key: String,
    event_key: String,
    terminal_delivery_fingerprint: Option<String>,
    message_id: u64,
    bot_key: String,
    content_hash: String,
}

static MEMORY_TERMINAL_DELIVERIES: LazyLock<Mutex<Vec<MemoryTerminalDelivery>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

fn find_terminal_delivery_memory(
    scope: &TaskCardScope,
    content_hash: &str,
) -> Result<Option<(u64, String)>, String> {
    let Some(fingerprint) = scope.terminal_delivery_fingerprint.as_deref() else {
        return Ok(None);
    };
    let mut deliveries = MEMORY_TERMINAL_DELIVERIES
        .lock()
        .map_err(|_| "task card memory terminal ledger poisoned".to_string())?;
    let Some(delivered) = deliveries
        .iter_mut()
        .filter(|delivered| {
            delivered.channel_id == scope.channel_id && delivered.provider == scope.provider
        })
        .find(|delivered| {
            delivered.terminal_delivery_fingerprint.as_deref() == Some(fingerprint)
                || (delivered.terminal_delivery_fingerprint.is_none()
                    && delivered.event_key == scope.event_key
                    && delivered.content_hash == content_hash)
        })
    else {
        return Ok(None);
    };
    delivered.terminal_delivery_fingerprint = Some(fingerprint.to_string());
    let message_id = delivered.message_id;
    let bot_key = delivered.bot_key.clone();
    let delivered_session_key = delivered.session_key.clone();
    let delivered_event_key = delivered.event_key.clone();
    drop(deliveries);

    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    if let Some((_, row)) = rows.iter_mut().find(|(row_scope, row)| {
        row_scope.channel_id == scope.channel_id
            && row_scope.provider == scope.provider
            && row_scope.session_key == delivered_session_key
            && row_scope.event_key == delivered_event_key
            && row.message_id == Some(message_id)
    }) {
        if row.terminal_delivery_fingerprint.is_none() {
            row.terminal_delivery_fingerprint = Some(fingerprint.to_string());
        }
    }
    Ok(Some((message_id, bot_key)))
}

fn record_terminal_delivery_memory(
    claimed: &ClaimedCard,
    message_id: u64,
    content_hash: &str,
) -> Result<(), String> {
    let mut deliveries = MEMORY_TERMINAL_DELIVERIES
        .lock()
        .map_err(|_| "task card memory terminal ledger poisoned".to_string())?;
    if let Some(delivered) = deliveries.iter_mut().find(|delivered| {
        delivered.channel_id == claimed.scope.channel_id
            && delivered.provider == claimed.scope.provider
            && ((claimed.scope.terminal_delivery_fingerprint.is_some()
                && delivered.terminal_delivery_fingerprint
                    == claimed.scope.terminal_delivery_fingerprint)
                || (delivered.session_key == claimed.scope.session_key
                    && delivered.event_key == claimed.scope.event_key
                    && delivered.message_id == message_id))
    }) {
        if delivered.terminal_delivery_fingerprint.is_none() {
            delivered.terminal_delivery_fingerprint =
                claimed.scope.terminal_delivery_fingerprint.clone();
        }
        delivered.bot_key = claimed.bot_key.clone();
        delivered.content_hash = content_hash.to_string();
        delivered.session_key = claimed.scope.session_key.clone();
        delivered.event_key = claimed.scope.event_key.clone();
        delivered.message_id = message_id;
    } else {
        deliveries.push(MemoryTerminalDelivery {
            channel_id: claimed.scope.channel_id,
            provider: claimed.scope.provider.clone(),
            session_key: claimed.scope.session_key.clone(),
            event_key: claimed.scope.event_key.clone(),
            terminal_delivery_fingerprint: claimed.scope.terminal_delivery_fingerprint.clone(),
            message_id,
            bot_key: claimed.bot_key.clone(),
            content_hash: content_hash.to_string(),
        });
    }
    Ok(())
}

fn record_footer_only_memory(
    scope: &TaskCardScope,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    if find_terminal_delivery_memory(scope, content_hash)?.is_some() {
        return Ok(());
    }
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    match rows.get_mut(scope) {
        Some(row) if row.state == MemoryState::FooterOnly => {
            row.rendered_content = content.to_string();
            row.content_hash = content_hash.to_string();
            if row.terminal_delivery_fingerprint.is_none() {
                row.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
            }
        }
        Some(_) => {}
        None => {
            rows.insert(
                scope.clone(),
                MemoryRow {
                    state: MemoryState::FooterOnly,
                    bot_key: String::new(),
                    nonce: stable_nonce(scope, 1),
                    message_id: None,
                    revision: 1,
                    update_count: 1,
                    rendered_content: content.to_string(),
                    content_hash: content_hash.to_string(),
                    terminal_delivery_fingerprint: scope.terminal_delivery_fingerprint.clone(),
                    lease_owner: None,
                    lease_expires_at: None,
                    post_started_at: None,
                    last_error: None,
                },
            );
        }
    }
    Ok(())
}

fn claim_card_memory(
    scope: &TaskCardScope,
    preferred_bot_key: &str,
    seed_content: &str,
    seed_hash: &str,
    intent: StoreIntent,
) -> Result<CardClaim, String> {
    if let Some((message_id, bot_key)) = find_terminal_delivery_memory(scope, seed_hash)? {
        return Ok(CardClaim::Existing {
            message_id,
            bot_key,
        });
    }
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let lease_owner = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    let lease_expires_at = now + Duration::from_secs(LEASE_SECONDS as u64);
    let row = rows.entry(scope.clone()).or_insert_with(|| MemoryRow {
        state: MemoryState::Posting,
        bot_key: preferred_bot_key.to_string(),
        nonce: stable_nonce(scope, 1),
        message_id: None,
        revision: 1,
        update_count: 1,
        rendered_content: seed_content.to_string(),
        content_hash: seed_hash.to_string(),
        terminal_delivery_fingerprint: scope.terminal_delivery_fingerprint.clone(),
        lease_owner: Some(lease_owner.clone()),
        lease_expires_at: Some(lease_expires_at),
        post_started_at: None,
        last_error: None,
    });
    if row.lease_owner.as_deref() == Some(lease_owner.as_str()) {
        return Ok(CardClaim::Owned(memory_claim(
            scope,
            &lease_owner,
            row,
            ClaimAction::Post,
        )));
    }
    let lease_active = row
        .lease_expires_at
        .is_some_and(|expires_at| expires_at > now)
        && row.lease_owner.is_some();
    if lease_active {
        return Ok(CardClaim::Busy {
            bot_key: row.bot_key.clone(),
        });
    }
    let exact_terminal_replay = scope.terminal_delivery_fingerprint.is_some()
        && row.terminal_delivery_fingerprint == scope.terminal_delivery_fingerprint;
    if row.state == MemoryState::CardPosted
        && (intent == StoreIntent::Promotion || exact_terminal_replay)
    {
        return Ok(CardClaim::Existing {
            message_id: row
                .message_id
                .ok_or_else(|| "memory card row omitted message id".to_string())?,
            bot_key: row.bot_key.clone(),
        });
    }
    row.lease_owner = Some(lease_owner.clone());
    row.lease_expires_at = Some(lease_expires_at);
    row.last_error = None;
    let new_terminal_completion = row.state == MemoryState::CardPosted
        && intent == StoreIntent::Observation
        && scope.terminal_delivery_fingerprint.is_some()
        && !exact_terminal_replay;
    let action = if new_terminal_completion {
        row.state = MemoryState::Posting;
        row.message_id = None;
        row.revision = row.revision.saturating_add(1);
        row.update_count = 1;
        row.rendered_content = seed_content.to_string();
        row.content_hash = seed_hash.to_string();
        row.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
        row.nonce = stable_nonce(scope, row.revision);
        row.post_started_at = None;
        ClaimAction::Post
    } else if row.state == MemoryState::CardPosted {
        row.update_count = row.update_count.saturating_add(1);
        ClaimAction::Edit {
            message_id: row
                .message_id
                .ok_or_else(|| "memory card row omitted message id".to_string())?,
        }
    } else {
        let was_footer_only = row.state == MemoryState::FooterOnly;
        row.state = MemoryState::Posting;
        if was_footer_only {
            row.post_started_at = None;
        }
        if row.bot_key.is_empty() {
            row.bot_key = preferred_bot_key.to_string();
            row.nonce = stable_nonce(scope, row.revision);
        }
        if row.terminal_delivery_fingerprint.is_none() {
            row.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
        }
        if intent == StoreIntent::Observation && row.post_started_at.is_none() {
            row.rendered_content = seed_content.to_string();
            row.content_hash = seed_hash.to_string();
        }
        ClaimAction::Post
    };
    let mut claimed = memory_claim(scope, &lease_owner, row, action);
    claimed.new_terminal_completion = new_terminal_completion;
    Ok(CardClaim::Owned(claimed))
}

fn begin_card_post_memory(claimed: &ClaimedCard) -> Result<CardPostAttempt, String> {
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&claimed.scope)
        .ok_or_else(|| "memory task card row disappeared".to_string())?;
    if row.lease_owner.as_deref() != Some(claimed.lease_owner.as_str())
        || row.state != MemoryState::Posting
    {
        return Err("memory task card POST boundary lost exact ownership".to_string());
    }
    if let Some(started_at) = row.post_started_at {
        return Ok(CardPostAttempt {
            started_at,
            resumed: true,
        });
    }
    let started_at = Utc::now();
    row.post_started_at = Some(started_at);
    Ok(CardPostAttempt {
        started_at,
        resumed: false,
    })
}

fn memory_claim(
    scope: &TaskCardScope,
    lease_owner: &str,
    row: &MemoryRow,
    action: ClaimAction,
) -> ClaimedCard {
    let mut claimed_scope = scope.clone();
    claimed_scope.terminal_delivery_fingerprint = row.terminal_delivery_fingerprint.clone();
    ClaimedCard {
        scope: claimed_scope,
        lease_owner: lease_owner.to_string(),
        bot_key: row.bot_key.clone(),
        discord_nonce: row.nonce.clone(),
        revision: row.revision,
        update_count: row.update_count,
        rendered_content: row.rendered_content.clone(),
        action,
        new_terminal_completion: false,
    }
}

fn mark_posted_memory(
    claimed: &ClaimedCard,
    message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    update_owned_memory(claimed, |row| {
        if row.state != MemoryState::Posting {
            return Err("memory task card is not posting".to_string());
        }
        row.state = MemoryState::CardPosted;
        row.message_id = Some(message_id);
        row.rendered_content = content.to_string();
        row.content_hash = content_hash.to_string();
        row.lease_owner = None;
        row.lease_expires_at = None;
        row.post_started_at = None;
        row.last_error = None;
        Ok(())
    })?;
    record_terminal_delivery_memory(claimed, message_id, content_hash)
}

fn mark_edited_memory(
    claimed: &ClaimedCard,
    message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    update_owned_memory(claimed, |row| {
        if row.state != MemoryState::CardPosted || row.message_id != Some(message_id) {
            return Err("memory task card edit lost exact message".to_string());
        }
        row.rendered_content = content.to_string();
        row.content_hash = content_hash.to_string();
        row.lease_owner = None;
        row.lease_expires_at = None;
        row.last_error = None;
        Ok(())
    })
}

fn mark_failure_memory(
    claimed: &ClaimedCard,
    message_id: Option<u64>,
    expected_state: MemoryState,
    error: &str,
) -> Result<(), String> {
    update_owned_memory(claimed, |row| {
        if row.state != expected_state || message_id.is_some_and(|id| row.message_id != Some(id)) {
            return Err("memory task card failure lost exact state/message".to_string());
        }
        row.lease_owner = None;
        row.lease_expires_at = None;
        row.last_error = Some(error.to_string());
        Ok(())
    })
}

fn prepare_replacement_memory(
    claimed: &ClaimedCard,
    missing_message_id: u64,
    content: &str,
    content_hash: &str,
) -> Result<ClaimedCard, String> {
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&claimed.scope)
        .ok_or_else(|| "memory task card row disappeared".to_string())?;
    if row.lease_owner.as_deref() != Some(claimed.lease_owner.as_str())
        || row.state != MemoryState::CardPosted
        || row.message_id != Some(missing_message_id)
    {
        return Err("memory task card replacement lost exact ownership".to_string());
    }
    row.revision = row.revision.saturating_add(1);
    row.nonce = stable_nonce(&claimed.scope, row.revision);
    row.state = MemoryState::Posting;
    row.message_id = None;
    row.post_started_at = None;
    row.rendered_content = content.to_string();
    row.content_hash = content_hash.to_string();
    row.lease_expires_at = Some(Instant::now() + Duration::from_secs(LEASE_SECONDS as u64));
    Ok(memory_claim(
        &claimed.scope,
        &claimed.lease_owner,
        row,
        ClaimAction::Post,
    ))
}

fn update_owned_memory(
    claimed: &ClaimedCard,
    update: impl FnOnce(&mut MemoryRow) -> Result<(), String>,
) -> Result<(), String> {
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&claimed.scope)
        .ok_or_else(|| "memory task card row disappeared".to_string())?;
    if row.lease_owner.as_deref() != Some(claimed.lease_owner.as_str()) {
        return Err("memory task card lease ownership changed".to_string());
    }
    update(row)
}
