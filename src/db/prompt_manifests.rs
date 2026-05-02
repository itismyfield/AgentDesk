use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

const USER_DERIVED_PREVIEW_CHARS: usize = 240;

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
        let content = content.into();
        let chars = usize_to_i64(content.chars().count());
        let tokens_est = estimate_tokens_from_chars_i64(chars);
        let content_sha256 = sha256_hex(&content);
        let (full_content, redacted_preview) = match content_visibility {
            PromptContentVisibility::AdkProvided => (Some(content), None),
            PromptContentVisibility::UserDerived => (None, redacted_preview(&content)),
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
        }
    }
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

    for layer in &manifest.layers {
        let layer_name = layer.layer_name.trim();
        if layer_name.is_empty() {
            return Err(anyhow!("prompt manifest layer_name is required"));
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
                redacted_preview
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
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
        .bind(layer.full_content.as_deref())
        .bind(layer.redacted_preview.as_deref())
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
            redacted_preview
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
}
