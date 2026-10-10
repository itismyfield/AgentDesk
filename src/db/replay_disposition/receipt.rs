//! Typed reads of replay receipts for admission, retry and restore decisions.

use sqlx::PgPool;

/// A receipt's lifecycle; a spelling this build does not know is kept and blocks a rerun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    RegisteredNotStarted,
    StartedUnclassified,
    StartupFailedNoEffect,
    ClassifiedNormal,
    Withheld,
    Unknown(String),
}

impl From<String> for Disposition {
    fn from(value: String) -> Self {
        match value.as_str() {
            "registered_not_started" => Self::RegisteredNotStarted,
            "started_unclassified" => Self::StartedUnclassified,
            "startup_failed_no_effect" => Self::StartupFailedNoEffect,
            "classified_normal" => Self::ClassifiedNormal,
            "withheld" => Self::Withheld,
            _ => Self::Unknown(value),
        }
    }
}

/// One receipt as stored: its full source set, current attempt and preserved request.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(crate) struct ReceiptSnapshot {
    pub(crate) id: i64,
    pub(crate) sources: Vec<String>,
    #[sqlx(try_from = "String")]
    pub(crate) disposition: Disposition,
    pub(crate) episode_nonce: Option<String>,
    pub(crate) request_key: Option<String>,
    pub(crate) request_hash: Option<String>,
    pub(crate) original_text: String,
    pub(crate) preserved: Option<serde_json::Value>,
}

/// Every receipt of `provider` in `channel` that names any of `sources`, or that `key` names.
pub(crate) async fn receipts_for_request(
    pool: &PgPool,
    provider: &str,
    channel: &str,
    sources: &[String],
    key: Option<&str>,
) -> Result<Vec<ReceiptSnapshot>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, replay_source_message_ids AS sources, replay_disposition AS disposition,
                replay_episode_nonce AS episode_nonce, replay_request_key AS request_key,
                replay_request_hash AS request_hash, user_text AS original_text,
                replay_preserved AS preserved
           FROM intake_outbox
          WHERE replay_disposition IS NOT NULL AND (replay_request_key = $4
             OR (lower(btrim(provider)) = lower(btrim($1)) AND channel_id = $2
                 AND replay_source_message_ids && $3::TEXT[]))
          ORDER BY id",
    )
    .bind(provider)
    .bind(channel)
    .bind(sources)
    .bind(key)
    .fetch_all(pool)
    .await
}
