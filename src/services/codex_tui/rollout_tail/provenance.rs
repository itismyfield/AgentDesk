//! Dormant native span evidence; fixed source and byte boundaries never select a live binding.

use serde_json::Value;

use crate::services::agent_protocol::codex_payload_turn_id;
use crate::services::codex_tui::rollout_index::strict_parent_session;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, CapturedRecord, MAX_READ_BYTES, SourceId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeTurnAnchor {
    pub native_turn_id: String,
    pub start: u64,
}

/// The caller supplies the boot snapshot's EOF, not a later file length.
pub(crate) fn scan_anchor(
    source: &SourceId,
    turn_start_offset: u64,
    boot_eof: u64,
) -> Result<Option<NativeTurnAnchor>, &'static str> {
    let mut anchor = None;
    for record in records(source, turn_start_offset, boot_eof)? {
        if let Fact::Opened(Some(id)) = fact(&record, source)? {
            if anchor.is_some() {
                return Err("multiple_native_openers");
            }
            anchor = Some(NativeTurnAnchor {
                native_turn_id: id,
                start: record.start,
            });
        }
    }
    Ok(anchor)
}

/// Replay begins at the saved opener; only its own named native terminal closes it.
pub(crate) fn terminal_end(
    source: &SourceId,
    anchor: &NativeTurnAnchor,
    through: u64,
) -> Result<Option<u64>, &'static str> {
    if anchor.native_turn_id.trim().is_empty() {
        return Err("missing_native_turn");
    }
    let mut records = records(source, anchor.start, through)?.into_iter();
    let Some(opener) = records.next() else {
        return Err("missing_native_opener");
    };
    if fact(&opener, source)? != Fact::Opened(Some(anchor.native_turn_id.clone())) {
        return Err("native_anchor_mismatch");
    }
    for record in records {
        match fact(&record, source)? {
            Fact::Opened(_) => return Err("successor_or_duplicate_opener"),
            Fact::Terminal(Some(id)) if id == anchor.native_turn_id => {
                return Ok(Some(record.end));
            }
            Fact::Terminal(_) => return Err("unnamed_or_foreign_terminal"),
            Fact::Other => {}
        }
    }
    Ok(None)
}

#[derive(PartialEq, Eq)]
enum Fact {
    Opened(Option<String>),
    Terminal(Option<String>),
    Other,
}

fn fact(record: &CapturedRecord, source: &SourceId) -> Result<Fact, &'static str> {
    let value: Value = serde_json::from_slice(&record.line).map_err(|_| "invalid_native_record")?;
    let payload = value.get("payload").ok_or("missing_native_payload")?;
    let named = || codex_payload_turn_id(payload).map(str::to_owned);
    match value.get("type").and_then(Value::as_str) {
        Some("session_meta")
            if payload.get("id").and_then(Value::as_str) == Some(source.session_id.as_str()) =>
        {
            Ok(Fact::Other)
        }
        Some("turn_context" | "compacted") if payload.is_object() => Ok(Fact::Other),
        Some("response_item") => match payload.get("type").and_then(Value::as_str) {
            Some(
                "message"
                | "reasoning"
                | "agent_message"
                | "function_call"
                | "function_call_output"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "tool_search_call"
                | "tool_search_output",
            ) => Ok(Fact::Other),
            _ => Err("unknown_native_schema"),
        },
        Some("event_msg") => match payload.get("type").and_then(Value::as_str) {
            Some("task_started") => Ok(Fact::Opened(named())),
            Some("task_complete" | "turn_aborted")
                if payload.get("synthetic") != Some(&Value::Bool(true)) =>
            {
                Ok(Fact::Terminal(named()))
            }
            Some(
                "token_count"
                | "agent_reasoning"
                | "agent_message"
                | "user_message"
                | "item_completed"
                | "composer_ready"
                | "thread_settings_applied",
            ) => Ok(Fact::Other),
            _ => Err("unknown_native_schema"),
        },
        _ => Err("unknown_native_schema"),
    }
}

fn records(
    source: &SourceId,
    from: u64,
    through: u64,
) -> Result<Vec<CapturedRecord>, &'static str> {
    if from > through {
        return Err("invalid_native_bounds");
    }
    strict_parent_session(&source.path, &source.session_id)
        .map_err(|_| "source_session_mismatch")?;
    // Reopen permits dev-only renumbering; historical prefix verification belongs to the caller.
    let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let mut capture = SourceCapture::reopen(source.clone(), 0, empty_hash)
        .map_err(|_| "source_descriptor_mismatch")?;
    if capture.file_len().map_err(|_| "unreadable_native_source")? < through {
        return Err("truncated_native_source");
    }
    let mut records = Vec::new();
    while capture.read_through() < through {
        let before = capture.read_through();
        let remaining = through - capture.read_through();
        match capture.poll(remaining.min(MAX_READ_BYTES)) {
            CaptureOutcome::Batch(batch) => records.extend(batch.records),
            CaptureOutcome::Anomaly(_) => return Err("native_source_changed"),
        }
        if capture.read_through() == before {
            return Err("truncated_native_source");
        }
    }
    if capture.captured_through() != through {
        return Err("incomplete_native_record");
    }
    if from != 0
        && !records
            .iter()
            .any(|record| record.start == from || record.end == from)
    {
        return Err("nonboundary_native_offset");
    }
    if !capture
        .verify_prefix()
        .map_err(|_| "unreadable_native_source")?
        || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
    {
        return Err("native_prefix_changed");
    }
    records.retain(|record| record.start >= from);
    Ok(records)
}

#[cfg(test)]
#[path = "provenance_tests.rs"]
mod tests;
