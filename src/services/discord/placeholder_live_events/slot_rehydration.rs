//! #3402: transcript-driven footer-panel slot REHYDRATION after a dcserver
//! restart.
//!
//! The footer panel's live-events slots (`placeholder_live_events`: Tasks /
//! Subagents) are in-memory only. A deploy SIGTERM wipes them mid-flight: any
//! subagent or background Bash task that was launched before the restart keeps
//! running in its tmux session, but the freshly-booted process never saw its
//! Start event, so the footer renders NOTHING for it. When such a task later
//! completes, the #3393 XML bridge pushes a terminal `SubagentEnd` /
//! `BackgroundTaskEnd` for an UNKNOWN `tool_use_id` → an intended slot no-op →
//! the task never appears at all.
//!
//! On the first time a channel's live-events state is (re)touched for a session
//! after boot, we scan the session transcript JSONL TAIL once for UNMATCHED
//! start/end pairs and re-inject synthesized Start `StatusEvent`s with their
//! REAL `tool_use_id`s. The slots then re-exist and the normal lifecycle (#3393
//! End bridge → ✓ → #3391 delivered-once eviction) just works.
//!
//! Bound: the scan reads only the transcript TAIL — at most the last
//! [`REHYDRATION_TAIL_MAX_BYTES`] bytes, further narrowed to start after the
//! last `isCompactSummary` compaction record when one lies inside that window.
//! NO pass ever reads more than the tail window (the compact-boundary probe
//! itself seeks into the window first): a pre-compaction subagent that is
//! still running is a vanishingly rare, unbounded-cost case the bounded
//! one-shot scan deliberately declines.
//!
//! Reuse: the JSONL is read with the SAME seek-from-offset / `read_line` access
//! pattern the idle transcript relay uses, and start records are classified with
//! the EXISTING `status_events_from_json_for_footer_mode` parser; terminal
//! `<task-notification>` user records are parsed with the SHARED
//! `tui_task_card::parse_task_notification`. No new JSONL/notification parser is
//! introduced.

use std::collections::HashSet;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use crate::services::agent_protocol::StatusEvent;

use super::PlaceholderLiveEvents;
use super::background_task_events::{notification_is_terminal, tool_use_id_from_notification};
use super::status_events::status_events_from_json_for_footer_mode;

/// Tail-scan bound: never read more than the last 256 KiB of a transcript when
/// no compaction boundary is found nearer EOF. A restart's still-running
/// subagents wrote their Start record within the active turn, which is well
/// inside this window; a larger window would only re-scan finished backlog and
/// inflate the one-shot cost.
const REHYDRATION_TAIL_MAX_BYTES: u64 = 256 * 1024;

/// One-shot guard: `"{channel_id}#{session}"` keys already rehydrated this
/// process lifetime (session = Claude session_id when known, else transcript
/// path — both stable per session), so the bounded tail scan never re-runs.
static REHYDRATED_SESSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Outcome of one rehydration pass — surfaced for the INFO log line and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct RehydrationOutcome {
    pub(in crate::services::discord) subagents: usize,
    pub(in crate::services::discord) background_tasks: usize,
}

impl RehydrationOutcome {
    pub(in crate::services::discord) fn restored_any(&self) -> bool {
        self.subagents > 0 || self.background_tasks > 0
    }
}

impl PlaceholderLiveEvents {
    /// #3402: rehydrate footer-panel slots for this (channel, session) exactly
    /// once after boot. The one-shot guard keeps the bounded tail scan to a
    /// single run per session; the scan + re-injection (and the footer-mode
    /// gate) live in [`Self::rehydrate_slots_from_transcript_tail`].
    pub(in crate::services::discord) fn rehydrate_slots_once_for_session(
        &self,
        channel_id: ChannelId,
        session_id: Option<&str>,
        transcript_path: &Path,
    ) -> RehydrationOutcome {
        let session_marker = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| transcript_path.to_string_lossy().into_owned());
        let guard_key = format!("{}#{session_marker}", channel_id.get());
        let first = REHYDRATED_SESSIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(guard_key);
        if !first {
            return RehydrationOutcome::default();
        }
        self.rehydrate_slots_from_transcript_tail(channel_id, transcript_path)
    }

    /// #3402: scan the transcript tail at `transcript_path` for in-flight
    /// subagent / background-task starts whose terminal notification has not yet
    /// landed, and re-inject their Start `StatusEvent`s into `channel_id`'s slot
    /// state so the footer re-renders them. Footer-mode gated (no-op in legacy
    /// separate-panel mode). Idempotent at the injection layer: a start whose
    /// `tool_use_id` already has a live slot is skipped, so a second call adds no
    /// duplicates even without the caller's one-shot guard.
    pub(in crate::services::discord) fn rehydrate_slots_from_transcript_tail(
        &self,
        channel_id: ChannelId,
        transcript_path: &Path,
    ) -> RehydrationOutcome {
        self.rehydrate_slots_from_transcript_tail_for_footer_mode(
            channel_id,
            transcript_path,
            super::super::single_message_panel::enabled(),
        )
    }

    /// Footer-mode-injectable variant (mirrors the `status_events` module's
    /// `_for_footer_mode` test surface): legacy mode is a no-op; footer mode runs
    /// the bounded tail scan + re-injection. Keeps the rehydration tests free of
    /// the process-global `single_message_panel::enabled()` env cache.
    pub(in crate::services::discord) fn rehydrate_slots_from_transcript_tail_for_footer_mode(
        &self,
        channel_id: ChannelId,
        transcript_path: &Path,
        footer_mode_enabled: bool,
    ) -> RehydrationOutcome {
        if !footer_mode_enabled {
            return RehydrationOutcome::default();
        }
        let starts = match scan_transcript_tail_for_unmatched_starts(transcript_path) {
            Ok(starts) => starts,
            Err(error) => {
                tracing::debug!(
                    channel_id = channel_id.get(),
                    transcript_path = %transcript_path.display(),
                    error = %error,
                    "#3402: slot rehydration tail scan skipped"
                );
                return RehydrationOutcome::default();
            }
        };
        if starts.is_empty() {
            return RehydrationOutcome::default();
        }

        let existing_ids = self.live_slot_tool_use_ids(channel_id);
        let mut outcome = RehydrationOutcome::default();
        let mut events: Vec<StatusEvent> = Vec::new();
        for start in starts {
            // Idempotency belt-and-suspenders: never re-inject a start whose id
            // already backs a live slot (a non-background `SubagentStart` always
            // pushes a fresh slot, so the upsert-by-id semantics alone do NOT
            // dedupe foreground subagents — this gate does).
            if existing_ids.contains(&start.tool_use_id) {
                continue;
            }
            match start.kind {
                StartKind::Subagent => outcome.subagents += 1,
                StartKind::BackgroundTask => outcome.background_tasks += 1,
            }
            events.push(start.event);
        }
        if outcome.restored_any() {
            self.push_status_events(channel_id, events);
            tracing::info!(
                channel_id = channel_id.get(),
                restored_subagents = outcome.subagents,
                restored_background_tasks = outcome.background_tasks,
                "#3402: rehydrated footer panel slots from transcript tail after restart"
            );
        }
        outcome
    }

    /// `tool_use_id`s already backing a live (running) footer slot in this
    /// channel — used to skip re-injecting a start the running process already
    /// tracks.
    fn live_slot_tool_use_ids(&self, channel_id: ChannelId) -> HashSet<String> {
        use super::completion_footer::SlotKey;
        use super::task_panel::{task_tool_slot_identity, task_tool_slot_is_unfinished_background};
        let Some(entry) = self.status_by_channel.get(&channel_id) else {
            return HashSet::new();
        };
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // UNFINISHED subagent slots + UNFINISHED background-task slots whose
        // identity is a `tool_use_id` — the live set a re-injection must skip.
        let subagent_ids = guard
            .subagents
            .iter()
            .filter(|slot| !slot.is_terminal())
            .map(|slot| slot.identity());
        let task_ids = guard
            .tasks
            .iter()
            .filter(|slot| task_tool_slot_is_unfinished_background(slot))
            .map(task_tool_slot_identity);
        subagent_ids
            .chain(task_ids)
            .filter_map(|key| match key {
                SlotKey::ToolUseId(id) => Some(id),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartKind {
    Subagent,
    BackgroundTask,
}

/// A synthesized start to (re)inject, paired with the `tool_use_id` it opens so
/// the caller can dedupe against already-live slots.
struct PendingStart {
    tool_use_id: String,
    kind: StartKind,
    event: StatusEvent,
}

/// Scans the transcript TAIL (bounded — see [`REHYDRATION_TAIL_MAX_BYTES`] and
/// the compact-boundary rule in [`tail_start_offset`]) and returns the synthesized
/// Start `StatusEvent`s for subagent / background-Bash launches whose terminal
/// notification has NOT yet landed in the tail, in transcript order.
fn scan_transcript_tail_for_unmatched_starts(
    transcript_path: &Path,
) -> Result<Vec<PendingStart>, String> {
    let start_offset = tail_start_offset(transcript_path)?;
    let mut file = std::fs::File::open(transcript_path)
        .map_err(|error| format!("open transcript {}: {error}", transcript_path.display()))?;
    file.seek(SeekFrom::Start(start_offset))
        .map_err(|error| format!("seek transcript {}: {error}", transcript_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    // Insertion-ordered start candidates keyed by tool_use_id, plus the set of
    // ids whose terminal record was seen — the unmatched set is starts − ended.
    let mut starts: Vec<PendingStart> = Vec::new();
    let mut ended: HashSet<String> = HashSet::new();

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read transcript {}: {error}", transcript_path.display()))?;
        if bytes_read == 0 {
            break;
        }
        let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        collect_starts_from_record(&json, &mut starts);
        collect_terminations_from_record(&json, &mut ended);
    }

    Ok(starts
        .into_iter()
        .filter(|start| !ended.contains(&start.tool_use_id))
        .collect())
}

/// Resolves the tail start offset: EOF − [`REHYDRATION_TAIL_MAX_BYTES`],
/// advanced to the last `isCompactSummary` record found WITHIN that window.
/// Records before this offset are never scanned (the documented bound) — the
/// compact-boundary probe itself is seek-bounded to the same window, so no
/// pass over the transcript is ever larger than the tail.
fn tail_start_offset(transcript_path: &Path) -> Result<u64, String> {
    let file_len = std::fs::metadata(transcript_path)
        .map_err(|error| format!("stat transcript {}: {error}", transcript_path.display()))?
        .len();
    let size_floor = file_len.saturating_sub(REHYDRATION_TAIL_MAX_BYTES);
    let compact_floor =
        last_compact_boundary_offset_within_tail(transcript_path, size_floor)?.unwrap_or(0);
    Ok(size_floor.max(compact_floor))
}

/// Offset of the start of the LAST record carrying `isCompactSummary: true`
/// (Claude Code's compaction marker) at or after `tail_floor`. `None` when no
/// compaction record exists inside the window. A boundary BEFORE `tail_floor`
/// is irrelevant: `tail_start_offset` takes the max with the size floor, so
/// only an in-window boundary can move the result. Seeking may land mid-line;
/// that partial line fails JSON parse and is skipped, same as the main scan.
fn last_compact_boundary_offset_within_tail(
    transcript_path: &Path,
    tail_floor: u64,
) -> Result<Option<u64>, String> {
    let mut file = std::fs::File::open(transcript_path)
        .map_err(|error| format!("open transcript {}: {error}", transcript_path.display()))?;
    file.seek(SeekFrom::Start(tail_floor))
        .map_err(|error| format!("seek transcript {}: {error}", transcript_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut offset = tail_floor;
    let mut boundary: Option<u64> = None;
    loop {
        line.clear();
        let line_start = offset;
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read transcript {}: {error}", transcript_path.display()))?;
        if bytes_read == 0 {
            break;
        }
        offset = offset.saturating_add(bytes_read as u64);
        if let Ok(json) = serde_json::from_str::<Value>(line.trim())
            && record_is_compact_summary(&json)
        {
            boundary = Some(line_start);
        }
    }
    Ok(boundary)
}

/// `true` for a Claude Code compaction boundary record. The marker is the
/// top-level `isCompactSummary` flag; the underscored spelling is accepted
/// defensively.
fn record_is_compact_summary(value: &Value) -> bool {
    ["isCompactSummary", "is_compact_summary"]
        .into_iter()
        .any(|key| value.get(key).and_then(Value::as_bool).unwrap_or(false))
}

/// Pulls the subagent / background-task Start events out of one transcript
/// record using the EXISTING footer-mode parser, retaining only the id-bearing
/// `SubagentStart` / `BackgroundTaskStart` variants (those are the slot-opening
/// starts a restart must restore).
fn collect_starts_from_record(value: &Value, starts: &mut Vec<PendingStart>) {
    for event in status_events_from_json_for_footer_mode(value, true) {
        match event {
            StatusEvent::SubagentStart {
                tool_use_id: Some(ref id),
                ..
            } if !id.trim().is_empty() => {
                let tool_use_id = id.clone();
                push_unique_start(starts, tool_use_id, StartKind::Subagent, event);
            }
            StatusEvent::BackgroundTaskStart {
                tool_use_id: ref id,
                ..
            } if !id.trim().is_empty() => {
                let tool_use_id = id.clone();
                push_unique_start(starts, tool_use_id, StartKind::BackgroundTask, event);
            }
            _ => {}
        }
    }
}

/// Records the `tool_use_id`s whose TERMINAL record appears in the tail, so
/// their start is treated as matched (completed) and NOT rehydrated:
///   - a `<task-notification>` `user` record with a terminal status (the #3393
///     bridge path: background commands and subagent completions reach the
///     transcript only as this XML), parsed with the shared `tui_task_card`
///     parser, and
///   - a `user` record's `tool_result` block for a Task tool-use id (the Task's
///     own synchronous result).
fn collect_terminations_from_record(value: &Value, ended: &mut HashSet<String>) {
    if value.get("type").and_then(Value::as_str) == Some("user") {
        if let Some(text) = user_record_text(value)
            && text.contains("<task-notification")
        {
            let parsed = super::super::tui_task_card::parse_task_notification(&text);
            if parsed
                .status
                .as_deref()
                .is_some_and(notification_is_terminal)
                && let Some(id) = parsed.tool_use_id
            {
                ended.insert(id);
            }
        }
        for id in tool_result_tool_use_ids(value) {
            ended.insert(id);
        }
    }
    // A `system` task_notification record (the never-occurring stream-json path)
    // carries the id top-level; harmless to honor it as a termination too.
    if value.get("subtype").and_then(Value::as_str) == Some("task_notification")
        && value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(notification_is_terminal)
        && let Some(id) = tool_use_id_from_notification(value)
    {
        ended.insert(id);
    }
}

/// Concatenated text of a `user` record's `message.content` (string or array of
/// `{text}` blocks) — where an injected `<task-notification>` XML lands.
fn user_record_text(value: &Value) -> Option<String> {
    match value.get("message")?.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

/// `tool_use_id`s of any `tool_result` blocks in a `user` record (the Task's
/// own result terminates a foreground subagent slot).
fn tool_result_tool_use_ids(value: &Value) -> Vec<String> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
                .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pushes a start, collapsing repeats of the same `tool_use_id` to the FIRST
/// occurrence (a re-observed start record must not double-count).
fn push_unique_start(
    starts: &mut Vec<PendingStart>,
    tool_use_id: String,
    kind: StartKind,
    event: StatusEvent,
) {
    if starts.iter().any(|start| start.tool_use_id == tool_use_id) {
        return;
    }
    starts.push(PendingStart {
        tool_use_id,
        kind,
        event,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn compact_record() -> String {
        r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"compacted"}}"#
            .to_string()
    }

    fn filler_record() -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(512)
        )
    }

    fn write_lines(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        file
    }

    // #3402 codex review: the compact-boundary probe must be seek-bounded to
    // the tail window — a full-file scan would FIND this pre-floor boundary
    // and return Some, so this pins the bounded contract directly.
    #[test]
    fn compact_probe_ignores_boundary_before_tail_floor() {
        let lines = vec![compact_record(), filler_record(), filler_record()];
        let transcript = write_lines(&lines);
        let floor = (compact_record().len() + 1) as u64;
        assert_eq!(
            last_compact_boundary_offset_within_tail(transcript.path(), floor).unwrap(),
            None,
            "boundary strictly before the floor must not be visible"
        );
    }

    #[test]
    fn compact_probe_finds_boundary_inside_window_despite_midline_seek() {
        let first = filler_record();
        let lines = vec![first.clone(), filler_record(), compact_record()];
        let transcript = write_lines(&lines);
        // Seek lands mid-way through the SECOND filler line: the partial line
        // fails JSON parse and is skipped, the in-window boundary still lands.
        let floor = (first.len() + 1 + 64) as u64;
        let expected = (2 * (first.len() + 1)) as u64;
        assert_eq!(
            last_compact_boundary_offset_within_tail(transcript.path(), floor).unwrap(),
            Some(expected),
        );
    }

    // tail_start_offset on a transcript larger than the window with the only
    // compact record BEFORE the size floor: the result is the size floor (the
    // out-of-window boundary neither moves the offset nor gets scanned).
    #[test]
    fn tail_start_offset_clamps_to_size_floor_when_boundary_is_out_of_window() {
        let mut lines = vec![compact_record()];
        // ~600 KiB of filler so the size floor lands well past the boundary.
        for _ in 0..1200 {
            lines.push(filler_record());
        }
        let transcript = write_lines(&lines);
        let file_len = std::fs::metadata(transcript.path()).unwrap().len();
        assert!(
            file_len > REHYDRATION_TAIL_MAX_BYTES,
            "fixture must exceed the window"
        );
        assert_eq!(
            tail_start_offset(transcript.path()).unwrap(),
            file_len - REHYDRATION_TAIL_MAX_BYTES,
        );
    }
}
