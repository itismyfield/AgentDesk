use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

use std::sync::OnceLock;

use crate::config::{PromptManifestRetentionConfig, PromptManifestVisibilityKind};

const USER_DERIVED_PREVIEW_CHARS: usize = 240;

/// Marker appended to truncated `full_content` so transcript readers can detect
/// the truncation client-side without reading the schema.
const TRUNCATION_MARKER: &str = "...[truncated by retention policy]";

/// Process-wide retention config snapshot used at write time. Set by bootstrap
/// (`crate::bootstrap`) to mirror `Config::prompt_manifest_retention`.
/// `save_prompt_manifest_pg` reads this and applies per-layer byte caps so
/// every persistence call site benefits without threading the config through
/// every caller. When unset (e.g. tests), no global cap is applied — but
/// caller-supplied `from_content_with_retention` still works.
static PROMPT_MANIFEST_RETENTION_CONFIG: OnceLock<PromptManifestRetentionConfig> = OnceLock::new();

/// Install the process-wide retention config snapshot. Called once from
/// `crate::bootstrap` after `Config` is parsed. Subsequent calls are ignored
/// (the OnceLock is set-once); restart is required to change retention bounds.
pub fn install_retention_config(config: PromptManifestRetentionConfig) {
    let _ = PROMPT_MANIFEST_RETENTION_CONFIG.set(config);
}

fn current_retention_config() -> Option<&'static PromptManifestRetentionConfig> {
    PROMPT_MANIFEST_RETENTION_CONFIG.get()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptContentVisibility {
    AdkProvided,
    UserDerived,
}

impl PromptContentVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdkProvided => "adk_provided",
            Self::UserDerived => "user_derived",
        }
    }

    pub fn kind(self) -> PromptManifestVisibilityKind {
        match self {
            Self::AdkProvided => PromptManifestVisibilityKind::AdkProvided,
            Self::UserDerived => PromptManifestVisibilityKind::UserDerived,
        }
    }
}

impl TryFrom<&str> for PromptContentVisibility {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value.trim() {
            "adk_provided" => Ok(Self::AdkProvided),
            "user_derived" => Ok(Self::UserDerived),
            other => Err(anyhow!("unknown prompt content visibility: {other}")),
        }
    }
}

impl std::fmt::Display for PromptContentVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptManifest {
    pub id: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
    pub turn_id: String,
    pub channel_id: String,
    pub dispatch_id: Option<String>,
    pub profile: Option<String>,
    pub total_input_tokens_est: i64,
    pub layer_count: i64,
    pub layers: Vec<PromptManifestLayer>,
}

impl PromptManifest {
    pub fn recompute_totals(&mut self) {
        self.total_input_tokens_est = self
            .layers
            .iter()
            .fold(0_i64, |sum, layer| sum.saturating_add(layer.tokens_est));
        self.layer_count = usize_to_i64(self.layers.len());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptManifestLayer {
    pub id: Option<i64>,
    pub manifest_id: Option<i64>,
    pub layer_name: String,
    pub enabled: bool,
    pub source: Option<String>,
    pub reason: Option<String>,
    pub chars: i64,
    pub tokens_est: i64,
    pub content_sha256: String,
    pub content_visibility: PromptContentVisibility,
    pub full_content: Option<String>,
    pub redacted_preview: Option<String>,
    /// True when stored body was clipped at write time by the per-layer cap.
    /// `content_sha256` always reflects the *original* content, never the
    /// truncated body.
    #[serde(default)]
    pub is_truncated: bool,
    /// Byte length of the original content. Lets the dashboard report the true
    /// storage cost of a manifest even after retention trim.
    #[serde(default)]
    pub original_bytes: Option<i64>,
}

impl PromptManifestLayer {
    pub fn from_content(
        layer_name: impl Into<String>,
        enabled: bool,
        source: Option<impl Into<String>>,
        reason: Option<impl Into<String>>,
        content_visibility: PromptContentVisibility,
        content: impl Into<String>,
    ) -> Self {
        Self::from_content_with_retention(
            layer_name,
            enabled,
            source,
            reason,
            content_visibility,
            content,
            None,
        )
    }

    /// Variant of [`Self::from_content`] that consults a retention config to
    /// apply per-layer write-time truncation. The `content_sha256` is always
    /// computed against the *original* content so audit hashes survive trims.
    pub fn from_content_with_retention(
        layer_name: impl Into<String>,
        enabled: bool,
        source: Option<impl Into<String>>,
        reason: Option<impl Into<String>>,
        content_visibility: PromptContentVisibility,
        content: impl Into<String>,
        retention: Option<&PromptManifestRetentionConfig>,
    ) -> Self {
        let content = content.into();
        let chars = usize_to_i64(content.chars().count());
        let tokens_est = estimate_tokens_from_chars_i64(chars);
        let content_sha256 = sha256_hex(&content);
        let original_bytes = Some(usize_to_i64(content.len()));

        let cap = retention
            .filter(|cfg| cfg.enabled)
            .and_then(|cfg| cfg.cap_for(content_visibility.kind()));

        let (full_content, redacted_preview, is_truncated) = match content_visibility {
            PromptContentVisibility::AdkProvided => {
                let (body, truncated) = apply_byte_cap(content, cap);
                (Some(body), None, truncated)
            }
            PromptContentVisibility::UserDerived => {
                let preview = redacted_preview(&content);
                // The redacted preview is already clamped by character count;
                // additionally clamp it by byte cap so a wide-char preview can't
                // blow past the user-derived budget.
                let (preview, truncated) = match preview {
                    Some(text) => {
                        let (body, t) = apply_byte_cap(text, cap);
                        (Some(body), t)
                    }
                    None => (None, false),
                };
                (None, preview, truncated)
            }
        };

        Self {
            id: None,
            manifest_id: None,
            layer_name: layer_name.into(),
            enabled,
            source: normalized_opt_owned(source.map(Into::into)),
            reason: normalized_opt_owned(reason.map(Into::into)),
            chars,
            tokens_est,
            content_sha256,
            content_visibility,
            full_content,
            redacted_preview,
            is_truncated,
            original_bytes,
        }
    }
}

/// If `cap` is `Some(n)` and `body.len() > n`, return a UTF-8-safe prefix that
/// fits within `n - marker_len` bytes plus the truncation marker, and `true`.
/// Otherwise return the body unchanged and `false`.
fn apply_byte_cap(body: String, cap: Option<usize>) -> (String, bool) {
    let Some(cap) = cap else {
        return (body, false);
    };
    if body.len() <= cap {
        return (body, false);
    }
    let marker = TRUNCATION_MARKER;
    // Reserve room for the marker; if the cap is smaller than the marker
    // itself, just emit a marker-only body so the row still hashes the
    // original content but stores something legible.
    if cap <= marker.len() {
        return (marker.to_string(), true);
    }
    let budget = cap - marker.len();
    let mut end = budget;
    // Walk back to the nearest UTF-8 char boundary.
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + marker.len());
    out.push_str(&body[..end]);
    out.push_str(marker);
    (out, true)
}

#[derive(Debug, Clone)]
pub struct PromptManifestBuilder {
    turn_id: String,
    channel_id: String,
    dispatch_id: Option<String>,
    profile: Option<String>,
    layers: Vec<PromptManifestLayer>,
}

impl PromptManifestBuilder {
    pub fn new(turn_id: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            channel_id: channel_id.into(),
            dispatch_id: None,
            profile: None,
            layers: Vec::new(),
        }
    }

    pub fn dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = normalized_opt_owned(Some(dispatch_id.into()));
        self
    }

    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = normalized_opt_owned(Some(profile.into()));
        self
    }

    pub fn layer(mut self, layer: PromptManifestLayer) -> Self {
        self.layers.push(layer);
        self
    }

    pub fn content_layer(
        self,
        layer_name: impl Into<String>,
        enabled: bool,
        source: Option<impl Into<String>>,
        reason: Option<impl Into<String>>,
        content_visibility: PromptContentVisibility,
        content: impl Into<String>,
    ) -> Self {
        self.layer(PromptManifestLayer::from_content(
            layer_name,
            enabled,
            source,
            reason,
            content_visibility,
            content,
        ))
    }

    pub fn build(self) -> Result<PromptManifest> {
        let turn_id = self.turn_id.trim().to_string();
        if turn_id.is_empty() {
            return Err(anyhow!("prompt manifest turn_id is required"));
        }
        let channel_id = self.channel_id.trim().to_string();
        if channel_id.is_empty() {
            return Err(anyhow!("prompt manifest channel_id is required"));
        }
        let mut manifest = PromptManifest {
            id: None,
            created_at: None,
            turn_id,
            channel_id,
            dispatch_id: self.dispatch_id,
            profile: self.profile,
            total_input_tokens_est: 0,
            layer_count: 0,
            layers: self.layers,
        };
        manifest.recompute_totals();
        Ok(manifest)
    }
}

pub fn estimate_tokens_from_chars(chars: usize) -> i64 {
    usize_to_i64(chars / 4)
}

pub async fn save_prompt_manifest(
    pg_pool: Option<&PgPool>,
    manifest: &PromptManifest,
) -> Result<Option<i64>> {
    let Some(pool) = pg_pool else {
        return Ok(None);
    };
    save_prompt_manifest_pg(pool, manifest).await.map(Some)
}

pub fn spawn_save_prompt_manifest(pg_pool: Option<PgPool>, manifest: PromptManifest) {
    let Some(pool) = pg_pool else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = save_prompt_manifest(Some(&pool), &manifest).await {
            tracing::warn!("[prompt-manifest] save failed: {error}");
        }
    });
}

pub async fn fetch_prompt_manifest(
    pg_pool: Option<&PgPool>,
    turn_id: &str,
) -> Result<Option<PromptManifest>> {
    let Some(pool) = pg_pool else {
        return Ok(None);
    };
    fetch_prompt_manifest_pg(pool, turn_id).await
}

async fn save_prompt_manifest_pg(pool: &PgPool, manifest: &PromptManifest) -> Result<i64> {
    let turn_id = manifest.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("prompt manifest turn_id is required"));
    }
    let channel_id = manifest.channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!("prompt manifest channel_id is required"));
    }

    let mut tx = pool.begin().await?;
    let total_input_tokens_est = manifest
        .layers
        .iter()
        .fold(0_i64, |sum, layer| sum.saturating_add(layer.tokens_est));
    let layer_count = usize_to_i64(manifest.layers.len());

    let manifest_id: i64 = sqlx::query_scalar(
        "INSERT INTO prompt_manifests (
            turn_id,
            channel_id,
            dispatch_id,
            profile,
            total_input_tokens_est,
            layer_count
         ) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (turn_id) DO UPDATE SET
            created_at = NOW(),
            channel_id = EXCLUDED.channel_id,
            dispatch_id = EXCLUDED.dispatch_id,
            profile = EXCLUDED.profile,
            total_input_tokens_est = EXCLUDED.total_input_tokens_est,
            layer_count = EXCLUDED.layer_count
         RETURNING id",
    )
    .bind(turn_id)
    .bind(channel_id)
    .bind(normalized_opt(manifest.dispatch_id.as_deref()))
    .bind(normalized_opt(manifest.profile.as_deref()))
    .bind(total_input_tokens_est)
    .bind(layer_count)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM prompt_manifest_layers WHERE manifest_id = $1")
        .bind(manifest_id)
        .execute(&mut *tx)
        .await?;

    let retention_for_write = current_retention_config();
    for layer in &manifest.layers {
        let layer_name = layer.layer_name.trim();
        if layer_name.is_empty() {
            return Err(anyhow!("prompt manifest layer_name is required"));
        }

        // Apply the per-layer byte cap at write-time using the process-wide
        // retention config. This catches layers built via the legacy
        // `from_content` (no retention) and call sites that mutate
        // `full_content` after construction. `content_sha256` is preserved
        // (we never recompute the hash on the truncated body).
        let cap = retention_for_write
            .filter(|cfg| cfg.enabled)
            .and_then(|cfg| cfg.cap_for(layer.content_visibility.kind()));
        let mut full_content_for_write = layer.full_content.clone();
        let mut redacted_preview_for_write = layer.redacted_preview.clone();
        let mut is_truncated_for_write = layer.is_truncated;
        let mut original_bytes_for_write = layer.original_bytes;
        if let Some(cap) = cap {
            match layer.content_visibility {
                PromptContentVisibility::AdkProvided => {
                    if let Some(body) = full_content_for_write.take() {
                        let original_len = body.len();
                        if original_bytes_for_write.is_none() {
                            original_bytes_for_write = Some(usize_to_i64(original_len));
                        }
                        let (capped, truncated) = apply_byte_cap(body, Some(cap));
                        if truncated {
                            is_truncated_for_write = true;
                        }
                        full_content_for_write = Some(capped);
                    }
                }
                PromptContentVisibility::UserDerived => {
                    if let Some(preview) = redacted_preview_for_write.take() {
                        if original_bytes_for_write.is_none() {
                            original_bytes_for_write = Some(usize_to_i64(preview.len()));
                        }
                        let (capped, truncated) = apply_byte_cap(preview, Some(cap));
                        if truncated {
                            is_truncated_for_write = true;
                        }
                        redacted_preview_for_write = Some(capped);
                    }
                }
            }
        }

        sqlx::query(
            "INSERT INTO prompt_manifest_layers (
                manifest_id,
                layer_name,
                enabled,
                source,
                reason,
                chars,
                tokens_est,
                content_sha256,
                content_visibility,
                full_content,
                redacted_preview,
                is_truncated,
                original_bytes
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(manifest_id)
        .bind(layer_name)
        .bind(layer.enabled)
        .bind(normalized_opt(layer.source.as_deref()))
        .bind(normalized_opt(layer.reason.as_deref()))
        .bind(layer.chars)
        .bind(layer.tokens_est)
        .bind(&layer.content_sha256)
        .bind(layer.content_visibility.as_str())
        .bind(full_content_for_write.as_deref())
        .bind(redacted_preview_for_write.as_deref())
        .bind(is_truncated_for_write)
        .bind(original_bytes_for_write)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(manifest_id)
}

async fn fetch_prompt_manifest_pg(pool: &PgPool, turn_id: &str) -> Result<Option<PromptManifest>> {
    let turn_id = turn_id.trim();
    if turn_id.is_empty() {
        return Ok(None);
    }

    let manifest = sqlx::query(
        "SELECT
            id,
            created_at,
            turn_id,
            channel_id,
            dispatch_id,
            profile,
            total_input_tokens_est,
            layer_count
         FROM prompt_manifests
         WHERE turn_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = manifest else {
        return Ok(None);
    };
    let manifest_id: i64 = row.try_get("id")?;
    let layers = fetch_prompt_manifest_layers_pg(pool, manifest_id).await?;

    Ok(Some(PromptManifest {
        id: Some(manifest_id),
        created_at: row.try_get("created_at")?,
        turn_id: row.try_get("turn_id")?,
        channel_id: row.try_get("channel_id")?,
        dispatch_id: row.try_get("dispatch_id")?,
        profile: row.try_get("profile")?,
        total_input_tokens_est: row.try_get("total_input_tokens_est")?,
        layer_count: row.try_get("layer_count")?,
        layers,
    }))
}

async fn fetch_prompt_manifest_layers_pg(
    pool: &PgPool,
    manifest_id: i64,
) -> Result<Vec<PromptManifestLayer>> {
    let rows = sqlx::query(
        "SELECT
            id,
            manifest_id,
            layer_name,
            enabled,
            source,
            reason,
            chars,
            tokens_est,
            content_sha256,
            content_visibility,
            full_content,
            redacted_preview,
            is_truncated,
            original_bytes
         FROM prompt_manifest_layers
         WHERE manifest_id = $1
         ORDER BY id ASC",
    )
    .bind(manifest_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let visibility_raw: String = row.try_get("content_visibility")?;
            Ok(PromptManifestLayer {
                id: Some(row.try_get("id")?),
                manifest_id: Some(row.try_get("manifest_id")?),
                layer_name: row.try_get("layer_name")?,
                enabled: row.try_get("enabled")?,
                source: row.try_get("source")?,
                reason: row.try_get("reason")?,
                chars: row.try_get("chars")?,
                tokens_est: row.try_get("tokens_est")?,
                content_sha256: row.try_get("content_sha256")?,
                content_visibility: PromptContentVisibility::try_from(visibility_raw.as_str())?,
                full_content: row.try_get("full_content")?,
                redacted_preview: row.try_get("redacted_preview")?,
                is_truncated: row.try_get("is_truncated").unwrap_or(false),
                original_bytes: row.try_get("original_bytes").ok().flatten(),
            })
        })
        .collect()
}

fn estimate_tokens_from_chars_i64(chars: i64) -> i64 {
    if chars <= 0 { 0 } else { chars / 4 }
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn redacted_preview(content: &str) -> Option<String> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    Some(truncate_chars(content, USER_DERIVED_PREVIEW_CHARS))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    if value.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalized_opt_owned(value: Option<String>) -> Option<String> {
    normalized_opt(value.as_deref())
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// Retention / storage-stats surface (#1699)
// ---------------------------------------------------------------------------

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

/// Aggregate dashboard stats for the prompt-manifest retention surface.
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
    })
}

fn horizon_for(config: &PromptManifestRetentionConfig) -> DateTime<Utc> {
    let days = i64::from(config.full_content_days);
    Utc::now() - chrono::Duration::days(days)
}

#[cfg(test)]
mod tests {
    use super::{
        PromptContentVisibility, PromptManifestBuilder, estimate_tokens_from_chars,
        fetch_prompt_manifest, save_prompt_manifest,
    };

    struct PromptManifestPgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl PromptManifestPgDatabase {
        async fn create() -> Option<Self> {
            let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
                .ok()
                .filter(|value| !value.trim().is_empty())?;
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let base = base.trim().trim_end_matches('/').to_string();
            let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "postgres".to_string());
            let admin_url = format!("{base}/{admin_db}");
            let database_name = format!(
                "agentdesk_prompt_manifest_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{base}/{database_name}");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "prompt manifest pg",
            )
            .await
            .expect("create prompt manifest postgres test db");
            Some(Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "prompt manifest pg",
            )
            .await
            .expect("connect + migrate prompt manifest postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "prompt manifest pg",
            )
            .await
            .expect("drop prompt manifest postgres test db");
        }
    }

    #[test]
    fn prompt_manifest_token_estimate_is_chars_div_four() {
        assert_eq!(estimate_tokens_from_chars(0), 0);
        assert_eq!(estimate_tokens_from_chars(3), 0);
        assert_eq!(estimate_tokens_from_chars(4), 1);
        assert_eq!(estimate_tokens_from_chars(17), 4);
    }

    #[test]
    fn prompt_manifest_builder_separates_content_visibility() {
        let manifest = PromptManifestBuilder::new(" turn-1 ", " channel-1 ")
            .dispatch_id(" dispatch-1 ")
            .profile(" project-agentdesk ")
            .content_layer(
                "system",
                true,
                Some("prompt_builder"),
                Some("base prompt"),
                PromptContentVisibility::AdkProvided,
                "abcd",
            )
            .content_layer(
                "user",
                true,
                Some("discord"),
                Some("user message"),
                PromptContentVisibility::UserDerived,
                "sensitive user content",
            )
            .build()
            .expect("build manifest");

        assert_eq!(manifest.turn_id, "turn-1");
        assert_eq!(manifest.channel_id, "channel-1");
        assert_eq!(manifest.dispatch_id.as_deref(), Some("dispatch-1"));
        assert_eq!(manifest.profile.as_deref(), Some("project-agentdesk"));
        assert_eq!(manifest.layer_count, 2);
        assert_eq!(manifest.total_input_tokens_est, 6);
        assert_eq!(
            manifest.layers[0].content_visibility,
            PromptContentVisibility::AdkProvided
        );
        assert_eq!(manifest.layers[0].full_content.as_deref(), Some("abcd"));
        assert!(manifest.layers[0].redacted_preview.is_none());
        assert_eq!(
            manifest.layers[1].content_visibility,
            PromptContentVisibility::UserDerived
        );
        assert!(manifest.layers[1].full_content.is_none());
        assert_eq!(
            manifest.layers[1].redacted_preview.as_deref(),
            Some("sensitive user content")
        );
    }

    #[tokio::test]
    async fn prompt_manifest_save_fetch_round_trip_pg() {
        let Some(test_db) = PromptManifestPgDatabase::create().await else {
            eprintln!("skipping prompt_manifest_save_fetch_round_trip_pg: postgres unavailable");
            return;
        };
        let pool = test_db.migrate().await;

        let manifest = PromptManifestBuilder::new("turn-round-trip", "1499610614904131594")
            .dispatch_id("dispatch-1")
            .profile("project-agentdesk")
            .content_layer(
                "system",
                true,
                Some("prompt_builder"),
                Some("authoritative instructions"),
                PromptContentVisibility::AdkProvided,
                "system prompt content",
            )
            .content_layer(
                "user",
                true,
                Some("discord"),
                Some("latest user message"),
                PromptContentVisibility::UserDerived,
                "user supplied prompt content",
            )
            .build()
            .expect("build manifest");

        let manifest_id = save_prompt_manifest(Some(&pool), &manifest)
            .await
            .expect("save manifest")
            .expect("manifest id");
        let fetched = fetch_prompt_manifest(Some(&pool), "turn-round-trip")
            .await
            .expect("fetch manifest")
            .expect("manifest");

        assert_eq!(fetched.id, Some(manifest_id));
        assert_eq!(fetched.turn_id, "turn-round-trip");
        assert_eq!(fetched.channel_id, "1499610614904131594");
        assert_eq!(fetched.dispatch_id.as_deref(), Some("dispatch-1"));
        assert_eq!(fetched.profile.as_deref(), Some("project-agentdesk"));
        assert_eq!(fetched.layer_count, 2);
        assert_eq!(
            fetched.total_input_tokens_est,
            manifest.total_input_tokens_est
        );
        assert_eq!(fetched.layers.len(), 2);
        assert_eq!(
            fetched.layers[0].content_visibility,
            PromptContentVisibility::AdkProvided
        );
        assert!(fetched.layers[0].full_content.is_some());
        assert!(fetched.layers[0].redacted_preview.is_none());
        assert_eq!(
            fetched.layers[1].content_visibility,
            PromptContentVisibility::UserDerived
        );
        assert!(fetched.layers[1].full_content.is_none());
        assert!(fetched.layers[1].redacted_preview.is_some());

        crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
            .await
            .expect("close test pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn manifest_storage_stats_counts_utf8_bytes_not_chars_pg() {
        let Some(test_db) = PromptManifestPgDatabase::create().await else {
            eprintln!(
                "skipping manifest_storage_stats_counts_utf8_bytes_not_chars_pg: postgres unavailable"
            );
            return;
        };
        let pool = test_db.migrate().await;

        let system_text = "한글🙂";
        let user_text = "요청🙂한글";
        let expected_stored_bytes = (system_text.len() + user_text.len()) as i64;
        let expected_chars = (system_text.chars().count() + user_text.chars().count()) as i64;
        assert!(
            expected_stored_bytes > expected_chars,
            "multibyte fixture must distinguish UTF-8 bytes from characters"
        );

        let manifest = PromptManifestBuilder::new("turn-utf8-storage-stats", "1499610614904131594")
            .content_layer(
                "system",
                true,
                Some("prompt_builder"),
                Some("utf8 system content"),
                PromptContentVisibility::AdkProvided,
                system_text,
            )
            .content_layer(
                "user",
                true,
                Some("discord"),
                Some("utf8 user content"),
                PromptContentVisibility::UserDerived,
                user_text,
            )
            .build()
            .expect("build manifest");

        save_prompt_manifest(Some(&pool), &manifest)
            .await
            .expect("save manifest");

        let cfg = crate::config::PromptManifestRetentionConfig {
            enabled: true,
            full_content_days: 30,
            per_layer_max_bytes_adk_provided: 0,
            per_layer_max_bytes_user_derived: 0,
        };
        let stats = super::manifest_storage_stats(&pool, &cfg)
            .await
            .expect("stats");

        assert_eq!(stats.layer_count, 2);
        assert_eq!(stats.total_stored_bytes, expected_stored_bytes);
        assert_eq!(stats.total_original_bytes, expected_stored_bytes);
        assert_ne!(stats.total_stored_bytes, expected_chars);

        crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
            .await
            .expect("close test pool");
        test_db.drop().await;
    }

    #[test]
    fn prompt_manifest_layer_truncates_adk_provided_at_byte_cap() {
        let cfg = crate::config::PromptManifestRetentionConfig {
            enabled: true,
            full_content_days: 30,
            // 64-byte cap for adk_provided.
            per_layer_max_bytes_adk_provided: 64,
            per_layer_max_bytes_user_derived: 0,
        };
        let body = "A".repeat(2_048);
        let layer = super::PromptManifestLayer::from_content_with_retention(
            "system",
            true,
            Some("prompt_builder"),
            Some("base"),
            PromptContentVisibility::AdkProvided,
            body.clone(),
            Some(&cfg),
        );

        assert!(layer.is_truncated, "layer should be flagged truncated");
        assert_eq!(
            layer.original_bytes,
            Some(body.len() as i64),
            "original_bytes must reflect the pre-truncation size"
        );
        let stored = layer.full_content.as_deref().unwrap();
        assert!(stored.len() <= 64, "stored body must fit byte cap");
        assert!(stored.ends_with("[truncated by retention policy]"));

        // Hash MUST match the original (full) content, never the truncated body.
        let expected_hash = super::sha256_hex(&body);
        assert_eq!(layer.content_sha256, expected_hash);
    }

    #[test]
    fn prompt_manifest_layer_truncation_disabled_when_config_disabled() {
        let cfg = crate::config::PromptManifestRetentionConfig {
            enabled: false,
            full_content_days: 30,
            per_layer_max_bytes_adk_provided: 8,
            per_layer_max_bytes_user_derived: 8,
        };
        let body = "A".repeat(1_024);
        let layer = super::PromptManifestLayer::from_content_with_retention(
            "system",
            true,
            None::<&str>,
            None::<&str>,
            PromptContentVisibility::AdkProvided,
            body.clone(),
            Some(&cfg),
        );
        assert!(!layer.is_truncated);
        assert_eq!(layer.full_content.as_deref(), Some(body.as_str()));
    }

    #[test]
    fn prompt_manifest_layer_zero_cap_disables_truncation_for_visibility() {
        let cfg = crate::config::PromptManifestRetentionConfig {
            enabled: true,
            full_content_days: 30,
            per_layer_max_bytes_adk_provided: 0, // disabled
            per_layer_max_bytes_user_derived: 0,
        };
        let body = "A".repeat(1_024);
        let layer = super::PromptManifestLayer::from_content_with_retention(
            "system",
            true,
            None::<&str>,
            None::<&str>,
            PromptContentVisibility::AdkProvided,
            body.clone(),
            Some(&cfg),
        );
        assert!(!layer.is_truncated);
        assert_eq!(layer.full_content.as_deref(), Some(body.as_str()));
    }

    #[test]
    fn prompt_manifest_layer_truncation_handles_utf8_boundary() {
        let cfg = crate::config::PromptManifestRetentionConfig {
            enabled: true,
            full_content_days: 30,
            per_layer_max_bytes_adk_provided: 64,
            per_layer_max_bytes_user_derived: 0,
        };
        // Mix of multi-byte chars to ensure we never split a codepoint.
        let body: String = std::iter::repeat("한").take(200).collect();
        let layer = super::PromptManifestLayer::from_content_with_retention(
            "system",
            true,
            None::<&str>,
            None::<&str>,
            PromptContentVisibility::AdkProvided,
            body.clone(),
            Some(&cfg),
        );
        let stored = layer.full_content.as_deref().unwrap();
        // Must be valid UTF-8 (would panic on `as_str()` otherwise).
        let _: &str = stored;
        assert!(layer.is_truncated);
        assert!(stored.len() <= 64);
    }

    #[tokio::test]
    async fn prompt_manifest_save_applies_write_time_cap_via_global_pg() {
        use sqlx::Row;
        let Some(test_db) = PromptManifestPgDatabase::create().await else {
            eprintln!(
                "skipping prompt_manifest_save_applies_write_time_cap_via_global_pg: postgres unavailable"
            );
            return;
        };
        let pool = test_db.migrate().await;

        // Install a tight global cap so the next save trips it.
        super::install_retention_config(crate::config::PromptManifestRetentionConfig {
            enabled: true,
            full_content_days: 30,
            per_layer_max_bytes_adk_provided: 64,
            per_layer_max_bytes_user_derived: 0,
        });

        let big_body = "B".repeat(8_192);
        let manifest = PromptManifestBuilder::new("turn-write-cap", "1499610614904131594")
            .content_layer(
                "system",
                true,
                Some("prompt_builder"),
                Some("base"),
                PromptContentVisibility::AdkProvided,
                big_body.clone(),
            )
            .build()
            .expect("build manifest");
        save_prompt_manifest(Some(&pool), &manifest)
            .await
            .expect("save manifest");

        let row = sqlx::query(
            "SELECT full_content, is_truncated, content_sha256, original_bytes \
             FROM prompt_manifest_layers LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch saved layer");
        let stored: Option<String> = row.try_get("full_content").unwrap_or(None);
        let is_truncated: bool = row.try_get("is_truncated").unwrap_or(false);
        let content_sha256: String = row.try_get("content_sha256").unwrap_or_default();
        let original_bytes: Option<i64> = row.try_get("original_bytes").ok().flatten();

        let stored = stored.expect("body stored");
        assert!(stored.len() <= 64, "saved body must fit byte cap");
        assert!(is_truncated, "is_truncated must be set when capped");
        assert_eq!(content_sha256, super::sha256_hex(&big_body));
        assert_eq!(
            original_bytes,
            Some(big_body.len() as i64),
            "original_bytes must reflect the pre-truncation length"
        );

        crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
            .await
            .expect("close test pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn prompt_manifest_apply_retention_trims_old_full_content_pg() {
        use sqlx::Row;
        let Some(test_db) = PromptManifestPgDatabase::create().await else {
            eprintln!(
                "skipping prompt_manifest_apply_retention_trims_old_full_content_pg: postgres unavailable"
            );
            return;
        };
        let pool = test_db.migrate().await;

        // Seed a manifest with full content, then back-date it past the horizon.
        let manifest = PromptManifestBuilder::new("turn-retention", "1499610614904131594")
            .content_layer(
                "system",
                true,
                Some("prompt_builder"),
                Some("base"),
                PromptContentVisibility::AdkProvided,
                "old-system-content",
            )
            .build()
            .expect("build manifest");
        save_prompt_manifest(Some(&pool), &manifest)
            .await
            .expect("save manifest");

        // Back-date by 60 days.
        sqlx::query("UPDATE prompt_manifests SET created_at = NOW() - INTERVAL '60 days'")
            .execute(&pool)
            .await
            .expect("backdate manifest");

        let cfg = crate::config::PromptManifestRetentionConfig {
            enabled: true,
            full_content_days: 30,
            per_layer_max_bytes_adk_provided: 0,
            per_layer_max_bytes_user_derived: 0,
        };
        let report = super::apply_retention_policy(&pool, &cfg, false)
            .await
            .expect("apply retention policy");
        assert_eq!(report.trimmed_full_content, 1);

        // Verify hash is preserved, full_content is NULL, is_truncated set.
        let row = sqlx::query(
            "SELECT full_content, is_truncated, content_sha256 \
             FROM prompt_manifest_layers LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch trimmed layer");
        let full_content: Option<String> = row.try_get("full_content").unwrap_or(None);
        let is_truncated: bool = row.try_get("is_truncated").unwrap_or(false);
        let content_sha256: String = row.try_get("content_sha256").unwrap_or_default();
        assert!(full_content.is_none(), "full_content must be NULL");
        assert!(is_truncated, "is_truncated must be TRUE");
        assert_eq!(content_sha256, super::sha256_hex("old-system-content"));

        // Stats should report at least one layer + zero remaining full_content rows.
        let stats = super::manifest_storage_stats(&pool, &cfg)
            .await
            .expect("stats");
        assert!(stats.layer_count >= 1);
        assert_eq!(
            stats.oldest_full_content_at, None,
            "no rows should still have full_content"
        );
        assert_eq!(stats.retention_days, 30);
        assert!(stats.retention_horizon_at.is_some());

        crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
            .await
            .expect("close test pool");
        test_db.drop().await;
    }
}
