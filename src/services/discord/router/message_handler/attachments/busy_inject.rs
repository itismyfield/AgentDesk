//! A busy turn reads files after the injection call returns, using the existing upload lifetime.
use super::*;
use crate::services::cluster::attachment_transfer::materialize::{self, MaterializedUploads};

pub(in crate::services::discord::router) async fn prepare_busy_inject_uploads(
    channel_id: serenity::ChannelId,
    uploads: &[Upload],
    pool: Option<&sqlx::PgPool>,
) -> Result<Vec<String>, String> {
    if uploads.is_empty() {
        return Ok(Vec::new());
    }
    let prepared = materialize::prepare(uploads, pool).await?;
    retain_busy_uploads(channel_id, prepared).await
}

pub(in crate::services::discord::router) async fn retain_busy_uploads(
    channel_id: serenity::ChannelId,
    prepared: MaterializedUploads,
) -> Result<Vec<String>, String> {
    if !prepared.has_materialized_files() {
        return Ok(prepared.records);
    }
    let directory = channel_upload_dir(channel_id).ok_or("cannot resolve upload directory")?;
    tokio::task::spawn_blocking(move || prepared.copy_into(&directory))
        .await
        .map_err(|error| error.to_string())?
}
