//! A completed reply of this launch, over the exact pinned source and an audited quiet suffix.
mod source;
use super::{closed_turn, evidence::Stamp, source::Source};
use crate::services::tui_turn_state::{
    claude_closed_compactions, claude_completed_reply, claude_idle_metadata,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

pub(super) fn current(source: &Source, stamp: &Stamp) -> bool {
    checked(source, stamp).is_some()
}
fn checked(source: &Source, stamp: &Stamp) -> Option<()> {
    let launched = source::launched(source, stamp)?;
    let records = records(source, stamp)?;
    let (compacted, _) = claude_closed_compactions(&records);
    let mut summaries = HashSet::new();
    let mut stops = HashSet::new();
    for (record, compacted) in records.iter().zip(compacted).rev() {
        let native = record["sessionId"]
            .as_str()
            .or_else(|| record["session_id"].as_str());
        if native.is_some_and(|id| id != source.native) {
            return None;
        }
        if claude_completed_reply(record) {
            let at = DateTime::parse_from_rfc3339(record["timestamp"].as_str()?)
                .ok()?
                .to_utc();
            let launch_matches = at >= launched && at <= Utc::now();
            if !launch_matches || native != Some(source.native.as_str()) {
                return None;
            }
            return summaries.is_subset(&stops).then_some(());
        }
        let attachment = &record["attachment"];
        // A closed idle compaction passes its boundary, summary and its quiet `SessionStart`.
        if compacted
            && (record["type"] != "attachment"
                || (attachment["hookEvent"] == "SessionStart"
                    && attachment["stdout"].as_str().map(str::trim)
                        == Some(r#"{"suppressOutput":true}"#)))
        {
            continue;
        }
        let stop = attachment["hookEvent"] == "Stop"
            || attachment["hookName"] == "Stop"
            || attachment["stdout"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .is_some_and(|output| output["hookSpecificOutput"]["hookEventName"] == "Stop");
        let starts_work = ["hookEvent", "hookName"].iter().any(|key| {
            matches!(
                attachment[key].as_str(),
                Some("UserPromptSubmit" | "PreToolUse" | "PostToolUse")
            )
        });
        let has_output = !attachment["stdout"].is_null()
            && !attachment["stdout"]
                .as_str()
                .is_some_and(|text| text.trim().is_empty());
        if record["type"] == "attachment"
            && attachment["type"] == "hook_success"
            && !stop
            && (starts_work || has_output)
        {
            return None;
        }
        if record["type"] == "attachment" && stop {
            if !closed_turn::quiet_relay_stop(attachment, &source.native) {
                return None;
            }
            if let Some(id) = attachment["toolUseID"].as_str() {
                stops.insert(id);
            }
        } else if record["type"] == "system" && record["subtype"] == "stop_hook_summary" {
            summaries.insert(closed_turn::quiet_summary(record)?);
        } else if !(record["type"] == "system" && record["subtype"] == "turn_duration")
            && !claude_idle_metadata(record)
        {
            return None;
        }
    }
    None
}
fn records(source: &Source, stamp: &Stamp) -> Option<Vec<Value>> {
    let mut file = std::fs::File::open(&source.path).ok()?;
    if !source::same_file(&file, &source.path, stamp) {
        return None;
    }
    let start = stamp.len.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = BufReader::new((&mut file).take(stamp.len - start));
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
    drop(reader);
    source::same_file(&file, &source.path, stamp).then_some(records)
}
