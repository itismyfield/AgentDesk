use super::runtime_store::durable_publish::{FlushStatus, publish_durable};
use super::*;
use std::path::Path;

pub(super) fn read(path: &Path) -> Result<Option<Attempt>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("corrupt body create record: {e}"))
}
pub(super) fn publish(root: &Path, path: &Path, attempt: &Attempt) -> Result<(), String> {
    let bytes = serde_json::to_string(attempt).map_err(|e| e.to_string())?;
    match publish_durable(root, path, &bytes)
        .map_err(|e| e.to_string())?
        .flush_status
    {
        FlushStatus::Flushed => Ok(()),
        status => Err(format!(
            "body create record retained with uncertain flush: {status:?}"
        )),
    }
}

/// Call only under the record's lock after verifying its synced, rename-visible bytes.
pub(super) fn confirm_existing(root: &Path, path: &Path) -> Result<(), String> {
    match super::runtime_store::durable_publish::flush_chain_before_publish(root, path) {
        FlushStatus::Flushed => Ok(()),
        status => {
            tracing::warn!(path = %path.display(), ?status, "created settlement remains unconfirmed");
            Err(format!(
                "created settlement retained with uncertain flush: {status:?}"
            ))
        }
    }
}
