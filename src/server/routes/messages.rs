use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, QueryBuilder, Row};

use super::AppState;

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListMessagesQuery {
    #[serde(rename = "receiverId")]
    pub receiver_id: Option<String>,
    #[serde(rename = "receiverType")]
    pub receiver_type: Option<String>,
    #[serde(rename = "messageType")]
    pub message_type: Option<String>,
    pub limit: Option<i64>,
    pub before: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CreateMessageBody {
    pub sender_type: Option<String>,
    pub sender_id: Option<String>,
    pub receiver_type: String,
    pub receiver_id: Option<String>,
    pub discord_target: Option<serde_json::Value>,
    pub content: String,
    pub message_type: Option<String>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/messages
pub async fn list_messages(
    State(state): State<AppState>,
    Query(params): Query<ListMessagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        return match list_messages_pg(pool, &params).await {
            Ok(messages) => (StatusCode::OK, Json(json!({"messages": messages}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let limit = params.limit.unwrap_or(20);

    let mut sql = String::from(
        "SELECT m.id, m.sender_type, m.sender_id, m.receiver_type, m.receiver_id,
                m.content, m.message_type, m.created_at,
                sa.name AS sender_name, sa.name_ko AS sender_name_ko, sa.avatar_emoji AS sender_avatar,
                ra.name AS receiver_name, ra.name_ko AS receiver_name_ko
         FROM messages m
         LEFT JOIN agents sa ON sa.id = m.sender_id
         LEFT JOIN agents ra ON ra.id = m.receiver_id
         WHERE 1=1",
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref receiver_id) = params.receiver_id {
        bind_values.push(receiver_id.clone());
        sql.push_str(&format!(" AND m.receiver_id = ?{}", bind_values.len()));
    }
    if let Some(ref receiver_type) = params.receiver_type {
        bind_values.push(receiver_type.clone());
        sql.push_str(&format!(" AND m.receiver_type = ?{}", bind_values.len()));
    }
    if let Some(ref message_type) = params.message_type {
        bind_values.push(message_type.clone());
        sql.push_str(&format!(" AND m.message_type = ?{}", bind_values.len()));
    }
    if let Some(ref before) = params.before {
        bind_values.push(before.clone());
        sql.push_str(&format!(" AND m.created_at < ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY m.created_at DESC");
    bind_values.push(limit.to_string());
    sql.push_str(&format!(" LIMIT ?{}", bind_values.len()));

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn libsql_rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| message_row_to_json(row))
        .ok();

    let messages: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"messages": messages})))
}

/// POST /api/messages
pub async fn create_message(
    State(state): State<AppState>,
    Json(body): Json<CreateMessageBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let sender_type = body
        .sender_type
        .clone()
        .unwrap_or_else(|| "ceo".to_string());
    let message_type = body
        .message_type
        .clone()
        .unwrap_or_else(|| "chat".to_string());

    if let Some(pool) = state.pg_pool.as_ref() {
        return match create_message_pg(pool, &body, &sender_type, &message_type).await {
            Ok(message) => (StatusCode::CREATED, Json(message)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if let Err(e) = conn.execute(
        "INSERT INTO messages (sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
        libsql_rusqlite::params![
            sender_type,
            body.sender_id,
            body.receiver_type,
            body.receiver_id,
            body.content,
            message_type,
        ],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    let last_id = conn.last_insert_rowid();

    match conn.query_row(
        "SELECT m.id, m.sender_type, m.sender_id, m.receiver_type, m.receiver_id,
                m.content, m.message_type, m.created_at,
                sa.name AS sender_name, sa.name_ko AS sender_name_ko, sa.avatar_emoji AS sender_avatar,
                ra.name AS receiver_name, ra.name_ko AS receiver_name_ko
         FROM messages m
         LEFT JOIN agents sa ON sa.id = m.sender_id
         LEFT JOIN agents ra ON ra.id = m.receiver_id
         WHERE m.id = ?1",
        [last_id],
        |row| message_row_to_json(row),
    ) {
        Ok(msg) => (StatusCode::CREATED, Json(msg)),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────

async fn list_messages_pg(
    pool: &PgPool,
    params: &ListMessagesQuery,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let limit = params.limit.unwrap_or(20);
    let mut query = QueryBuilder::new(
        "SELECT m.id, m.sender_type, m.sender_id, m.receiver_type, m.receiver_id,
                m.content, m.message_type, m.created_at::TEXT AS created_at,
                sa.name AS sender_name, sa.name_ko AS sender_name_ko, sa.avatar_emoji AS sender_avatar,
                ra.name AS receiver_name, ra.name_ko AS receiver_name_ko
         FROM messages m
         LEFT JOIN agents sa ON sa.id = m.sender_id
         LEFT JOIN agents ra ON ra.id = m.receiver_id
         WHERE 1=1",
    );

    if let Some(receiver_id) = params.receiver_id.as_deref() {
        query.push(" AND m.receiver_id = ").push_bind(receiver_id);
    }
    if let Some(receiver_type) = params.receiver_type.as_deref() {
        query
            .push(" AND m.receiver_type = ")
            .push_bind(receiver_type);
    }
    if let Some(message_type) = params.message_type.as_deref() {
        query.push(" AND m.message_type = ").push_bind(message_type);
    }
    if let Some(before) = params.before.as_deref() {
        query.push(" AND m.created_at < ").push_bind(before);
    }

    query
        .push(" ORDER BY m.created_at DESC LIMIT ")
        .push_bind(limit);

    let rows = query.build().fetch_all(pool).await?;
    Ok(rows.into_iter().map(message_row_to_json_pg).collect())
}

async fn create_message_pg(
    pool: &PgPool,
    body: &CreateMessageBody,
    sender_type: &str,
    message_type: &str,
) -> Result<serde_json::Value, sqlx::Error> {
    let message_id: i64 = sqlx::query_scalar(
        "INSERT INTO messages (
            sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW()
         )
         RETURNING id::BIGINT",
    )
    .bind(sender_type)
    .bind(body.sender_id.as_deref())
    .bind(body.receiver_type.as_str())
    .bind(body.receiver_id.as_deref())
    .bind(body.content.as_str())
    .bind(message_type)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        "SELECT m.id, m.sender_type, m.sender_id, m.receiver_type, m.receiver_id,
                m.content, m.message_type, m.created_at::TEXT AS created_at,
                sa.name AS sender_name, sa.name_ko AS sender_name_ko, sa.avatar_emoji AS sender_avatar,
                ra.name AS receiver_name, ra.name_ko AS receiver_name_ko
         FROM messages m
         LEFT JOIN agents sa ON sa.id = m.sender_id
         LEFT JOIN agents ra ON ra.id = m.receiver_id
         WHERE m.id = $1",
    )
    .bind(message_id)
    .fetch_one(pool)
    .await
    .map(|row| message_row_to_json_pg(row))
}

fn message_row_to_json(row: &libsql_rusqlite::Row) -> libsql_rusqlite::Result<serde_json::Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "sender_type": row.get::<_, Option<String>>(1)?,
        "sender_id": row.get::<_, Option<String>>(2)?,
        "receiver_type": row.get::<_, Option<String>>(3)?,
        "receiver_id": row.get::<_, Option<String>>(4)?,
        "content": row.get::<_, Option<String>>(5)?,
        "message_type": row.get::<_, Option<String>>(6)?,
        "created_at": row.get::<_, Option<String>>(7)?,
        "sender_name": row.get::<_, Option<String>>(8)?,
        "sender_name_ko": row.get::<_, Option<String>>(9)?,
        "sender_avatar": row.get::<_, Option<String>>(10)?,
        "receiver_name": row.get::<_, Option<String>>(11)?,
        "receiver_name_ko": row.get::<_, Option<String>>(12)?,
    }))
}

fn message_row_to_json_pg(row: sqlx::postgres::PgRow) -> serde_json::Value {
    json!({
        "id": row.get::<i64, _>("id"),
        "sender_type": row.get::<Option<String>, _>("sender_type"),
        "sender_id": row.get::<Option<String>, _>("sender_id"),
        "receiver_type": row.get::<Option<String>, _>("receiver_type"),
        "receiver_id": row.get::<Option<String>, _>("receiver_id"),
        "content": row.get::<Option<String>, _>("content"),
        "message_type": row.get::<Option<String>, _>("message_type"),
        "created_at": row.get::<Option<String>, _>("created_at"),
        "sender_name": row.get::<Option<String>, _>("sender_name"),
        "sender_name_ko": row.get::<Option<String>, _>("sender_name_ko"),
        "sender_avatar": row.get::<Option<String>, _>("sender_avatar"),
        "receiver_name": row.get::<Option<String>, _>("receiver_name"),
        "receiver_name_ko": row.get::<Option<String>, _>("receiver_name_ko"),
    })
}
