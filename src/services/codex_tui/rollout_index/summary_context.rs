//! Read Codex model metadata from the exact rollout already bound to a Legacy turn.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::strict_parent_session_file;
use crate::services::agent_protocol::codex_payload_turn_id;

const MAX_CONTEXT_SCAN_BYTES: u64 = 1024 * 1024;

pub(crate) struct CodexModel {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

pub(crate) fn context(value: &Value) -> Option<(&str, Option<&str>, Option<&str>)> {
    if value.get("type")?.as_str()? != "turn_context" {
        return None;
    }
    let payload = value.get("payload")?;
    let turn = codex_payload_turn_id(payload)?;
    let settings = payload.pointer("/collaboration_mode/settings");
    let model = text(payload.get("model")).or_else(|| text(settings?.get("model")));
    let effort = text(payload.get("effort")).or_else(|| text(settings?.get("reasoning_effort")));
    Some((turn, model, effort))
}

fn text(value: Option<&Value>) -> Option<&str> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn read(
    path: &Path,
    session_id: &str,
    turn_start_offset: Option<u64>,
    native_turn_id: Option<&str>,
    started: DateTime<Utc>,
    ended: DateTime<Utc>,
) -> Option<CodexModel> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let offset = turn_start_offset.or_else(|| native_turn_id.map(|_| len))?;
    if offset > len {
        return None;
    }
    strict_parent_session_file(&file, session_id).ok()?;

    // The native ID can identify a context before an offset advanced past the prompt.
    let floor = if native_turn_id.is_some() {
        offset.saturating_sub(MAX_CONTEXT_SCAN_BYTES)
    } else {
        offset
    };
    let ceiling = len.min(offset.saturating_add(MAX_CONTEXT_SCAN_BYTES));
    let mut partial_record = false;
    if let Some(previous) = floor.checked_sub(1) {
        file.seek(SeekFrom::Start(previous)).ok()?;
        let mut byte = [0];
        file.read_exact(&mut byte).ok()?;
        partial_record = byte[0] != b'\n';
        if partial_record && native_turn_id.is_none() {
            return None;
        }
    } else {
        file.seek(SeekFrom::Start(floor)).ok()?;
    }

    let mut reader = BufReader::new(file.take(ceiling - floor));
    let mut line = Vec::new();
    if partial_record {
        reader.read_until(b'\n', &mut line).ok()?;
    }
    let mut own_turn = native_turn_id.map(str::to_owned);
    loop {
        line.clear();
        reader.read_until(b'\n', &mut line).ok()?;
        if !line.ends_with(b"\n") {
            return None;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: Value = serde_json::from_slice(&line).ok()?;
        let payload = record.get("payload");
        let event = if record["type"] == "event_msg" {
            record.pointer("/payload/type").and_then(Value::as_str)
        } else {
            None
        };
        let in_turn_time = || {
            record["timestamp"]
                .as_str()
                .and_then(|stamp| stamp.parse::<DateTime<Utc>>().ok())
                .is_some_and(|stamp| (started..=ended).contains(&stamp))
        };

        if native_turn_id.is_none() {
            match event {
                Some("task_started") if own_turn.is_none() && in_turn_time() => {
                    let payload = payload?;
                    let id = codex_payload_turn_id(payload)?;
                    if text(payload.get("root_turn_id")).is_some_and(|root| root != id) {
                        return None;
                    }
                    own_turn = Some(id.to_owned());
                }
                Some("task_started" | "task_complete" | "turn_aborted") => return None,
                _ => {}
            }
        } else if matches!(event, Some("task_complete" | "turn_aborted"))
            && payload.and_then(codex_payload_turn_id) == native_turn_id
        {
            return None;
        }

        if let Some((id, model, effort)) = context(&record)
            && Some(id) == own_turn.as_deref()
            && (native_turn_id.is_some() || in_turn_time())
            && (model.is_some() || effort.is_some())
        {
            return Some(CodexModel {
                model: model.map(str::to_owned),
                effort: effort.map(str::to_owned),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{"type":"session_meta","payload":{"id":"current","cwd":"/work"}}"#;
    const START: &str = r#"{"type":"event_msg","timestamp":"2026-10-01T10:00:01Z","payload":{"type":"task_started","turn_id":"ours"}}"#;
    const CONTEXT: &str = r#"{"type":"turn_context","timestamp":"2026-10-01T10:00:02Z","payload":{"turn_id":"ours","model":"gpt-6.2-sol","effort":"ultra"}}"#;
    const END: &str = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"ours"}}"#;
    const NEXT_START: &str = r#"{"type":"event_msg","timestamp":"2026-10-01T10:00:10Z","payload":{"type":"task_started","turn_id":"next"}}"#;
    const NEXT_CONTEXT: &str = r#"{"type":"turn_context","timestamp":"2026-10-01T10:00:11Z","payload":{"turn_id":"next","model":"wrong"}}"#;

    fn tape(records: &[&str]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), format!("{}\n", records.join("\n"))).unwrap();
        file
    }

    fn initial_offset() -> u64 {
        HEADER.len() as u64 + 1
    }

    fn read_fixture(
        file: &tempfile::NamedTempFile,
        session: &str,
        offset: Option<u64>,
        native: Option<&str>,
    ) -> Option<CodexModel> {
        read(
            file.path(),
            session,
            offset,
            native,
            "2026-10-01T10:00:00Z".parse().unwrap(),
            "2026-10-01T10:00:09Z".parse().unwrap(),
        )
    }

    #[test]
    fn legacy_codex_reads_offset_pinned_turn_settings() {
        let file = tape(&[HEADER, START, CONTEXT, END, NEXT_START, NEXT_CONTEXT]);
        let got = read_fixture(&file, "current", Some(initial_offset()), None).unwrap();
        assert_eq!(got.model.as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(got.effort.as_deref(), Some("ultra"));
    }

    #[test]
    fn legacy_codex_native_id_allows_context_before_prompt_offset() {
        let file = tape(&[HEADER, START, CONTEXT, END, NEXT_START, NEXT_CONTEXT]);
        let advanced = initial_offset() + START.len() as u64 + CONTEXT.len() as u64 + 2;
        let got = read_fixture(&file, "current", Some(advanced), Some("ours")).unwrap();
        assert_eq!(got.model.as_deref(), Some("gpt-6.2-sol"));
        assert!(read_fixture(&file, "current", Some(advanced), None).is_none());
    }

    #[test]
    fn legacy_codex_rejects_wrong_session_partial_offset_and_following_turn() {
        let file = tape(&[HEADER, START, END, NEXT_START, NEXT_CONTEXT]);
        let good = Some(initial_offset());
        assert!(read_fixture(&file, "current", good, None).is_none());
        let wrong_time = Some(initial_offset() + START.len() as u64 + END.len() as u64 + 2);
        assert!(read_fixture(&file, "current", wrong_time, None).is_none());
        let file = tape(&[HEADER, START, CONTEXT]);
        assert!(read_fixture(&file, "other", good, Some("ours")).is_none());
        assert!(read_fixture(&file, "current", Some(initial_offset() + 1), None).is_none());
    }

    #[test]
    fn rollout_context_supports_nested_effort_and_missing_model() {
        let value = serde_json::json!({"type":"turn_context", "payload":{
            "turn_id":"ours", "model":" ", "collaboration_mode":{
                "settings":{"reasoning_effort":"ultra"}
            }
        }});
        assert_eq!(context(&value), Some(("ours", None, Some("ultra"))));
        assert!(
            context(&serde_json::json!({"type":"turn_context","payload":{"effort":"ultra"}}))
                .is_none()
        );
    }
}
