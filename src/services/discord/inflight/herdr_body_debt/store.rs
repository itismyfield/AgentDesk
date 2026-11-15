//! Strict file and ancestor-directory durability before an archive can authorize row removal.

use super::*;
use crate::services::discord::runtime_store::{self, AtomicWriteContext};
use runtime_store::durable_publish::{
    FlushStatus, critical_publish_status, retry_published_status,
};

fn flushed(status: FlushStatus) -> Result<(), String> {
    match status {
        FlushStatus::Flushed => Ok(()),
        other => Err(format!("body debt publication not durable: {other:?}")),
    }
}

pub(super) fn publish(path: &Path, encoded: &str) -> Result<(), String> {
    let context = AtomicWriteContext::new("discord_herdr_body_debt");
    match fs::read_to_string(path) {
        Ok(old) if old == encoded => {
            fs::File::open(path)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())?;
            flushed(retry_published_status(path, context))
        }
        Ok(_) => Err("immutable body debt snapshot already differs".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            flushed(critical_publish_status(path, encoded, context)?)
        }
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn read(path: &Path, provider: &ProviderKind) -> Result<Record, String> {
    let encoded = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let record: Record = serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
    record.row()?;
    if record.provider != provider.as_str() || record.path(&directory(provider)?, &encoded) != path
    {
        return Err("body debt filename or provider differs from its snapshot".into());
    }
    Ok(record)
}

pub(super) fn resync(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())?;
    flushed(retry_published_status(
        path,
        AtomicWriteContext::new("discord_herdr_body_debt"),
    ))
}
