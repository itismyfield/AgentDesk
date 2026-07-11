//! Durable card-before-response fence state (#4055).

use sqlx::PgPool;

use super::super::TaskCardScope;
use super::{MEMORY_STORE, MemoryState, db_id, exact_change, memory_fallback_unavailable};

pub(in super::super) async fn bind_response_turn(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    response_turn_key: &str,
    card_message_id: u64,
) -> Result<(), String> {
    match pool {
        Some(pool) => bind_response_turn_pg(pool, scope, response_turn_key, card_message_id).await,
        None if cfg!(any(test, debug_assertions)) => {
            bind_response_turn_memory(scope, response_turn_key, card_message_id)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn response_fallback_must_wait(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: Option<&str>,
    response_turn_key: Option<&str>,
) -> Result<bool, String> {
    match pool {
        Some(pool) => {
            response_fallback_must_wait_pg(
                pool,
                channel_id,
                provider,
                session_key,
                event_key,
                response_turn_key,
            )
            .await
        }
        None if cfg!(any(test, debug_assertions)) => response_fallback_must_wait_memory(
            channel_id,
            provider,
            session_key,
            event_key,
            response_turn_key,
        ),
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn mark_response_delivered(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    card_message_id: u64,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_response_delivered_pg(pool, scope, card_message_id).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_response_delivered_memory(scope, card_message_id)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

async fn bind_response_turn_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    response_turn_key: &str,
    card_message_id: u64,
) -> Result<(), String> {
    if response_turn_key.len() != 64 {
        return Err("task response turn key must be a 64-character fingerprint".to_string());
    }
    let changed = sqlx::query(
        "UPDATE task_notification_card_state
         SET response_turn_key = $6, updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND delivery_state = 'card_posted' AND discord_message_id = $5
           AND (response_turn_key IS NULL OR response_turn_key = $6)",
    )
    .bind(db_id(scope.channel_id, "channel_id")?)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(db_id(card_message_id, "message_id")?)
    .bind(response_turn_key)
    .execute(pool)
    .await
    .map_err(|error| format!("bind task response turn to confirmed card: {error}"))?
    .rows_affected();
    exact_change(changed, "bind task response turn to confirmed card")
}

async fn response_fallback_must_wait_pg(
    pool: &PgPool,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: Option<&str>,
    response_turn_key: Option<&str>,
) -> Result<bool, String> {
    if event_key.is_none() && response_turn_key.is_none() {
        return Ok(true);
    }
    let must_wait = sqlx::query_scalar::<_, bool>(
        "SELECT response_delivered_at IS NULL
         FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND (($4::TEXT IS NOT NULL AND event_key = $4)
             OR ($4::TEXT IS NULL AND $5::TEXT IS NOT NULL AND response_turn_key = $5))",
    )
    .bind(db_id(channel_id, "channel_id")?)
    .bind(provider.trim().to_ascii_lowercase())
    .bind(session_key)
    .bind(event_key)
    .bind(response_turn_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load durable task response fallback fence: {error}"))?;
    Ok(must_wait.unwrap_or(true))
}

async fn mark_response_delivered_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    card_message_id: u64,
) -> Result<(), String> {
    let card_message_id = db_id(card_message_id, "message_id")?;
    let changed = sqlx::query(
        "UPDATE task_notification_card_state
         SET response_delivered_at = COALESCE(response_delivered_at, NOW()),
             response_card_message_id = COALESCE(response_card_message_id, $5),
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND delivery_state = 'card_posted' AND discord_message_id = $5
           AND (response_card_message_id IS NULL OR response_card_message_id = $5)",
    )
    .bind(db_id(scope.channel_id, "channel_id")?)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(card_message_id)
    .execute(pool)
    .await
    .map_err(|error| format!("commit durable task response delivery: {error}"))?
    .rows_affected();
    exact_change(changed, "commit durable task response delivery")
}

fn bind_response_turn_memory(
    scope: &TaskCardScope,
    response_turn_key: &str,
    card_message_id: u64,
) -> Result<(), String> {
    if response_turn_key.len() != 64 {
        return Err("task response turn key must be a 64-character fingerprint".to_string());
    }
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    if rows.iter().any(|(other_scope, row)| {
        other_scope != scope
            && other_scope.channel_id == scope.channel_id
            && other_scope.provider == scope.provider
            && other_scope.session_key == scope.session_key
            && row.response_turn_key.as_deref() == Some(response_turn_key)
    }) {
        return Err("task response turn is already bound to another event".to_string());
    }
    let row = rows
        .get_mut(scope)
        .ok_or_else(|| "memory task card row disappeared".to_string())?;
    if row.state != MemoryState::CardPosted || row.message_id != Some(card_message_id) {
        return Err("memory task response turn lost its confirmed card".to_string());
    }
    if row
        .response_turn_key
        .as_deref()
        .is_some_and(|key| key != response_turn_key)
    {
        return Err("memory task response turn binding changed".to_string());
    }
    row.response_turn_key = Some(response_turn_key.to_string());
    Ok(())
}

fn response_fallback_must_wait_memory(
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: Option<&str>,
    response_turn_key: Option<&str>,
) -> Result<bool, String> {
    let rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let provider = provider.trim().to_ascii_lowercase();
    let row = rows.iter().find_map(|(scope, row)| {
        let exact_scope = scope.channel_id == channel_id
            && scope.provider == provider
            && scope.session_key == session_key;
        let matches = match event_key {
            Some(event_key) => exact_scope && scope.event_key == event_key,
            None => {
                exact_scope
                    && response_turn_key.is_some()
                    && row.response_turn_key.as_deref() == response_turn_key
            }
        };
        matches.then_some(row)
    });
    Ok(row.is_none_or(|row| row.response_card_message_id.is_none()))
}

fn mark_response_delivered_memory(
    scope: &TaskCardScope,
    card_message_id: u64,
) -> Result<(), String> {
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let row = rows
        .get_mut(scope)
        .ok_or_else(|| "memory task card row disappeared".to_string())?;
    if row.state != MemoryState::CardPosted || row.message_id != Some(card_message_id) {
        return Err("memory task response delivery lost its confirmed card".to_string());
    }
    if row
        .response_card_message_id
        .is_some_and(|message_id| message_id != card_message_id)
    {
        return Err("memory task response delivery changed card identity".to_string());
    }
    row.response_card_message_id = Some(card_message_id);
    Ok(())
}
