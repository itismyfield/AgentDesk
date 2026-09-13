//! #5521: retain a detached terminal episode until the existing worker settles it.
//! The sibling directory is outside old inflight/receipt writers and reapers.
//! Payload interpretation, receipt checks and publication remain in turn_bridge.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use super::{
    outbound::delivery_record::{DeliveryRecordLock, lock_record_path},
    runtime_store,
};

const DIRECTORY: &str = "discord_terminal_delivery_custody";
const BATCH_SIZE: usize = 10;
static NEXT_ENTRY: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct Record {
    version: u8,
    key: String,
    seed_sha256: String,
    payload: Value,
}

fn root() -> Result<PathBuf, String> {
    runtime_store::runtime_root()
        .map(|root| root.join(DIRECTORY))
        .ok_or_else(|| "terminal custody runtime root unavailable".into())
}

fn record_path(root: &Path, key: &str) -> PathBuf {
    root.join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
}

async fn lock(path: &Path) -> Result<DeliveryRecordLock, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || lock_record_path(&path))
        .await
        .map_err(|error| error.to_string())?
}

/// A successful return means the exact retry payload reached a file and its
/// parent-directory fsync. The initial snapshot cannot be replaced on retry;
/// receipt/cleanup progress recorded by the terminal adapter is preserved.
pub(in crate::services::discord) async fn persist(
    key: &str,
    payload: &Value,
) -> Result<(), String> {
    persist_at(&root()?, key, payload).await
}

async fn persist_at(root: &Path, key: &str, payload: &Value) -> Result<(), String> {
    if key.trim().is_empty() || !payload.is_object() {
        return Err("terminal custody requires episode identity and object payload".into());
    }
    let path = record_path(root, key);
    let _lock = lock(&path).await?;
    // lock_record_path creates the directory. Also persist its entry in the
    // runtime root before acknowledging the first record in that directory.
    runtime_store::fsync_parent_dir(root).map_err(|error| error.to_string())?;
    let record = Record {
        version: 1,
        key: key.into(),
        seed_sha256: format!("{:x}", Sha256::digest(payload.to_string().as_bytes())),
        payload: payload.clone(),
    };
    match fs::read_to_string(&path) {
        Ok(existing) => {
            let existing: Record =
                serde_json::from_str(&existing).map_err(|error| error.to_string())?;
            if existing.version != record.version
                || existing.key != record.key
                || existing.seed_sha256 != record.seed_sha256
                || !existing.payload.is_object()
            {
                return Err(
                    "terminal custody episode payload conflicts with retained record".into(),
                );
            }
            return runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let encoded = serde_json::to_string(&record).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(&path, &encoded)?;
    runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())
}

/// Called by message_outbox_loop, including after restart. A failed resume
/// keeps custody; only the terminal adapter can attest that it is settled.
pub(crate) async fn drain(registry: &super::health::HealthRegistry) {
    let result = match root() {
        Ok(root) => {
            drain_with(&root, |mut payload| async move {
                let result =
                    super::turn_bridge::resume_foreign_terminal_custody(registry, &mut payload)
                        .await;
                (payload, result)
            })
            .await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        tracing::warn!(%error, "detached terminal custody retained for retry");
    }
}

async fn drain_with<F, Fut>(root: &Path, mut resume: F) -> Result<usize, String>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = (Value, Result<bool, String>)>,
{
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };
    // Round-robin batches keep one temporarily undeliverable episode from
    // starving later records. This cursor is scheduling only, never authority.
    let is_record = |entry: &std::io::Result<fs::DirEntry>| {
        entry.as_ref().map_or(true, |entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        })
    };
    let count = entries.filter(is_record).count();
    if count == 0 {
        return Ok(0);
    }
    let start = NEXT_ENTRY.fetch_add(1, Ordering::Relaxed) % count;
    let entries = fs::read_dir(root).map_err(|error| error.to_string())?;
    let mut settled = 0;
    for entry in entries.filter(is_record).skip(start).take(BATCH_SIZE) {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let _lock = lock(&path).await?;
        let encoded = match fs::read_to_string(&path) {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let mut record: Record =
            serde_json::from_str(&encoded).map_err(|error| error.to_string())?;
        if record.version != 1
            || record_path(root, &record.key) != path
            || record.seed_sha256.len() != 64
            || !record
                .seed_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !record.payload.is_object()
        {
            return Err(
                "terminal custody version, identity or payload is invalid; record preserved".into(),
            );
        }
        let (payload, result) = resume(record.payload.clone()).await;
        // The flock spans resume and removal, so another process cannot replay
        // or replace this episode between publication and this exact CAS.
        if fs::read_to_string(&path).map_err(|error| error.to_string())? != encoded {
            return Err("terminal custody changed during resume; record preserved".into());
        }
        if !matches!(result, Ok(true)) {
            // A real transport receipt must survive a later cleanup failure.
            // The adapter owns this progress; storage never interprets it as
            // delivery evidence or replaces the immutable initial identity.
            if payload != record.payload {
                if !payload.is_object() {
                    return Err(
                        "terminal custody resume payload is invalid; record preserved".into(),
                    );
                }
                record.payload = payload;
                let updated = serde_json::to_string(&record).map_err(|error| error.to_string())?;
                runtime_store::atomic_write(&path, &updated)?;
                runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
            }
            result?;
            continue;
        }
        fs::remove_file(&path).map_err(|error| error.to_string())?;
        runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
        settled += 1;
    }
    Ok(settled)
}

#[cfg(test)]
mod tests;
