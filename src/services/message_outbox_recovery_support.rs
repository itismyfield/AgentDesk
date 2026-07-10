use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, Postgres};

pub(super) const MAX_SIBLINGS: i64 = 20;

#[derive(Clone, Debug, FromRow)]
pub(super) struct OutboxRow {
    pub id: i64,
    pub target: String,
    pub content: String,
    pub bot: String,
    pub source: String,
    pub status: String,
    pub reason_code: Option<String>,
    pub session_key: Option<String>,
    pub retry_count: i64,
    pub error: Option<String>,
    pub dedupe_key: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, FromRow, Serialize)]
pub(crate) struct SiblingState {
    pub id: i64,
    pub status: String,
    pub retry_count: i64,
    pub sent_at: Option<DateTime<Utc>>,
    pub claimed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FailedOutboxInspection {
    pub id: i64,
    pub status: String,
    pub target: String,
    pub bot: String,
    pub source: String,
    pub reason_code: Option<String>,
    pub session_key: Option<String>,
    pub retry_count: i64,
    pub error_snippet: Option<String>,
    pub dedupe_key: Option<String>,
    pub content_snippet: String,
    pub content_hash: String,
    pub created_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub semantic_siblings: Vec<SiblingState>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RedriveOutcome {
    pub id: i64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_outcome: Option<String>,
}

pub(super) fn outcome(
    id: i64,
    name: &str,
    canonical_id: Option<i64>,
    previous_outcome: Option<String>,
) -> RedriveOutcome {
    RedriveOutcome {
        id,
        outcome: name.to_string(),
        canonical_id,
        previous_outcome,
    }
}

pub(super) fn snippet(value: &str, limit: usize) -> String {
    let mut text: String = value.chars().take(limit).collect();
    if value.chars().count() > limit {
        text.push('…');
    }
    text
}

pub(super) fn semantic_key(row: &OutboxRow) -> String {
    row.dedupe_key
        .clone()
        .map(|key| format!("dedupe:{key}"))
        .unwrap_or_else(|| {
            let raw = format!(
                "{}\0{}\0{}\0{}\0{}",
                row.target,
                row.source,
                row.reason_code.as_deref().unwrap_or(""),
                row.session_key.as_deref().unwrap_or(""),
                row.content
            );
            format!("tuple:{}", blake3::hash(raw.as_bytes()).to_hex())
        })
}

pub(super) async fn load_rows<'e, E>(
    executor: E,
    ids: &[i64],
    lock: bool,
) -> Result<Vec<OutboxRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let suffix = if lock { " FOR UPDATE" } else { "" };
    let query = format!(
        "SELECT id,target,content,bot,source,status,reason_code,session_key,retry_count,error,dedupe_key,created_at,sent_at,claimed_at,next_attempt_at FROM message_outbox WHERE id=ANY($1) ORDER BY id{suffix}"
    );
    sqlx::query_as(&query).bind(ids).fetch_all(executor).await
}

pub(super) async fn semantic_siblings<'e, E>(
    executor: E,
    row: &OutboxRow,
) -> Result<Vec<SiblingState>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as("SELECT id,status,retry_count,sent_at,claimed_at FROM message_outbox WHERE id!=$1 AND (($2::TEXT IS NOT NULL AND dedupe_key=$2) OR ($2 IS NULL AND target=$3 AND source=$4 AND reason_code IS NOT DISTINCT FROM $5 AND session_key IS NOT DISTINCT FROM $6 AND content=$7)) ORDER BY id LIMIT $8")
        .bind(row.id)
        .bind(&row.dedupe_key)
        .bind(&row.target)
        .bind(&row.source)
        .bind(&row.reason_code)
        .bind(&row.session_key)
        .bind(&row.content)
        .bind(MAX_SIBLINGS)
        .fetch_all(executor)
        .await
}
