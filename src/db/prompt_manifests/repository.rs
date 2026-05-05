use anyhow::{Result, anyhow};
use sqlx::{PgPool, Row};

use super::model::{PromptContentVisibility, PromptManifest, PromptManifestLayer};
use super::redaction::{apply_byte_cap, normalized_opt, usize_to_i64};
use super::retention::current_retention_config;

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
