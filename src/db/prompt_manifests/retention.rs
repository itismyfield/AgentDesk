use std::sync::OnceLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::config::PromptManifestRetentionConfig;

/// Process-wide retention config snapshot used at write time. Set by bootstrap
/// (`crate::bootstrap`) to mirror `Config::prompt_manifest_retention`.
/// `save_prompt_manifest_pg` reads this and applies per-layer byte caps so
/// every persistence call site benefits without threading the config through
/// every caller. When unset (e.g. tests), no global cap is applied — but
/// caller-supplied `from_content_with_retention` still works.
static PROMPT_MANIFEST_RETENTION_CONFIG: OnceLock<PromptManifestRetentionConfig> = OnceLock::new();

pub const PROMPT_MANIFEST_RETENTION_CONFIG_APPLIED_AT: &str = "boot";
pub const PROMPT_MANIFEST_RETENTION_CONFIG_SOURCE: &str = "agentdesk.yaml boot snapshot";

/// Install the process-wide retention config snapshot. Called once from
/// `crate::bootstrap` after `Config` is parsed. Subsequent calls are ignored
/// (the OnceLock is set-once); restart is required to change retention bounds.
pub fn install_retention_config(config: PromptManifestRetentionConfig) {
    let _ = PROMPT_MANIFEST_RETENTION_CONFIG.set(config);
}

pub(super) fn current_retention_config() -> Option<&'static PromptManifestRetentionConfig> {
    PROMPT_MANIFEST_RETENTION_CONFIG.get()
}

/// Outcome of a single retention pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptManifestRetentionReport {
    pub dry_run: bool,
    /// Rows whose `full_content` was set to NULL (or would be, in dry-run).
    pub trimmed_full_content: i64,
    /// Cutoff used for trimming.
    pub horizon_at: Option<DateTime<Utc>>,
}

/// Apply the retention policy: rows with `created_at < now() - retention_days`
/// have their `full_content` trimmed to NULL. `content_sha256` and metadata are
/// preserved. No-op when `enabled = false` or `retention_days = 0`.
pub async fn apply_retention_policy(
    pool: &PgPool,
    config: &PromptManifestRetentionConfig,
    dry_run: bool,
) -> Result<PromptManifestRetentionReport> {
    let mut report = PromptManifestRetentionReport {
        dry_run,
        ..Default::default()
    };
    if !config.enabled || config.full_content_days == 0 {
        return Ok(report);
    }
    let horizon = horizon_for(config);
    report.horizon_at = Some(horizon);

    if dry_run {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM prompt_manifest_layers AS l
             JOIN prompt_manifests AS m ON m.id = l.manifest_id
             WHERE l.full_content IS NOT NULL AND m.created_at < $1",
        )
        .bind(horizon)
        .fetch_one(pool)
        .await?;
        report.trimmed_full_content = count;
        return Ok(report);
    }

    // Mark trimmed rows as `is_truncated = TRUE` so observers can distinguish
    // "never had full content" from "trimmed by retention". Hash + metadata
    // remain intact.
    let result = sqlx::query(
        "UPDATE prompt_manifest_layers AS l
            SET full_content = NULL,
                is_truncated = TRUE
          FROM prompt_manifests AS m
         WHERE m.id = l.manifest_id
           AND l.full_content IS NOT NULL
           AND m.created_at < $1",
    )
    .bind(horizon)
    .execute(pool)
    .await?;

    report.trimmed_full_content = i64::try_from(result.rows_affected()).unwrap_or(i64::MAX);
    Ok(report)
}

pub(super) fn horizon_for(config: &PromptManifestRetentionConfig) -> DateTime<Utc> {
    let days = i64::from(config.full_content_days);
    Utc::now() - chrono::Duration::days(days)
}
