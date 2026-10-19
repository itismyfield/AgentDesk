//! Exact source and ordered-prefix proof for a producer that has lost its process.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use serde_json::Value;

use crate::services::agent_protocol::codex_payload_turn_id;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder;
use crate::services::discord::inflight::{InflightTurnState, parse_started_at_unix};

pub(super) struct Proof {
    pub(super) raw: File,
    pub(super) wrapper: File,
    pub(super) own_turn: String,
    pub(super) origin: u64,
    pub(super) replay: Vec<Value>,
}

/// Keeps the opened files; publication rechecks their identities and byte prefixes.
pub(super) fn prove(
    state: &InflightTurnState,
    mut raw: File,
    mut wrapper: File,
) -> Result<Proof, String> {
    let session = state
        .session_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or("session_unknown")?;
    let birth = parse_started_at_unix(&state.started_at)
        .and_then(|seconds| seconds.checked_mul(1_000))
        .ok_or("timestamp_missing")?;
    if state.user_text.is_empty() || state.full_response.is_empty() {
        return Err("empty_prompt_or_composed_response".into());
    }
    let native = records(&mut raw, 0)?;
    let mut replay = records(&mut wrapper, state.turn_start_offset.unwrap_or(0))?
        .into_iter()
        .map(|(_, record)| record)
        .collect::<Vec<_>>();
    if replay.is_empty() || replay.iter().any(|record| record["type"] == "result") {
        return Err("empty_or_terminal_wrapper".into());
    }
    let mut session_record = None;
    let mut current: Option<(String, u64)> = None;
    let mut candidate = None;
    let mut starts = std::collections::HashSet::new();
    for (offset, record) in &native {
        if record["type"] == "session_meta" {
            if record.pointer("/payload/id").and_then(Value::as_str) != Some(session) {
                return Err("session_mismatch".into());
            }
            if session_record.replace(record).is_some() {
                return Err("ambiguous_session".into());
            }
        }
        if let Some(opened) = opened_turn(record) {
            let turn = opened.ok_or("turn_unknown")?;
            let task_start =
                record.pointer("/payload/type").and_then(Value::as_str) == Some("task_started");
            if task_start && !starts.insert(turn.to_owned()) {
                return Err("ambiguous_turn".into());
            }
            // Compaction may repeat this turn's context without starting another turn.
            if task_start || current.as_ref().is_none_or(|(id, _)| id != turn) {
                current = Some((turn.to_owned(), *offset));
            }
        }
        let Some(prompt) = exact_user_prompt(record) else {
            continue;
        };
        if !prompt_matches(&state.user_text, &prompt, state.request_owner_user_id) {
            continue;
        }
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
            .map(|stamp| stamp.timestamp_millis())
            .ok_or("timestamp_missing")?;
        if timestamp < birth {
            continue;
        }
        if session_record.is_none() {
            return Err("session_unknown".into());
        }
        let selected = current.clone().ok_or("turn_unknown")?;
        if record_turn(record).is_some_and(|turn| turn != selected.0)
            || state
                .native_turn_id
                .as_deref()
                .is_some_and(|turn| turn != selected.0)
        {
            return Err("prompt_turn_mismatch".into());
        }
        if candidate.replace(selected).is_some() {
            return Err("ambiguous_prompt".into());
        }
    }
    let session_record = session_record.ok_or("session_unknown")?;
    let (own_turn, origin) = candidate.ok_or("prompt_not_found")?;
    let mut decoder = RolloutRecordDecoder::default();
    let init = decoder
        .decode(session_record)
        .and_then(|messages| messages.into_iter().next())
        .and_then(super::super::rebind_runtime::codex_rebind_stream_message_json)
        .ok_or("session_unknown")?;
    // A later episode can start after the wrapper's original init. Its virtual replay init
    // seeds the same decoder while leaving every stored event in its original order.
    if replay.first() != Some(&init) {
        replay.insert(0, init.clone());
    }
    let mut matched = 1;
    for (offset, record) in &native {
        if *offset < origin {
            continue;
        }
        if opened_turn(record).is_some_and(|opened| opened != Some(own_turn.as_str())) {
            return Err("foreign_turn_opener".into());
        }
        if record_turn(record).is_some_and(|turn| turn != own_turn) {
            return Err("foreign_turn_record".into());
        }
        match record.pointer("/payload/type").and_then(Value::as_str) {
            Some("task_complete") => break,
            Some("turn_aborted") => return Err("aborted_turn".into()),
            _ => {}
        }
        // Generic completion and EOF are not proof of this task's terminal.
        if record["type"] == "turn.completed" {
            continue;
        }
        for message in decoder.decode(record).unwrap_or_default() {
            if let Some(json) =
                super::super::rebind_runtime::codex_rebind_stream_message_json(message)
                && matched < replay.len()
            {
                if replay[matched] != json {
                    return Err("normalized_prefix_mismatch".into());
                }
                matched += 1;
            }
        }
    }
    if matched != replay.len() {
        return Err("incomplete_normalized_prefix".into());
    }
    if !decoder.response().starts_with(&state.full_response) {
        return Err("composed_prefix_mismatch".into());
    }
    raw.seek(SeekFrom::Start(origin))
        .map_err(|error| format!("io:{error}"))?;
    Ok(Proof {
        raw,
        wrapper,
        own_turn,
        origin,
        replay,
    })
}

fn records(file: &mut File, start: u64) -> Result<Vec<(u64, Value)>, String> {
    let metadata = file.metadata().map_err(|error| format!("io:{error}"))?;
    if !metadata.is_file()
        || SourceFileIdentity::from_open_file(file) == SourceFileIdentity::Unavailable
    {
        return Err("source_not_regular_or_identity_unavailable".into());
    }
    if start > metadata.len() {
        return Err("range_beyond_source".into());
    }
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1))
            .map_err(|error| format!("io:{error}"))?;
        let mut previous = [0];
        file.read_exact(&mut previous)
            .map_err(|error| format!("io:{error}"))?;
        if previous[0] != b'\n' {
            return Err("range_not_record_boundary".into());
        }
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("io:{error}"))?;
    let mut reader = BufReader::new(file.take(metadata.len() - start));
    let mut result = Vec::new();
    let mut offset = start;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("io:{error}"))?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            return Err("torn_tail".into());
        }
        let record = serde_json::from_slice(&line).map_err(|_| "malformed_record")?;
        result.push((offset, record));
        offset += read as u64;
    }
    Ok(result)
}

fn opened_turn(record: &Value) -> Option<Option<&str>> {
    (record["type"] == "turn_context"
        || (record["type"] == "event_msg"
            && record.pointer("/payload/type").and_then(Value::as_str) == Some("task_started")))
    .then(|| record_turn(record))
}

fn record_turn(record: &Value) -> Option<&str> {
    record
        .get("payload")
        .and_then(codex_payload_turn_id)
        .or_else(|| {
            record
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
        })
}

fn exact_user_prompt(record: &Value) -> Option<String> {
    let payload = record.get("payload")?;
    if record["type"] != "response_item"
        || payload["type"] != "message"
        || payload["role"] != "user"
    {
        return None;
    }
    if payload
        .pointer("/internal_chat_message_metadata_passthrough/content_item_kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| {
            !kinds.is_empty() && kinds.iter().all(|kind| kind == "shell.user_command")
        })
    {
        return None;
    }
    match payload.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if !matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("text" | "input_text")
                ) {
                    return None;
                }
                item.get("text")
                    .or_else(|| item.get("input_text"))
                    .and_then(Value::as_str)
            })
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join("\n")),
        _ => None,
    }
}

fn prompt_matches(expected: &str, observed: &str, owner: u64) -> bool {
    if expected == observed {
        return true;
    }
    let separator = if expected.contains('\n') { "\n" } else { " " };
    let Some(envelope) = observed
        .strip_suffix(expected)
        .and_then(|prefix| prefix.strip_suffix(separator))
    else {
        return false;
    };
    let ending = format!(" (ID: {owner})]");
    let Some(author) = envelope
        .strip_prefix("[User: ")
        .and_then(|prefix| prefix.strip_suffix(&ending))
    else {
        return false;
    };
    !author.is_empty()
        && !author.contains(['[', ']', '{', '}', '\r', '\n'])
        && author.split_whitespace().collect::<Vec<_>>().join(" ") == author
}

#[cfg(test)]
#[path = "proof_tests.rs"]
mod tests;
