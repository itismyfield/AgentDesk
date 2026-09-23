//! Bounded PostgreSQL storage. Publish references only after `put` commits.
//! Expired/missing references are errors, never an empty attachment set.
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::uploads::BundleRef;
use super::{
    ATTACHMENT_BUNDLE_V1, AttachmentBundleV1, AttachmentEntryV1, ValidatedAttachmentBundle,
    validate_attachment_bundle_v1,
};

pub(crate) const MAX_FILES: usize = 10;
pub(crate) const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_BUNDLE_BYTES: usize = 24 * 1024 * 1024;
const MAX_STORE_BYTES: i64 = 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadError {
    #[error("attachment storage unavailable: {0}")]
    Storage(#[from] sqlx::Error),
    #[error("{0}")]
    Rejected(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    filename: String,
    sha256: String,
    len: usize,
}

pub(crate) fn check_limits(bundle: &AttachmentBundleV1) -> Result<(), String> {
    if bundle.entries.is_empty()
        || bundle.entries.len() > MAX_FILES
        || bundle.entries.iter().any(|entry| {
            entry.bytes.len() > MAX_FILE_BYTES
                || entry.filename.is_empty()
                || entry.filename.len() > 1024
        })
        || bundle
            .entries
            .iter()
            .map(|entry| entry.bytes.len())
            .sum::<usize>()
            > MAX_BUNDLE_BYTES
    {
        return Err("attachment limit: 10 files, 8 MiB/file, 24 MiB/message".into());
    }
    Ok(())
}

pub(crate) async fn put(
    pool: &PgPool,
    validated: &ValidatedAttachmentBundle,
) -> Result<BundleRef, String> {
    let bundle = validated.as_bundle();
    check_limits(bundle)?;
    let manifest = serde_json::to_value(
        bundle
            .entries
            .iter()
            .map(|entry| Entry {
                filename: entry.filename.clone(),
                sha256: entry.sha256.clone(),
                len: entry.bytes.len(),
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|e| e.to_string())?;
    let payload: Vec<u8> = bundle
        .entries
        .iter()
        .flat_map(|entry| entry.bytes.iter().copied())
        .collect();
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('agentdesk.attachment-bundles.v1')::BIGINT)",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM intake_attachment_bundles WHERE expires_at <= NOW()")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let existing: Option<(uuid::Uuid, serde_json::Value, Vec<u8>)> = sqlx::query_as(
        "SELECT id, manifest, payload FROM intake_attachment_bundles WHERE provider=$1 AND channel_id=$2 AND user_msg_id=$3")
        .bind(&bundle.identity.provider).bind(&bundle.identity.channel_id).bind(&bundle.identity.user_msg_id)
        .fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
    let id = if let Some((id, old_manifest, old_payload)) = existing {
        if old_manifest != manifest || old_payload != payload {
            return Err("attachment identity already contains different bytes".into());
        }
        id
    } else {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(octet_length(payload)),0)::BIGINT FROM intake_attachment_bundles",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if used + payload.len() as i64 > MAX_STORE_BYTES {
            return Err("attachment storage budget exhausted (1 GiB); retry after expiry".into());
        }
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO intake_attachment_bundles (id,provider,channel_id,user_msg_id,manifest,payload) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(id).bind(&bundle.identity.provider).bind(&bundle.identity.channel_id)
            .bind(&bundle.identity.user_msg_id).bind(&manifest).bind(&payload)
            .execute(&mut *tx).await.map_err(|e| e.to_string())?;
        id
    };
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(BundleRef {
        bundle_id: id,
        identity: bundle.identity.clone(),
        source_count: bundle.source_attachment_count,
    })
}

pub(crate) async fn load(
    pool: &PgPool,
    reference: &BundleRef,
) -> Result<ValidatedAttachmentBundle, ReadError> {
    let (manifest, payload): (serde_json::Value, Vec<u8>) = sqlx::query_as(
        "SELECT manifest,payload FROM intake_attachment_bundles WHERE id=$1 AND provider=$2 AND channel_id=$3 AND user_msg_id=$4 AND expires_at > NOW()")
        .bind(reference.bundle_id).bind(&reference.identity.provider).bind(&reference.identity.channel_id)
        .bind(&reference.identity.user_msg_id).fetch_optional(pool).await?
        .ok_or_else(|| ReadError::Rejected("attachment bundle missing, expired or identity mismatch; resend the original files".into()))?;
    decode(reference, manifest, payload).map_err(ReadError::Rejected)
}

fn decode(
    reference: &BundleRef,
    manifest: serde_json::Value,
    payload: Vec<u8>,
) -> Result<ValidatedAttachmentBundle, String> {
    let entries: Vec<Entry> = serde_json::from_value(manifest).map_err(|e| e.to_string())?;
    if entries.len() > MAX_FILES || payload.len() > MAX_BUNDLE_BYTES {
        return Err("attachment bundle exceeds limits".into());
    }
    let mut offset: usize = 0;
    let mut restored = Vec::new();
    for entry in entries {
        let end = offset
            .checked_add(entry.len)
            .ok_or("invalid attachment length")?;
        let bytes = payload
            .get(offset..end)
            .ok_or("incomplete attachment payload")?
            .to_vec();
        restored.push(AttachmentEntryV1 {
            filename: entry.filename,
            sha256: entry.sha256,
            bytes,
        });
        offset = end;
    }
    if offset != payload.len() {
        return Err("unexpected trailing attachment bytes".into());
    }
    let bundle = AttachmentBundleV1 {
        version: ATTACHMENT_BUNDLE_V1,
        identity: reference.identity.clone(),
        source_attachment_count: reference.source_count,
        entries: restored,
    };
    check_limits(&bundle)?;
    validate_attachment_bundle_v1(bundle, &reference.identity)
        .map_err(|e| format!("invalid attachment bundle: {e:?}"))
}

pub(crate) async fn validate_refs(
    pool: &PgPool,
    refs: &[BundleRef],
    provider: &str,
    channel: &str,
) -> Result<(), ReadError> {
    if refs.len() > MAX_FILES {
        return Err(ReadError::Rejected(
            "too many attachment bundles in one turn".into(),
        ));
    }
    let mut total = 0;
    for reference in refs {
        if reference.identity.provider != provider || reference.identity.channel_id != channel {
            return Err(ReadError::Rejected(
                "attachment reference belongs to another provider/channel".into(),
            ));
        }
        let bundle = load(pool, reference).await?;
        total += bundle
            .as_bundle()
            .entries
            .iter()
            .map(|entry| entry.bytes.len())
            .sum::<usize>();
        if total > MAX_BUNDLE_BYTES {
            return Err(ReadError::Rejected(
                "merged attachment turn exceeds 24 MiB".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) async fn cleanup(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM intake_attachment_bundles WHERE expires_at <= NOW()")
            .execute(pool)
            .await?
            .rows_affected(),
    )
}
