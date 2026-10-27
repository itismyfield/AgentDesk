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

/// Runs before any unlink, marker change or legacy publish, even in an adopted actor. Only a
/// snapshot read as a non-legacy object refuses; a failed read leaves the write to report itself.
pub(crate) fn require_legacy_write(path: &Path) -> Result<(), String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    if content.trim_start().starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| format!("unreadable queue object {}: {e}", path.display()))?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(format!(
                "read-only queue snapshot {}: legacy mutation refused; preserve the original file and use a V2-capable recovery binary for manual inspection",
                path.display()
            ));
        }
    }
    Ok(())
}
