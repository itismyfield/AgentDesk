//! Durable pre-POST journal for task-response chunks (#4055/#4446).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use super::super::ResponseDeliveryClaim;
use super::{db_id, memory_fallback_unavailable, message_id};

const AMBIGUOUS_BACKOFF_SECONDS: i64 = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct PreparedResponseChunk {
    pub(in super::super) chunk_index: usize,
    pub(in super::super) chunk_count: usize,
    pub(in super::super) content_hash: String,
    pub(in super::super) discord_nonce: String,
    pub(in super::super) bot_user_id: u64,
    pub(in super::super) referenced_message_id: Option<u64>,
    pub(in super::super) attempt_started_at: DateTime<Utc>,
    pub(in super::super) post_started_at: Option<DateTime<Utc>>,
    pub(in super::super) next_reconcile_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct ConfirmedResponseChunk {
    pub(in super::super) prepared: PreparedResponseChunk,
    pub(in super::super) discord_message_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) enum ResponseChunkJournal {
    Prepared(PreparedResponseChunk),
    Posting(PreparedResponseChunk),
    Confirmed(ConfirmedResponseChunk),
}

#[derive(Debug, thiserror::Error)]
pub(in super::super) enum ResponseChunkPrepareError {
    #[error("immutable task-response chunk conflict: {0}")]
    Conflict(String),
    #[error("task-response chunk store failure: {0}")]
    Store(String),
}

impl From<String> for ResponseChunkPrepareError {
    fn from(error: String) -> Self {
        Self::Store(error)
    }
}

impl From<&str> for ResponseChunkPrepareError {
    fn from(error: &str) -> Self {
        Self::Store(error.to_string())
    }
}

#[allow(clippy::too_many_arguments)]
pub(in super::super) async fn prepare_response_chunk(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    chunk_index: usize,
    chunk_count: usize,
    content_hash: &str,
    discord_nonce: &str,
    bot_user_id: u64,
    referenced_message_id: Option<u64>,
) -> Result<ResponseChunkJournal, ResponseChunkPrepareError> {
    match pool {
        Some(pool) => {
            prepare_response_chunk_pg(
                pool,
                claim,
                chunk_index,
                chunk_count,
                content_hash,
                discord_nonce,
                bot_user_id,
                referenced_message_id,
            )
            .await
        }
        None if cfg!(any(test, debug_assertions)) => prepare_response_chunk_memory(
            claim,
            chunk_index,
            chunk_count,
            content_hash,
            discord_nonce,
            bot_user_id,
            referenced_message_id,
        ),
        None => Err(ResponseChunkPrepareError::Store(
            memory_fallback_unavailable(),
        )),
    }
}

pub(in super::super) async fn confirm_response_chunk(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    discord_message_id: u64,
) -> Result<(), String> {
    match pool {
        Some(pool) => confirm_response_chunk_pg(pool, claim, prepared, discord_message_id).await,
        None if cfg!(any(test, debug_assertions)) => {
            confirm_response_chunk_memory(claim, prepared, discord_message_id)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn mark_response_chunk_posting(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
) -> Result<PreparedResponseChunk, String> {
    match pool {
        Some(pool) => mark_response_chunk_posting_pg(pool, claim, prepared).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_response_chunk_posting_memory(claim, prepared)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

pub(in super::super) async fn mark_response_chunk_ambiguous(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    reason: &str,
) -> Result<(), String> {
    match pool {
        Some(pool) => mark_response_chunk_ambiguous_pg(pool, claim, prepared, reason).await,
        None if cfg!(any(test, debug_assertions)) => {
            mark_response_chunk_ambiguous_memory(claim, prepared, reason)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_response_chunk_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
    chunk_index: usize,
    chunk_count: usize,
    content_hash: &str,
    discord_nonce: &str,
    bot_user_id: u64,
    referenced_message_id: Option<u64>,
) -> Result<ResponseChunkJournal, ResponseChunkPrepareError> {
    let chunk_index = i32::try_from(chunk_index).map_err(|_| "chunk index exceeds i32")?;
    let chunk_count = i64::try_from(chunk_count).map_err(|_| "chunk count exceeds i64")?;
    let inserted = sqlx::query(
        "INSERT INTO task_notification_response_chunk
             (response_delivery_id, response_generation, chunk_index, chunk_count,
              content_hash, discord_nonce, bot_user_id, referenced_message_id,
              delivery_state)
         SELECT id, response_generation, $8, $9, $10, $11, $12, $13, 'prepared'
         FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5
           AND referenced_card_message_id = $6 AND response_generation = $7
           AND owner_token = $14 AND delivery_state = 'claimed'
         ON CONFLICT DO NOTHING
         RETURNING response_delivery_id",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(claim.response_generation)
    .bind(chunk_index)
    .bind(chunk_count)
    .bind(content_hash)
    .bind(discord_nonce)
    .bind(db_id(bot_user_id, "bot_user_id")?)
    .bind(
        referenced_message_id
            .map(|id| db_id(id, "referenced_message_id"))
            .transpose()?,
    )
    .bind(&claim.owner_token)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("prepare task response chunk journal: {error}"))?;
    if inserted.is_none() {
        let owns = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1 FROM task_notification_response_delivery
                 WHERE channel_id = $1 AND provider = $2 AND session_key = $3
                   AND event_key = $4 AND response_turn_key = $5
                   AND referenced_card_message_id = $6 AND response_generation = $7
                   AND owner_token = $8 AND delivery_state = 'claimed'
             )",
        )
        .bind(db_id(claim.scope.channel_id, "channel_id")?)
        .bind(&claim.scope.provider)
        .bind(&claim.scope.session_key)
        .bind(&claim.scope.event_key)
        .bind(&claim.response_turn_key)
        .bind(db_id(claim.card_message_id, "message_id")?)
        .bind(claim.response_generation)
        .bind(&claim.owner_token)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("verify task response chunk owner: {error}"))?;
        if !owns {
            return Err(ResponseChunkPrepareError::Store(
                "task response chunk preparation lost exact response ownership".into(),
            ));
        }
    }

    let row = sqlx::query(
        "SELECT chunk.chunk_index, chunk.chunk_count, chunk.content_hash,
                chunk.discord_nonce, chunk.bot_user_id, chunk.referenced_message_id,
                chunk.delivery_state, chunk.discord_message_id,
                chunk.attempt_started_at, chunk.post_started_at,
                chunk.next_reconcile_at
         FROM task_notification_response_delivery AS response
         JOIN task_notification_response_chunk AS chunk
           ON chunk.response_delivery_id = response.id
          AND chunk.response_generation = response.response_generation
         WHERE response.channel_id = $1 AND response.provider = $2
           AND response.session_key = $3 AND response.event_key = $4
           AND response.response_turn_key = $5
           AND response.referenced_card_message_id = $6
           AND response.response_generation = $7 AND response.owner_token = $8
           AND response.delivery_state = 'claimed' AND chunk.chunk_index = $9",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(claim.response_generation)
    .bind(&claim.owner_token)
    .bind(chunk_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load task response chunk journal: {error}"))?
    .ok_or_else(|| "task response chunk journal disappeared or owner changed".to_string())?;
    decode_chunk_row(
        &row,
        chunk_count,
        content_hash,
        discord_nonce,
        bot_user_id,
        referenced_message_id,
    )
}

fn decode_chunk_row(
    row: &sqlx::postgres::PgRow,
    expected_chunk_count: i64,
    expected_hash: &str,
    expected_nonce: &str,
    expected_bot_user_id: u64,
    expected_reference: Option<u64>,
) -> Result<ResponseChunkJournal, ResponseChunkPrepareError> {
    let chunk_index: i32 = row.get("chunk_index");
    let chunk_count: i64 = row.get("chunk_count");
    let content_hash: String = row.get("content_hash");
    let discord_nonce: String = row.get("discord_nonce");
    let bot_user_id = u64::try_from(row.get::<i64, _>("bot_user_id"))
        .map_err(|_| "invalid persisted task response bot id")?;
    let referenced_message_id = row
        .get::<Option<i64>, _>("referenced_message_id")
        .map(|id| u64::try_from(id).map_err(|_| "invalid persisted response reference"))
        .transpose()?;
    if chunk_count != expected_chunk_count
        || content_hash != expected_hash
        || discord_nonce != expected_nonce
        || bot_user_id != expected_bot_user_id
        || referenced_message_id != expected_reference
    {
        return Err(ResponseChunkPrepareError::Conflict(
            "payload/bot/reference differs from the durable journal".into(),
        ));
    }
    let prepared = PreparedResponseChunk {
        chunk_index: usize::try_from(chunk_index).map_err(|_| "invalid chunk index")?,
        chunk_count: usize::try_from(chunk_count).map_err(|_| "invalid chunk count")?,
        content_hash,
        discord_nonce,
        bot_user_id,
        referenced_message_id,
        attempt_started_at: row.get("attempt_started_at"),
        post_started_at: row.get("post_started_at"),
        next_reconcile_at: row.get("next_reconcile_at"),
    };
    match row.get::<String, _>("delivery_state").as_str() {
        "prepared" => Ok(ResponseChunkJournal::Prepared(prepared)),
        "posting" => Ok(ResponseChunkJournal::Posting(prepared)),
        "confirmed" => Ok(ResponseChunkJournal::Confirmed(ConfirmedResponseChunk {
            prepared,
            discord_message_id: message_id(row.get("discord_message_id"))?,
        })),
        state => Err(format!("invalid task response chunk state {state}").into()),
    }
}

async fn mark_response_chunk_posting_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
) -> Result<PreparedResponseChunk, String> {
    let post_started_at: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE task_notification_response_chunk AS chunk
         SET delivery_state = 'posting', post_started_at = NOW(), updated_at = NOW()
         FROM task_notification_response_delivery AS response
         WHERE chunk.response_delivery_id = response.id
           AND chunk.response_generation = response.response_generation
           AND response.channel_id = $1 AND response.provider = $2
           AND response.session_key = $3 AND response.event_key = $4
           AND response.response_turn_key = $5
           AND response.referenced_card_message_id = $6
           AND response.response_generation = $7 AND response.owner_token = $8
           AND response.delivery_state = 'claimed' AND chunk.chunk_index = $9
           AND chunk.delivery_state = 'prepared'
         RETURNING chunk.post_started_at",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(claim.response_generation)
    .bind(&claim.owner_token)
    .bind(i32::try_from(prepared.chunk_index).map_err(|_| "chunk index exceeds i32")?)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("mark exact task response chunk posting: {error}"))?
    .ok_or_else(|| "mark exact task response chunk posting changed 0 rows".to_string())?;
    let mut posting = prepared.clone();
    posting.post_started_at = Some(post_started_at);
    Ok(posting)
}

async fn confirm_response_chunk_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    discord_message_id: u64,
) -> Result<(), String> {
    let changed = sqlx::query(
        "UPDATE task_notification_response_chunk AS chunk
         SET delivery_state = 'confirmed', discord_message_id = $10,
             confirmed_at = NOW(), last_reconcile_error = NULL,
             next_reconcile_at = NULL, updated_at = NOW()
         FROM task_notification_response_delivery AS response
         WHERE chunk.response_delivery_id = response.id
           AND chunk.response_generation = response.response_generation
           AND response.channel_id = $1 AND response.provider = $2
           AND response.session_key = $3 AND response.event_key = $4
           AND response.response_turn_key = $5
           AND response.referenced_card_message_id = $6
           AND response.response_generation = $7 AND response.owner_token = $8
           AND response.delivery_state = 'claimed' AND chunk.chunk_index = $9
           AND chunk.delivery_state = 'posting'",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(claim.response_generation)
    .bind(&claim.owner_token)
    .bind(i32::try_from(prepared.chunk_index).map_err(|_| "chunk index exceeds i32")?)
    .bind(db_id(discord_message_id, "discord_message_id")?)
    .execute(pool)
    .await
    .map_err(|error| format!("confirm exact task response chunk: {error}"))?
    .rows_affected();
    (changed == 1)
        .then_some(())
        .ok_or_else(|| format!("confirm exact task response chunk changed {changed} rows"))
}

async fn mark_response_chunk_ambiguous_pg(
    pool: &PgPool,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    reason: &str,
) -> Result<(), String> {
    let changed = sqlx::query(
        "UPDATE task_notification_response_chunk AS chunk
         SET last_reconcile_error = $10,
             next_reconcile_at = NOW() + make_interval(secs => $11),
             alert_count = alert_count + 1, updated_at = NOW()
         FROM task_notification_response_delivery AS response
         WHERE chunk.response_delivery_id = response.id
           AND chunk.response_generation = response.response_generation
           AND response.channel_id = $1 AND response.provider = $2
           AND response.session_key = $3 AND response.event_key = $4
           AND response.response_turn_key = $5
           AND response.referenced_card_message_id = $6
           AND response.response_generation = $7 AND response.owner_token = $8
           AND response.delivery_state = 'claimed' AND chunk.chunk_index = $9
           AND chunk.delivery_state = 'posting'",
    )
    .bind(db_id(claim.scope.channel_id, "channel_id")?)
    .bind(&claim.scope.provider)
    .bind(&claim.scope.session_key)
    .bind(&claim.scope.event_key)
    .bind(&claim.response_turn_key)
    .bind(db_id(claim.card_message_id, "message_id")?)
    .bind(claim.response_generation)
    .bind(&claim.owner_token)
    .bind(i32::try_from(prepared.chunk_index).map_err(|_| "chunk index exceeds i32")?)
    .bind(reason)
    .bind(AMBIGUOUS_BACKOFF_SECONDS)
    .execute(pool)
    .await
    .map_err(|error| format!("record ambiguous task response chunk: {error}"))?
    .rows_affected();
    (changed == 1)
        .then_some(())
        .ok_or_else(|| format!("record ambiguous task response chunk changed {changed} rows"))
}

type MemoryChunkKey = (u64, String, String, String, i32, usize);

#[derive(Clone)]
struct MemoryChunkRow {
    owner_token: String,
    journal: ResponseChunkJournal,
}

static MEMORY_CHUNKS: LazyLock<Mutex<HashMap<MemoryChunkKey, MemoryChunkRow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn memory_key(claim: &ResponseDeliveryClaim, chunk_index: usize) -> MemoryChunkKey {
    (
        claim.scope.channel_id,
        claim.scope.provider.clone(),
        claim.scope.session_key.clone(),
        claim.response_turn_key.clone(),
        claim.response_generation,
        chunk_index,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_response_chunk_memory(
    claim: &ResponseDeliveryClaim,
    chunk_index: usize,
    chunk_count: usize,
    content_hash: &str,
    discord_nonce: &str,
    bot_user_id: u64,
    referenced_message_id: Option<u64>,
) -> Result<ResponseChunkJournal, ResponseChunkPrepareError> {
    let mut rows = MEMORY_CHUNKS
        .lock()
        .map_err(|_| "task response chunk memory store poisoned".to_string())?;
    let row = rows
        .entry(memory_key(claim, chunk_index))
        .or_insert_with(|| MemoryChunkRow {
            owner_token: claim.owner_token.clone(),
            journal: ResponseChunkJournal::Prepared(PreparedResponseChunk {
                chunk_index,
                chunk_count,
                content_hash: content_hash.to_string(),
                discord_nonce: discord_nonce.to_string(),
                bot_user_id,
                referenced_message_id,
                attempt_started_at: Utc::now(),
                post_started_at: None,
                next_reconcile_at: None,
            }),
        });
    if row.owner_token != claim.owner_token {
        // Memory tests model restart takeover by replacing exact ownership.
        row.owner_token = claim.owner_token.clone();
    }
    let prepared = match &row.journal {
        ResponseChunkJournal::Prepared(prepared) => prepared,
        ResponseChunkJournal::Posting(prepared) => prepared,
        ResponseChunkJournal::Confirmed(confirmed) => &confirmed.prepared,
    };
    if prepared.chunk_count != chunk_count
        || prepared.content_hash != content_hash
        || prepared.discord_nonce != discord_nonce
        || prepared.bot_user_id != bot_user_id
        || prepared.referenced_message_id != referenced_message_id
    {
        return Err(ResponseChunkPrepareError::Conflict(
            "payload/bot/reference differs from the durable journal".into(),
        ));
    }
    Ok(row.journal.clone())
}

fn mark_response_chunk_posting_memory(
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
) -> Result<PreparedResponseChunk, String> {
    let mut rows = MEMORY_CHUNKS
        .lock()
        .map_err(|_| "task response chunk memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(claim, prepared.chunk_index))
        .ok_or_else(|| "task response chunk memory journal disappeared".to_string())?;
    if row.owner_token != claim.owner_token
        || !matches!(row.journal, ResponseChunkJournal::Prepared(_))
    {
        return Err("task response chunk memory ownership/state changed".to_string());
    }
    let mut posting = prepared.clone();
    posting.post_started_at = Some(Utc::now());
    row.journal = ResponseChunkJournal::Posting(posting.clone());
    Ok(posting)
}

fn confirm_response_chunk_memory(
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    discord_message_id: u64,
) -> Result<(), String> {
    let mut rows = MEMORY_CHUNKS
        .lock()
        .map_err(|_| "task response chunk memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(claim, prepared.chunk_index))
        .ok_or_else(|| "task response chunk memory journal disappeared".to_string())?;
    if row.owner_token != claim.owner_token
        || !matches!(row.journal, ResponseChunkJournal::Posting(_))
    {
        return Err("task response chunk memory ownership/state changed".to_string());
    }
    row.journal = ResponseChunkJournal::Confirmed(ConfirmedResponseChunk {
        prepared: prepared.clone(),
        discord_message_id,
    });
    Ok(())
}

fn mark_response_chunk_ambiguous_memory(
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    reason: &str,
) -> Result<(), String> {
    let mut rows = MEMORY_CHUNKS
        .lock()
        .map_err(|_| "task response chunk memory store poisoned".to_string())?;
    let row = rows
        .get_mut(&memory_key(claim, prepared.chunk_index))
        .ok_or_else(|| "task response chunk memory journal disappeared".to_string())?;
    if row.owner_token != claim.owner_token {
        return Err("task response chunk memory ownership changed".to_string());
    }
    let ResponseChunkJournal::Posting(current) = &mut row.journal else {
        return Err("only an in-flight response chunk can become ambiguous".to_string());
    };
    let _ = reason;
    current.next_reconcile_at =
        Some(Utc::now() + chrono::Duration::seconds(AMBIGUOUS_BACKOFF_SECONDS));
    Ok(())
}
