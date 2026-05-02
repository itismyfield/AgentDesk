use std::collections::HashSet;
use std::fmt::Write;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::services::discord::formatting::redact_sensitive_for_placeholder;

pub const RECOVERY_AUDIT_SOURCE_DISCORD_RECENT: &str = "discord_recent";
const RECOVERY_AUDIT_PREVIEW_CHARS_PER_MESSAGE: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryAuditDraft {
    pub channel_id: String,
    pub session_key: Option<String>,
    pub source: String,
    pub full_context: String,
    pub max_chars_per_message: i32,
}

impl RecoveryAuditDraft {
    pub fn discord_recent(
        channel_id: impl Into<String>,
        session_key: Option<String>,
        full_context: impl Into<String>,
        max_chars_per_message: usize,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            session_key,
            source: RECOVERY_AUDIT_SOURCE_DISCORD_RECENT.to_string(),
            full_context: full_context.into(),
            max_chars_per_message: max_chars_per_message.min(i32::MAX as usize) as i32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedRecoveryAuditRecord {
    pub channel_id: String,
    pub session_key: Option<String>,
    pub source: String,
    pub message_count: i32,
    pub max_chars_per_message: i32,
    pub authors: Vec<String>,
    pub redacted_preview: String,
    pub content_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryAuditRecord {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub channel_id: String,
    pub session_key: Option<String>,
    pub source: String,
    pub message_count: i32,
    pub max_chars_per_message: i32,
    pub authors: Vec<String>,
    pub redacted_preview: String,
    pub content_sha256: String,
    pub consumed_by_turn_id: Option<String>,
}

pub fn recovery_context_sha256(full_context: &str) -> String {
    let digest = Sha256::digest(full_context.trim().as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub fn build_recovery_audit_record(draft: &RecoveryAuditDraft) -> MaterializedRecoveryAuditRecord {
    let full_context = draft.full_context.trim();
    let mut seen_authors = HashSet::new();
    let mut authors = Vec::new();
    let mut preview_lines = Vec::new();
    let mut message_count = 0_i32;

    for line in full_context
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        message_count += 1;
        let (author, _) = split_author_and_body(line);
        if seen_authors.insert(author.to_string()) {
            authors.push(author.to_string());
        }

        let redacted = redact_sensitive_for_placeholder(line);
        preview_lines.push(take_chars(
            redacted.trim(),
            RECOVERY_AUDIT_PREVIEW_CHARS_PER_MESSAGE,
        ));
    }

    MaterializedRecoveryAuditRecord {
        channel_id: draft.channel_id.clone(),
        session_key: draft.session_key.clone(),
        source: draft.source.clone(),
        message_count,
        max_chars_per_message: draft.max_chars_per_message,
        authors,
        redacted_preview: preview_lines.join("\n"),
        content_sha256: recovery_context_sha256(full_context),
    }
}

pub async fn insert_recovery_audit_record(
    pool: &PgPool,
    draft: RecoveryAuditDraft,
) -> Result<RecoveryAuditRecord> {
    let materialized = build_recovery_audit_record(&draft);
    let authors_json = json!(materialized.authors);

    let row = sqlx::query(
        "INSERT INTO recovery_audit_records (
            channel_id,
            session_key,
            source,
            message_count,
            max_chars_per_message,
            authors_json,
            redacted_preview,
            content_sha256
         ) VALUES ($1, $2, $3, $4, $5, CAST($6 AS jsonb), $7, $8)
         RETURNING id,
                   created_at,
                   channel_id,
                   session_key,
                   source,
                   message_count,
                   max_chars_per_message,
                   authors_json,
                   redacted_preview,
                   content_sha256,
                   consumed_by_turn_id",
    )
    .bind(&materialized.channel_id)
    .bind(&materialized.session_key)
    .bind(&materialized.source)
    .bind(materialized.message_count)
    .bind(materialized.max_chars_per_message)
    .bind(authors_json.to_string())
    .bind(&materialized.redacted_preview)
    .bind(&materialized.content_sha256)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("insert recovery_audit_records: {error}"))?;

    decode_recovery_audit_record(row)
}

pub async fn mark_recovery_audit_consumed(
    pool: &PgPool,
    channel_id: impl AsRef<str>,
    turn_id: impl AsRef<str>,
) -> Result<Option<RecoveryAuditRecord>> {
    let channel_id = channel_id.as_ref().trim();
    let turn_id = turn_id.as_ref().trim();
    if channel_id.is_empty() || turn_id.is_empty() {
        return Ok(None);
    }

    let row = sqlx::query(
        "WITH next_record AS (
             SELECT id
             FROM recovery_audit_records
             WHERE channel_id = $1
               AND consumed_by_turn_id IS NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1
         )
         UPDATE recovery_audit_records records
         SET consumed_by_turn_id = $2
         FROM next_record
         WHERE records.id = next_record.id
         RETURNING records.id,
                   records.created_at,
                   records.channel_id,
                   records.session_key,
                   records.source,
                   records.message_count,
                   records.max_chars_per_message,
                   records.authors_json,
                   records.redacted_preview,
                   records.content_sha256,
                   records.consumed_by_turn_id",
    )
    .bind(channel_id)
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("mark recovery_audit_records consumed: {error}"))?;

    row.map(decode_recovery_audit_record).transpose()
}

pub async fn fetch_recovery_audit(
    pool: &PgPool,
    channel_id: impl AsRef<str>,
    limit: i64,
) -> Result<Vec<RecoveryAuditRecord>> {
    let channel_id = channel_id.as_ref().trim();
    if channel_id.is_empty() || limit <= 0 {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT id,
                created_at,
                channel_id,
                session_key,
                source,
                message_count,
                max_chars_per_message,
                authors_json,
                redacted_preview,
                content_sha256,
                consumed_by_turn_id
         FROM recovery_audit_records
         WHERE channel_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT $2",
    )
    .bind(channel_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("fetch recovery_audit_records: {error}"))?;

    rows.into_iter().map(decode_recovery_audit_record).collect()
}

pub async fn fetch_recovery_audit_for_turn(
    pool: &PgPool,
    turn_id: impl AsRef<str>,
) -> Result<Option<RecoveryAuditRecord>> {
    let turn_id = turn_id.as_ref().trim();
    if turn_id.is_empty() {
        return Ok(None);
    }

    let row = sqlx::query(
        "SELECT id,
                created_at,
                channel_id,
                session_key,
                source,
                message_count,
                max_chars_per_message,
                authors_json,
                redacted_preview,
                content_sha256,
                consumed_by_turn_id
         FROM recovery_audit_records
         WHERE consumed_by_turn_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("fetch recovery_audit_records for turn: {error}"))?;

    row.map(decode_recovery_audit_record).transpose()
}

fn decode_recovery_audit_record(row: PgRow) -> Result<RecoveryAuditRecord> {
    let authors_json = row
        .try_get::<Value, _>("authors_json")
        .map_err(|error| anyhow!("decode recovery audit authors_json: {error}"))?;

    Ok(RecoveryAuditRecord {
        id: row
            .try_get::<i64, _>("id")
            .map_err(|error| anyhow!("decode recovery audit id: {error}"))?,
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(|error| anyhow!("decode recovery audit created_at: {error}"))?,
        channel_id: row
            .try_get::<String, _>("channel_id")
            .map_err(|error| anyhow!("decode recovery audit channel_id: {error}"))?,
        session_key: row
            .try_get::<Option<String>, _>("session_key")
            .map_err(|error| anyhow!("decode recovery audit session_key: {error}"))?,
        source: row
            .try_get::<String, _>("source")
            .map_err(|error| anyhow!("decode recovery audit source: {error}"))?,
        message_count: row
            .try_get::<i32, _>("message_count")
            .map_err(|error| anyhow!("decode recovery audit message_count: {error}"))?,
        max_chars_per_message: row
            .try_get::<i32, _>("max_chars_per_message")
            .map_err(|error| anyhow!("decode recovery audit max_chars_per_message: {error}"))?,
        authors: decode_authors(authors_json),
        redacted_preview: row
            .try_get::<String, _>("redacted_preview")
            .map_err(|error| anyhow!("decode recovery audit redacted_preview: {error}"))?,
        content_sha256: row
            .try_get::<String, _>("content_sha256")
            .map_err(|error| anyhow!("decode recovery audit content_sha256: {error}"))?,
        consumed_by_turn_id: row
            .try_get::<Option<String>, _>("consumed_by_turn_id")
            .map_err(|error| anyhow!("decode recovery audit consumed_by_turn_id: {error}"))?,
    })
}

fn decode_authors(value: Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn split_author_and_body(line: &str) -> (&str, &str) {
    let Some((author, body)) = line.split_once(':') else {
        return ("unknown", line);
    };
    let author = author.trim();
    if author.is_empty() {
        ("unknown", body.trim())
    } else {
        (author, body.trim())
    }
}

fn take_chars(input: &str, limit: usize) -> String {
    input.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn try_create() -> Option<Self> {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name =
                format!("agentdesk_recovery_audit_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            if let Err(error) = crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "recovery_audit tests",
            )
            .await
            {
                eprintln!("[recovery_audit tests] skipping: {error}");
                return None;
            }

            Some(Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect_and_migrate(&self) -> Result<PgPool> {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "recovery_audit tests",
            )
            .await
            .map_err(|error| anyhow!("{error}"))
        }

        async fn drop(self) {
            if let Err(error) = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "recovery_audit tests",
            )
            .await
            {
                eprintln!("[recovery_audit tests] drop failed: {error}");
            }
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[test]
    fn build_recovery_audit_record_redacts_and_hashes_full_body() {
        let full_context = "\nAlice: email me at alice@example.com token=secret\nBob: Authorization: Bearer live-token\n";
        let draft = RecoveryAuditDraft::discord_recent(
            "42",
            Some("session-1".to_string()),
            full_context,
            300,
        );

        let record = build_recovery_audit_record(&draft);

        assert_eq!(record.channel_id, "42");
        assert_eq!(record.session_key.as_deref(), Some("session-1"));
        assert_eq!(record.source, RECOVERY_AUDIT_SOURCE_DISCORD_RECENT);
        assert_eq!(record.message_count, 2);
        assert_eq!(record.max_chars_per_message, 300);
        assert_eq!(record.authors, vec!["Alice", "Bob"]);
        assert_eq!(
            record.content_sha256,
            recovery_context_sha256(full_context.trim())
        );
        assert!(record.redacted_preview.contains("***@***"));
        assert!(record.redacted_preview.contains("token=***"));
        assert!(record.redacted_preview.contains("Bearer ***"));
        assert!(!record.redacted_preview.contains("alice@example.com"));
        assert!(!record.redacted_preview.contains("secret"));
        assert!(!record.redacted_preview.contains("live-token"));
    }

    #[tokio::test]
    async fn recovery_audit_round_trips_and_fetches_by_turn() -> Result<()> {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return Ok(());
        };
        let pool = pg_db.connect_and_migrate().await?;

        let full_context = "Alice: alice@example.com\nBob: Bearer token-value";
        let inserted = insert_recovery_audit_record(
            &pool,
            RecoveryAuditDraft::discord_recent(
                "42",
                Some("agentdesk-session".to_string()),
                full_context,
                300,
            ),
        )
        .await?;

        assert_eq!(inserted.message_count, 2);
        assert_eq!(inserted.authors, vec!["Alice", "Bob"]);
        assert_eq!(
            inserted.content_sha256,
            recovery_context_sha256(full_context)
        );
        assert!(inserted.redacted_preview.contains("***@***"));
        assert!(inserted.redacted_preview.contains("Bearer ***"));
        assert!(!inserted.redacted_preview.contains("alice@example.com"));
        assert!(!inserted.redacted_preview.contains("token-value"));

        let stamped = mark_recovery_audit_consumed(&pool, "42", "discord:42:420")
            .await?
            .expect("unconsumed record should be stamped");
        assert_eq!(
            stamped.consumed_by_turn_id.as_deref(),
            Some("discord:42:420")
        );

        let by_turn = fetch_recovery_audit_for_turn(&pool, "discord:42:420")
            .await?
            .expect("record should be fetchable by turn");
        assert_eq!(by_turn.id, inserted.id);
        assert_eq!(by_turn.channel_id, "42");

        let by_channel = fetch_recovery_audit(&pool, "42", 10).await?;
        assert_eq!(by_channel.len(), 1);
        assert_eq!(by_channel[0].id, inserted.id);

        pool.close().await;
        pg_db.drop().await;
        Ok(())
    }
}
