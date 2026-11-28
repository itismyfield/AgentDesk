//! Decision requests as an agent asked them: the immutable question, its options and the hash of
//! the requester's capability. One idempotency key names one request per requester scope.

use sqlx::PgPool;
use uuid::Uuid;

/// One option of a request, in the order it was asked.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct OptionRow {
    pub label: String,
    pub key: Option<String>,
    pub detail: String,
    pub recommendation_percent: Option<i16>,
}

/// What an ask stores. The capability is only ever its hash here.
pub struct NewRequest<'a> {
    pub agent_id: &'a str,
    pub logical_channel_id: u64,
    pub target_channel_id: u64,
    pub request_ref: &'a str,
    pub idempotency_key: &'a str,
    pub payload_sha256: &'a str,
    pub capability_sha256: &'a str,
    pub title: &'a str,
    pub context: &'a str,
    pub options: &'a [OptionRow],
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    Created(Uuid),
    /// The same key, payload and capability asked before: that request.
    Same(Uuid),
    /// The key is held by another payload or another capability.
    Conflict,
}

/// Stores the request and its options in one transaction, once per idempotency key.
pub async fn create_once(pool: &PgPool, new: &NewRequest<'_>) -> Result<Stored, sqlx::Error> {
    let request_id = Uuid::new_v4();
    let (logical, target) = (
        new.logical_channel_id.to_string(),
        new.target_channel_id.to_string(),
    );
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO decision_requests (request_id, requester_agent_id, logical_channel_id,
             target_channel_id, request_ref, idempotency_key, payload_sha256, capability_sha256,
             title, context)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (requester_agent_id, logical_channel_id, idempotency_key) DO NOTHING",
    )
    .bind(request_id)
    .bind(new.agent_id)
    .bind(&logical)
    .bind(&target)
    .bind(new.request_ref)
    .bind(new.idempotency_key)
    .bind(new.payload_sha256)
    .bind(new.capability_sha256)
    .bind(new.title)
    .bind(new.context)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await?;
        let held: Option<(Uuid, String, String)> = sqlx::query_as(
            "SELECT request_id, payload_sha256, capability_sha256 FROM decision_requests
             WHERE requester_agent_id = $1 AND logical_channel_id = $2 AND idempotency_key = $3",
        )
        .bind(new.agent_id)
        .bind(&logical)
        .bind(new.idempotency_key)
        .fetch_optional(pool)
        .await?;
        return Ok(match held {
            Some((id, payload, capability))
                if payload == new.payload_sha256 && capability == new.capability_sha256 =>
            {
                Stored::Same(id)
            }
            _ => Stored::Conflict,
        });
    }
    for (index, option) in new.options.iter().enumerate() {
        sqlx::query(
            "INSERT INTO decision_options (request_id, ordinal, label, option_key, detail,
                 recommendation_percent)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(request_id)
        .bind(index as i16 + 1)
        .bind(&option.label)
        .bind(&option.key)
        .bind(&option.detail)
        .bind(option.recommendation_percent)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Stored::Created(request_id))
}

/// A stored request as its requester may read it: no capability and no hash of one.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RequestRow {
    pub request_id: Uuid,
    pub agent_id: String,
    pub logical_channel_id: String,
    pub target_channel_id: String,
    #[serde(rename = "ref")]
    pub request_ref: String,
    pub title: String,
    pub context: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub options: Vec<OptionRow>,
}

/// The request only when `capability_sha256` is the hash it was asked under; a request of
/// another requester reads exactly like one that does not exist.
pub async fn load_owned(
    pool: &PgPool,
    request_id: Uuid,
    capability_sha256: &str,
) -> Result<Option<RequestRow>, sqlx::Error> {
    type Head = (
        String,
        String,
        String,
        String,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
    );
    let head: Option<Head> = sqlx::query_as(
        "SELECT requester_agent_id, logical_channel_id, target_channel_id, request_ref, title,
                context, created_at
         FROM decision_requests WHERE request_id = $1 AND capability_sha256 = $2",
    )
    .bind(request_id)
    .bind(capability_sha256)
    .fetch_optional(pool)
    .await?;
    let Some((agent_id, logical_channel_id, target_channel_id, request_ref, title, context, at)) =
        head
    else {
        return Ok(None);
    };
    let options: Vec<(String, Option<String>, String, Option<i16>)> = sqlx::query_as(
        "SELECT label, option_key, detail, recommendation_percent FROM decision_options
         WHERE request_id = $1 ORDER BY ordinal",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await?;
    let option = |(label, key, detail, recommendation_percent)| OptionRow {
        label,
        key,
        detail,
        recommendation_percent,
    };
    Ok(Some(RequestRow {
        request_id,
        agent_id,
        logical_channel_id,
        target_channel_id,
        request_ref,
        title,
        context,
        created_at: at,
        options: options.into_iter().map(option).collect(),
    }))
}
