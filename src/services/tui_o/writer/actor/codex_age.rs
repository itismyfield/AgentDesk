//! Native activity bounds dialogue age without publishing prompts or binding observations.

use std::path::Path;
use std::time::SystemTime;

use chrono::DateTime;
use serde_json::Value;

use crate::services::tui_o::shadow::SourceId;

pub(super) fn last_dialogue(bytes: &[u8], source: &SourceId, root: &Path) -> Option<SystemTime> {
    let id = uuid::Uuid::parse_str(&source.session_id).ok()?;
    if id.hyphenated().to_string() != source.session_id {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    let native_name = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
    };
    let canonical = source.path.canonicalize().ok()?;
    if !canonical.starts_with(root.canonicalize().ok()?)
        || !native_name(&source.path)
        || !native_name(&canonical)
    {
        return None;
    }
    let mut rows = bytes.strip_suffix(b"\n")?.split(|byte| *byte == b'\n');
    let header: Value = serde_json::from_slice(rows.next()?).ok()?;
    if !native_header(&header, source) {
        return None;
    }
    let mut last = None;
    for line in rows {
        let row: Value = serde_json::from_slice(line).ok()?;
        if row["type"] == "session_meta" {
            if !native_header(&row, source) {
                return None;
            }
            continue;
        }
        let timestamp =
            SystemTime::from(DateTime::parse_from_rfc3339(row["timestamp"].as_str()?).ok()?);
        last = Some(last.map_or(timestamp, |previous: SystemTime| previous.max(timestamp)));
    }
    last
}

fn native_header(row: &Value, source: &SourceId) -> bool {
    let Some(meta) = row.get("payload") else {
        return false;
    };
    row["type"] == "session_meta"
        && meta["id"].as_str() == Some(source.session_id.as_str())
        && meta["cwd"]
            .as_str()
            .is_some_and(|cwd| Path::new(cwd).is_absolute())
        && matches!(meta["source"].as_str(), Some("cli" | "exec" | "vscode"))
        && meta.get("parent_thread_id").is_none_or(Value::is_null)
}
