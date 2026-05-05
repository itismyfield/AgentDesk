use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::config::PromptManifestRetentionConfig;

use super::retention::{
    PROMPT_MANIFEST_RETENTION_CONFIG_APPLIED_AT, PROMPT_MANIFEST_RETENTION_CONFIG_SOURCE,
    horizon_for,
};

/// Aggregate storage cost for prompt manifests, surfaced on the dashboard via
/// `GET /api/prompt-manifest/retention`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptManifestStorageStats {
    /// Sum of stored bytes across `full_content` + `redacted_preview` for all
    /// rows that still carry a body. Excludes rows whose bodies have been
    /// trimmed by the retention sweeper.
    pub total_stored_bytes: i64,
    /// Sum of `original_bytes` across all layers, falling back to retained
    /// UTF-8 body bytes and then `chars` only for pre-#1699 bodyless rows.
    /// Reflects the audit-true content size.
    pub total_original_bytes: i64,
    /// Number of layer rows currently flagged `is_truncated`.
    pub truncated_count: i64,
    /// Total number of manifest rows.
    pub manifest_count: i64,
    /// Total number of layer rows.
    pub layer_count: i64,
    /// Created-at of the oldest row that still carries `full_content`. None
    /// when no rows currently retain full content.
    pub oldest_full_content_at: Option<DateTime<Utc>>,
    /// `now() - retention_days`. Layer bodies older than this are eligible for
    /// trim by the sweeper. Surfaced so the dashboard can render the policy.
    pub retention_horizon_at: Option<DateTime<Utc>>,
    /// Effective retention config snapshot.
    pub retention_days: u32,
    pub per_layer_max_bytes_adk_provided: u64,
    pub per_layer_max_bytes_user_derived: u64,
    pub enabled: bool,
    /// Retention bounds are installed into a process-wide OnceLock at startup;
    /// runtime config edits do not change write-time caps until restart.
    pub restart_required_for_config_changes: bool,
    /// Human-readable point in the process lifecycle when this config is
    /// captured. Kept flat in the API response for dashboard consumers.
    pub config_applied_at: String,
    /// Source and semantics of the surfaced retention config.
    pub config_source: String,
    /// Explicit negative capability so clients do not infer hot reload support.
    pub hot_reload: bool,
}

pub async fn manifest_storage_stats(
    pool: &PgPool,
    config: &PromptManifestRetentionConfig,
) -> Result<PromptManifestStorageStats> {
    let row = sqlx::query(
        "SELECT
            COALESCE(SUM(
                COALESCE(OCTET_LENGTH(full_content), 0)
              + COALESCE(OCTET_LENGTH(redacted_preview), 0)
            ), 0)::BIGINT AS total_stored_bytes,
            COALESCE(SUM(
                COALESCE(
                    original_bytes,
                    OCTET_LENGTH(full_content),
                    OCTET_LENGTH(redacted_preview),
                    chars
                )
            ), 0)::BIGINT AS total_original_bytes,
            COUNT(*) FILTER (WHERE is_truncated)::BIGINT AS truncated_count,
            COUNT(*)::BIGINT AS layer_count
         FROM prompt_manifest_layers",
    )
    .fetch_one(pool)
    .await?;

    let total_stored_bytes: i64 = row.try_get("total_stored_bytes").unwrap_or(0);
    let total_original_bytes: i64 = row.try_get("total_original_bytes").unwrap_or(0);
    let truncated_count: i64 = row.try_get("truncated_count").unwrap_or(0);
    let layer_count: i64 = row.try_get("layer_count").unwrap_or(0);

    let manifest_count: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM prompt_manifests")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    let oldest_full_content_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MIN(m.created_at) FROM prompt_manifest_layers AS l
            JOIN prompt_manifests AS m ON m.id = l.manifest_id
           WHERE l.full_content IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    let retention_horizon_at = if config.enabled && config.full_content_days > 0 {
        Some(horizon_for(config))
    } else {
        None
    };

    Ok(PromptManifestStorageStats {
        total_stored_bytes,
        total_original_bytes,
        truncated_count,
        manifest_count,
        layer_count,
        oldest_full_content_at,
        retention_horizon_at,
        retention_days: config.full_content_days,
        per_layer_max_bytes_adk_provided: config.per_layer_max_bytes_adk_provided,
        per_layer_max_bytes_user_derived: config.per_layer_max_bytes_user_derived,
        enabled: config.enabled,
        restart_required_for_config_changes: true,
        config_applied_at: PROMPT_MANIFEST_RETENTION_CONFIG_APPLIED_AT.to_string(),
        config_source: PROMPT_MANIFEST_RETENTION_CONFIG_SOURCE.to_string(),
        hot_reload: false,
    })
}
