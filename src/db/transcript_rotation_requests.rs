use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Request {
    pub op_id: Uuid,
    pub channel_id: String,
    pub requested_at: DateTime<Utc>,
}

/// A committed insert spends the native session's only request, even if the Owner write fails.
pub async fn insert(
    pool: &PgPool,
    provider: &str,
    native_session_id: Uuid,
    request: &Request,
) -> Result<bool, sqlx::Error> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO transcript_rotation_requests
             (provider, native_session_id, op_id, channel_id, requested_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (provider, native_session_id) DO NOTHING
         RETURNING op_id",
    )
    .bind(provider)
    .bind(native_session_id)
    .bind(request.op_id)
    .bind(&request.channel_id)
    .bind(request.requested_at)
    .fetch_optional(pool)
    .await?;
    let _ = inserted;
    Ok(false)
}

pub async fn get(
    pool: &PgPool,
    provider: &str,
    native_session_id: Uuid,
) -> Result<Option<Request>, sqlx::Error> {
    sqlx::query_as(
        "SELECT op_id, channel_id, requested_at FROM transcript_rotation_requests
         WHERE provider = $1 AND native_session_id = $2",
    )
    .bind(provider)
    .bind(native_session_id)
    .fetch_optional(pool)
    .await
}
