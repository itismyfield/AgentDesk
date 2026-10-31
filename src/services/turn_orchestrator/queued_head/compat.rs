//! V2 is inspectable, but is never a writable legacy queue.
use super::v2_types::SnapshotV2;
use super::*;

pub(crate) enum SnapshotRead {
    Legacy(Vec<PendingQueueItem>, Option<Box<SnapshotMeta>>),
    ReadOnlyV2(Box<SnapshotV2>),
}

pub(crate) fn inspect(content: &str) -> Result<SnapshotRead, String> {
    let value: serde_json::Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
    if value.get("version").and_then(serde_json::Value::as_u64) == Some(2) {
        let snapshot = serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(SnapshotRead::ReadOnlyV2(Box::new(snapshot)));
    }
    let (items, meta) = parse_snapshot(content)?;
    Ok(SnapshotRead::Legacy(items, meta.map(Box::new)))
}

/// /clear's guard: only a parsed non-legacy object refuses; a failed read or parse lets the clear
/// replace the file as main does.
pub(crate) fn require_clearable(path: &Path) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(content) => refuse_non_legacy(path, &String::from_utf8_lossy(&content)),
        Err(_) => Ok(()),
    }
}

/// The legacy save's guard: a missing file or a legacy one saves, but a file it cannot read is
/// kept, so the save never replaces a snapshot nobody read.
pub(crate) fn require_legacy_write(path: &Path) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(content) => refuse_non_legacy(path, &String::from_utf8_lossy(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "read {}: {error}; legacy save refused and the original file kept",
            path.display()
        )),
    }
}

fn refuse_non_legacy(path: &Path, content: &str) -> Result<(), String> {
    if content.trim_start().starts_with('{')
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
        && value.get("version").and_then(serde_json::Value::as_u64) != Some(1)
    {
        return Err(format!(
            "read-only queue snapshot {}: legacy mutation refused; preserve the original file and use a V2-capable recovery binary for manual inspection",
            path.display()
        ));
    }
    Ok(())
}
