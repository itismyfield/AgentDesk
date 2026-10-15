//! Native span evidence; fixed source and byte boundaries never select a live binding.

#[cfg(all(unix, test))]
use super::{
    Arc, CancelToken, CodexTuiTailResult, Sender, StreamMessage, discord_handoff_tail_options,
    tail_rollout_file_until_assistant_response_with_pane_busy_probe,
};
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
        match fact(&record, source)? {
            Fact::Opened(None) => return Err("anonymous_native_opener"),
            Fact::Opened(Some(id)) => {
                if anchor.is_some() {
                    return Err("multiple_native_openers");
                }
                anchor = Some(NativeTurnAnchor {
                    native_turn_id: id,
                    start: record.start,
                });
            }
            Fact::TurnRecord(id) if anchor.as_ref().is_some_and(|a| a.native_turn_id != id) => {
                return Err("foreign_native_record");
            }
            _ => {}
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
            Fact::TurnRecord(id) if id != anchor.native_turn_id => {
                return Err("foreign_native_record");
            }
            Fact::Other | Fact::TurnRecord(_) => {}
        }
    }
    Ok(None)
}

#[derive(PartialEq, Eq)]
enum Fact {
    Opened(Option<String>),
    Terminal(Option<String>),
    TurnRecord(String),
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
        // Metadata stays neutral only for the confirmed shape and native turn relationship.
        Some("token_usage_record") if payload.is_object() && named().is_some() => {
            Ok(Fact::TurnRecord(named().unwrap()))
        }
        Some("world_state")
            if payload.is_object()
                && payload.get("turn_id").is_none()
                && payload.get("full").is_none_or(Value::is_boolean) =>
        {
            Ok(Fact::Other)
        }
        Some("inter_agent_communication_metadata") => {
            match payload.get("trigger_turn").and_then(Value::as_bool) {
                Some(false) => Ok(Fact::Other),
                Some(true) => Err("agent_triggered_native_turn"),
                None => Err("unknown_native_schema"),
            }
        }
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
            Some("task_started") => {
                if payload.get("root_turn_id").is_some_and(|root| {
                    root.as_str() != payload.get("turn_id").and_then(Value::as_str)
                }) {
                    return Err("child_native_turn");
                }
                Ok(Fact::Opened(named()))
            }
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

/// Native evidence is observed before the delivery permission wait, without emitting a body.
#[cfg(unix)]
#[cfg(test)]
pub(crate) fn submitted_native_span(
    submitted: &crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::construction::SubmittedEpisode,
    floor: &crate::services::tui_o::store::codex_provenance::submit_floor::NativeFloor,
) -> Result<
    Option<crate::services::tui_prompt_dedupe::codex_verified::provenance::CodexEpisodeSpan>,
    &'static str,
> {
    let (source, through, hash) = floor.checkpoint();
    let mut capture = SourceCapture::reopen(source.clone(), through, hash)
        .map_err(|_| "native_floor_source_changed")?;
    if !capture
        .verify_prefix()
        .map_err(|_| "native_floor_source_changed")?
        || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
    {
        return Err("native_floor_source_changed");
    }
    let end = capture
        .file_len()
        .map_err(|_| "native_source_unavailable")?;
    let anchor = match scan_anchor(source, through, end) {
        Err("incomplete_native_record") => return Ok(None),
        other => other?,
    };
    let Some(anchor) = anchor else {
        return Ok(None);
    };
    let terminal = terminal_end(source, &anchor, end)?;
    let Some(terminal) = terminal else {
        return Ok(None);
    };
    let mut span = crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::construction::bind_episode_span(
        submitted, floor.execution(), anchor, floor.generation())?;
    span.end = Some(terminal);
    Ok(Some(span))
}

#[cfg(unix)]
#[cfg(test)]
pub(crate) fn tail_closed_episode(
    span: &crate::services::tui_prompt_dedupe::codex_verified::provenance::CodexEpisodeSpan,
    prompt: &str,
    sender: Sender<StreamMessage>,
    token: Arc<CancelToken>,
) -> Result<(), String> {
    let tmux = &span.execution.tmux_session;
    let mut options = discord_handoff_tail_options(tmux, Some(prompt));
    options.own_turn = Some(span.episode.native_turn_id.clone());
    let (read_result, outcome) = tail_rollout_file_until_assistant_response_with_pane_busy_probe(
        &span.execution.source.path,
        span.start,
        Some(span.execution.source.session_id.clone()),
        &sender,
        Some(Arc::clone(&token)),
        || crate::services::session_host::legacy_collapse::tmux_live_pane_bool(tmux),
        options,
    )?;
    crate::services::codex::emit_codex_tui_post_tail_handoff(
        CodexTuiTailResult {
            read_result,
            rollout_path: span.execution.source.path.clone(),
            final_offset: outcome.final_offset,
            session_id: outcome.session_id,
        },
        sender,
        Some(token),
        tmux,
    )
}
#[cfg(test)]
#[path = "provenance_tests.rs"]
mod tests;
