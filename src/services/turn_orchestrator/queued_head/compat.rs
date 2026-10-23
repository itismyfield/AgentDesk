//! V2 is inspectable, but is never a writable legacy queue.
use super::v2_types::SnapshotV2;
use super::*;

pub(crate) enum SnapshotRead {
    Legacy(Vec<PendingQueueItem>, Option<SnapshotMeta>),
    ReadOnlyV2(Box<SnapshotV2>),
}

pub(crate) fn inspect(content: &str) -> Result<SnapshotRead, String> {
    let value: serde_json::Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
    if value.get("version").and_then(serde_json::Value::as_u64) == Some(2) {
        let snapshot = serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(SnapshotRead::ReadOnlyV2(Box::new(snapshot)));
    }
    let (items, meta) = parse_snapshot(content)?;
    Ok(SnapshotRead::Legacy(items, meta))
}

/// Runs before any unlink, marker change or legacy publish, even in an adopted actor.
pub(crate) fn require_legacy_write(path: &Path) -> Result<(), String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read-only queue preflight {}: {e}", path.display())),
    };
    if content.trim_start().starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| format!("unreadable queue object {}: {e}", path.display()))?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(format!(
                "read-only queue snapshot {}: legacy mutation refused",
                path.display()
            ));
        }
    }
    Ok(())
}
