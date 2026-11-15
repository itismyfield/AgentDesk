//! A turn this execution finished with a non-continuing AgentDesk Stop, read back from the transcript.
use super::evidence::Stamp;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

const WINDOW: u64 = 256 * 1024;
const QUIET_STOP: &str = r#"{"suppressOutput":true}"#;

/// Whether `path` still ends on the closed turn its last activity began at or after `since`: the
/// relay's quiet Stop as the only Stop output, then its summary, then `turn_duration`, then only
/// records the readiness tail ignores. Any other or unknown shape proves nothing.
pub(super) fn closed_since(path: &Path, stamp: &Stamp, native: &str, since: DateTime<Utc>) -> bool {
    let Some(records) = tail(path, stamp) else {
        return false;
    };
    let mut rest = records
        .iter()
        .rev()
        .skip_while(|record| matches!(kind(record), Some("file-history-snapshot" | "last-prompt")));
    let (Some(duration), Some(summary)) = (rest.next(), rest.next()) else {
        return false;
    };
    let Some(stop) = quiet_summary(summary) else {
        return false;
    };
    if system(duration) != Some("turn_duration") {
        return false;
    }
    let mut relay = 0;
    for record in rest {
        if kind(record) == Some("assistant") {
            return relay == 1
                && at(record).is_some_and(|seen| seen >= since)
                && at(summary) >= Some(since);
        }
        if kind(record) != Some("attachment") {
            return false;
        }
        let output = &record["attachment"];
        if output["toolUseID"].as_str() == Some(stop) || output["hookEvent"] == "Stop" {
            if !quiet_relay_stop(output, native) {
                return false;
            }
            relay += 1;
        }
    }
    false
}

/// The summary of a Stop that asked for nothing: no errors, no added context, no stop reason.
fn quiet_summary(record: &Value) -> Option<&str> {
    let empty = |key: &str| record[key].as_array().is_some_and(Vec::is_empty);
    (system(record) == Some("stop_hook_summary")
        && record["preventedContinuation"] == false
        && empty("hookErrors")
        && (record["hookAdditionalContext"].is_null() || empty("hookAdditionalContext"))
        && (record["stopReason"].is_null() || record["stopReason"] == ""))
        .then(|| record["toolUseID"].as_str())
        .flatten()
        .filter(|id| !id.is_empty())
}

fn quiet_relay_stop(output: &Value, native: &str) -> bool {
    let command = output["command"].as_str().unwrap_or_default();
    output["type"] == "hook_success"
        && output["hookEvent"] == "Stop"
        && output["hookName"] == "Stop"
        && command.contains("claude-hook-relay")
        && command.contains(&format!("--session-id {native}"))
        && output["stdout"].as_str().map(str::trim) == Some(QUIET_STOP)
}

fn kind(record: &Value) -> Option<&str> {
    record["type"].as_str()
}

fn system(record: &Value) -> Option<&str> {
    (kind(record) == Some("system"))
        .then(|| record["subtype"].as_str())
        .flatten()
}

fn at(record: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(record["timestamp"].as_str()?)
        .ok()
        .map(|seen| seen.to_utc())
}

/// Complete records of the bounded tail, only while the file is still the one `stamp` names.
fn tail(path: &Path, stamp: &Stamp) -> Option<Vec<Value>> {
    let mut file = std::fs::File::open(path).ok()?;
    let start = stamp.len.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = BufReader::new(std::io::Read::take(file, stamp.len - start));
    let mut line = Vec::new();
    if start > 0 {
        reader.read_until(b'\n', &mut line).ok()?;
        line.clear();
    }
    let mut records = Vec::new();
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        if !line.ends_with(b"\n") {
            return None;
        }
        records.push(serde_json::from_slice(&line).ok()?);
        line.clear();
    }
    (Stamp::read(path).as_ref() == Some(stamp)).then_some(records)
}
