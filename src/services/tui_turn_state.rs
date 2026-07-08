use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;

const TURN_STATE_TAIL_BYTES: u64 = 64 * 1024;

#[path = "tui_turn_state/completion_scan.rs"]
mod completion_scan;
#[cfg(test)]
use self::completion_scan::{
    envelope_is_turn_end_terminator, is_interactive_mode_housekeeping_type,
    is_torn_trailing_fragment,
};
pub(crate) use self::completion_scan::{
    jsonl_completion_scan_idle, jsonl_strict_terminator_idle, jsonl_turn_end_terminator_idle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiTurnState {
    Idle,
    Streaming,
    UserSubmitted,
    Unknown,
}

impl TuiTurnState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Streaming => "streaming",
            Self::UserSubmitted => "user_submitted",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn is_busy(self) -> bool {
        matches!(self, Self::Streaming | Self::UserSubmitted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiReadyState {
    Ready,
    Busy,
    Unknown,
}

impl TuiReadyState {
    pub(crate) fn from_turn_state(state: TuiTurnState) -> Self {
        match state {
            TuiTurnState::Idle => Self::Ready,
            TuiTurnState::Streaming | TuiTurnState::UserSubmitted => Self::Busy,
            TuiTurnState::Unknown => Self::Unknown,
        }
    }

    pub(crate) fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Unknown => "unknown",
        }
    }
}

/// #3981/#4024: a busy JSONL `UserSubmitted` turn-state is only reclaimed as a
/// stale stranded turn once runtime activity has been frozen for this long and
/// the live pane shows the at-rest prompt marker.
///
/// Aligned with the proven `DEAD_WATCHER_PROVEN_DEAD_SECS = 600` reclaim floor
/// (`inflight::rebind_reap`, #3879): both classify "the pane may still be
/// alive, but runtime activity has gone quiescent past a long inter-activity
/// ceiling".
// #4353: a plain `i64`. `destructive_cancel_gate`, `manual_rebind_output_path`
// and `dispatched_sessions` all read it unguarded, so `cfg(unix)` broke non-unix
// builds without gating anything platform-specific.
pub(crate) const STALE_USER_SUBMITTED_RECLAIM_SECS: i64 = 600;

/// #3981/#4024: decide whether a busy JSONL `UserSubmitted` turn-state is a
/// stale stranded turn rather than a live submission still awaiting its first
/// assistant flush.
///
/// Reclaim requires BOTH runtime-activity quiescence and the live pane at-rest
/// prompt marker. `Streaming` is unconditionally live because an assistant
/// envelope already exists.
#[cfg(unix)]
pub(crate) fn user_submitted_is_stale_stranded(
    state: TuiTurnState,
    activity_age_secs: i64,
    prompt_marker_detected: bool,
) -> bool {
    matches!(state, TuiTurnState::UserSubmitted)
        && activity_age_secs >= STALE_USER_SUBMITTED_RECLAIM_SECS
        && prompt_marker_detected
}

/// #3981/#4024: age in seconds since the most recent runtime-activity mtime
/// (relay jsonl / `.generation` / rollout) for this tmux session.
///
/// Returns `0` when no activity is observable, so the AND gate in
/// [`user_submitted_is_stale_stranded`] never reclaims a turn without positive
/// quiescence evidence.
#[cfg(unix)]
pub(crate) fn runtime_activity_age_secs(tmux_session_name: &str) -> i64 {
    let activity_nanos =
        crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(tmux_session_name);
    if activity_nanos <= 0 {
        return 0;
    }
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0);
    now_nanos.saturating_sub(activity_nanos) / 1_000_000_000
}

pub(crate) trait TuiTurnStateProbe {
    fn observe(&self) -> TuiTurnState;
}

pub(crate) struct JsonlTurnStateProbe<'a> {
    provider: &'a ProviderKind,
    path: &'a Path,
}

impl<'a> JsonlTurnStateProbe<'a> {
    pub(crate) fn new(provider: &'a ProviderKind, path: &'a Path) -> Self {
        Self { provider, path }
    }
}

impl TuiTurnStateProbe for JsonlTurnStateProbe<'_> {
    fn observe(&self) -> TuiTurnState {
        observe_provider_jsonl_turn_state(self.provider, self.path)
    }
}

pub(crate) fn observe_provider_jsonl_turn_state(
    provider: &ProviderKind,
    path: &Path,
) -> TuiTurnState {
    match provider {
        ProviderKind::Claude => observe_claude_jsonl_turn_state(path),
        ProviderKind::Codex => observe_codex_jsonl_turn_state(path),
        _ => TuiTurnState::Unknown,
    }
}

pub(crate) fn provider_runtime_has_structured_jsonl_turn_state(
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
) -> bool {
    let provider_has_jsonl = matches!(provider, ProviderKind::Claude | ProviderKind::Codex);
    if !provider_has_jsonl {
        return false;
    }
    // Phase 1 of the claude-e rollout: `ClaudeEAdapter` is grouped with the
    // non-JSONL runtimes because the adapter streams stream-json directly
    // through `sender` (no on-disk transcript file — `output_path` stays
    // empty on the `RuntimeHandoff::ClaudeEAdapter` variant). The TUI
    // turn-state probes read JSONL from disk, so they have nothing to
    // poll for this adapter. See `docs/claude-e-rollout/`.
    !matches!(
        runtime_kind,
        Some(
            RuntimeHandoffKind::LegacyTmuxWrapper
                | RuntimeHandoffKind::ProcessBackend
                | RuntimeHandoffKind::ClaudeEAdapter
        )
    )
}

pub(crate) fn pane_ready_fallback_allowed(
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
) -> bool {
    !provider_runtime_has_structured_jsonl_turn_state(provider, runtime_kind)
}

pub(crate) fn jsonl_ready_for_input(
    provider: &ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
    path: &Path,
    consumed_offset: Option<u64>,
) -> Option<TuiReadyState> {
    if !provider_runtime_has_structured_jsonl_turn_state(provider, runtime_kind) {
        return None;
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some(TuiReadyState::Unknown);
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return Some(TuiReadyState::Unknown);
    }
    let offset_behind = consumed_offset.is_some_and(|offset| metadata.len() > offset);
    // When the relay has not yet consumed the full transcript we keep the
    // session marked Busy by default — a partially-relayed assistant stream
    // must not be mistaken for an idle turn. The exception is a fully
    // written terminator envelope (`result`, `turn.completed`, or
    // `system/turn_duration|stop_hook_summary|init`): the turn is over and
    // the remaining bytes are just trailing terminator metadata the relay
    // will deliver shortly. Holding Busy in that case strands the idle-queue
    // drain even though the next input can safely be sent (#2790 /
    // quick-exit + #2789 regression).
    //
    // The override must inspect the **latest complete JSON line only**. The
    // standard classifier `observe_provider_jsonl_turn_state` falls back
    // through partial trailing fragments (e.g. an early `{"ty` slice of a
    // new `user` envelope) to the previous complete line — so it would
    // misreport a turn that has *just been re-started* as still Idle. The
    // strict check below refuses to fall through partial lines and so
    // protects against the in-progress-new-turn race.
    //
    // When `offset_behind` is true we trust the strict predicate as the sole
    // source of truth and skip the regular observer pass; running observer
    // again would re-read the file and could fall through a partial line
    // written between the two reads, undoing the guarantee we just made.
    if offset_behind {
        return if jsonl_strict_terminator_idle(provider, path) {
            Some(TuiReadyState::Ready)
        } else {
            Some(TuiReadyState::Busy)
        };
    }
    Some(TuiReadyState::from_turn_state(
        observe_provider_jsonl_turn_state(provider, path),
    ))
}

pub(crate) fn runtime_binding_ready_for_input(
    provider: &ProviderKind,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    require_consumed: bool,
) -> Option<TuiReadyState> {
    if !provider_runtime_has_structured_jsonl_turn_state(provider, Some(binding.runtime_kind)) {
        return None;
    }
    // Phase 1 of the claude-e rollout. The adapter streams JSONL
    // through the in-memory `sender` channel and does NOT write a
    // transcript file, so there is no on-disk path to probe.
    // Phase 1.x may add a sidecar transcript for recovery-engine
    // support; at that point this arm would return that path
    // instead of falling through to `None`.
    let path = match binding.runtime_kind {
        RuntimeHandoffKind::ClaudeTui => Path::new(binding.relay_output_path()),
        RuntimeHandoffKind::CodexTui => Path::new(&binding.output_path),
        RuntimeHandoffKind::LegacyTmuxWrapper
        | RuntimeHandoffKind::ProcessBackend
        | RuntimeHandoffKind::ClaudeEAdapter => return None,
    };
    jsonl_ready_for_input(
        provider,
        Some(binding.runtime_kind),
        path,
        require_consumed.then_some(binding.last_offset),
    )
}

pub(crate) fn runtime_binding_turn_state(
    provider: &ProviderKind,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<TuiTurnState> {
    if !provider_runtime_has_structured_jsonl_turn_state(provider, Some(binding.runtime_kind)) {
        return None;
    }
    let path = match binding.runtime_kind {
        RuntimeHandoffKind::ClaudeTui => Path::new(binding.relay_output_path()),
        RuntimeHandoffKind::CodexTui => Path::new(&binding.output_path),
        RuntimeHandoffKind::LegacyTmuxWrapper
        | RuntimeHandoffKind::ProcessBackend
        | RuntimeHandoffKind::ClaudeEAdapter => return None,
    };
    Some(observe_provider_jsonl_turn_state(provider, path))
}

pub(crate) fn observe_claude_jsonl_turn_state(path: &Path) -> TuiTurnState {
    observe_jsonl_turn_state(
        path,
        claude_envelope_turn_state,
        claude_partial_turn_state,
        MalformedJsonlLinePolicy::FallbackToPrevious,
    )
}

pub(crate) fn observe_codex_jsonl_turn_state(path: &Path) -> TuiTurnState {
    observe_jsonl_turn_state(
        path,
        codex_envelope_turn_state,
        |_| None,
        MalformedJsonlLinePolicy::ReturnUnknown,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MalformedJsonlLinePolicy {
    FallbackToPrevious,
    ReturnUnknown,
}

fn observe_jsonl_turn_state(
    path: &Path,
    classify: fn(&Value) -> Option<TuiTurnState>,
    classify_partial: fn(&str) -> Option<TuiTurnState>,
    malformed_policy: MalformedJsonlLinePolicy,
) -> TuiTurnState {
    let Ok(lines) = read_recent_jsonl_lines(path) else {
        return TuiTurnState::Unknown;
    };
    if lines.is_empty() {
        return TuiTurnState::Idle;
    }
    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let json = match serde_json::from_str::<Value>(trimmed) {
            Ok(json) => json,
            Err(_) => {
                if let Some(state) = classify_partial(trimmed) {
                    return state;
                }
                if malformed_policy == MalformedJsonlLinePolicy::FallbackToPrevious {
                    continue;
                }
                return TuiTurnState::Unknown;
            }
        };
        if let Some(state) = classify(&json) {
            return state;
        }
    }
    TuiTurnState::Unknown
}

fn read_recent_jsonl_lines(path: &Path) -> Result<Vec<String>, std::io::Error> {
    Ok(read_recent_jsonl_window(path, TURN_STATE_TAIL_BYTES)?.lines)
}

/// Result of a bounded tail read: the parsed lines plus whether the window
/// reached the start of the file. `window_covers_file` is `false` when bytes
/// precede the window — i.e. there may be an older terminator we did not read.
struct JsonlTailWindow {
    lines: Vec<String>,
    window_covers_file: bool,
}

fn read_recent_jsonl_window(
    path: &Path,
    window_bytes: u64,
) -> Result<JsonlTailWindow, std::io::Error> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JsonlTailWindow {
                lines: Vec::new(),
                window_covers_file: true,
            });
        }
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(window_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut lines = buf.lines().map(ToString::to_string).collect::<Vec<_>>();
    // When the window does not begin at byte 0 the first "line" is almost
    // certainly a fragment of an envelope that started before the window, so
    // we drop it. That dropped fragment also means the window does not cover
    // the whole file.
    let dropped_partial_head = start > 0 && !buf.starts_with('\n') && !lines.is_empty();
    if dropped_partial_head {
        lines.remove(0);
    }
    Ok(JsonlTailWindow {
        lines,
        window_covers_file: start == 0,
    })
}

/// #3221: the exact set of trailing `[Request interrupted by user…]` markers
/// claude writes when a turn is aborted by a session-preserving interrupt (ESC
/// for the TUI / stream-json `control_request{interrupt}` for the wrapper,
/// #3207). Verified from real transcripts — these are the only two variants.
///
/// Matching is by EXACT equality (not prefix), so a user-TYPED prompt like
/// `[Request interrupted by user story…]` can never false-positive. A future
/// unknown variant becoming a false-NEGATIVE is safe (the turn merely stays busy
/// and self-heals on the next turn); a false-POSITIVE is not (it would drop a
/// real in-flight user turn). Add new variants here as they are observed.
const CLAUDE_INTERRUPT_MARKERS: [&str; 2] = [
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
];

/// #3221: detect the trailing `[Request interrupted by user]` envelope claude
/// writes when a turn is aborted by a session-preserving interrupt (ESC for the
/// TUI / stream-json `control_request{interrupt}` for the wrapper, #3207).
///
/// The marker is a `type=user` envelope whose `message.content` is effectively a
/// SINGLE text block that IS exactly one of `CLAUDE_INTERRUPT_MARKERS` (the
/// plain and the `… for tool use` variants). Matching is deliberately narrow on
/// two axes:
///   - single-block only: a multi-block user message that merely *includes* a
///     marker-looking block alongside real content is rejected, so a genuine
///     in-flight user turn is never dropped;
///   - exact equality (not prefix): a user-typed prompt such as
///     `[Request interrupted by user story…]` never trips it.
/// `content` may also be a plain string (older shape); the same exact check
/// applies. Coincidental conversation text or `tool_result` payloads that merely
/// mention "interrupted" therefore never match.
fn claude_user_envelope_is_interrupt_marker(json: &Value) -> bool {
    let Some(content) = json
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return false;
    };
    let text = match content {
        // Plain-string content: the whole string must be the marker.
        Value::String(text) => text.as_str(),
        // Array content: must be exactly one block, and that block must be a
        // `text` block whose text is the marker. Rejecting >1 block prevents a
        // multi-block message that merely embeds a marker-looking block from
        // being misclassified as a turn terminator.
        Value::Array(blocks) => {
            let [block] = blocks.as_slice() else {
                return false;
            };
            if block.get("type").and_then(Value::as_str) != Some("text") {
                return false;
            }
            let Some(text) = block.get("text").and_then(Value::as_str) else {
                return false;
            };
            text
        }
        _ => return false,
    };
    let text = text.trim();
    CLAUDE_INTERRUPT_MARKERS
        .iter()
        .any(|marker| text == *marker)
}

fn claude_envelope_turn_state(json: &Value) -> Option<TuiTurnState> {
    match json.get("type").and_then(Value::as_str)? {
        "result" => Some(TuiTurnState::Idle),
        "assistant" => Some(TuiTurnState::Streaming),
        // #3221: a turn aborted via a session-preserving interrupt leaves a
        // trailing `[Request interrupted by user]` user envelope as the newest
        // entry. It is `type=user` but marks the turn as ENDED, not a fresh
        // submission. Classifying it `UserSubmitted` left the JSONL-authoritative
        // busy gate (#3208) reporting the stopped turn as in-flight forever, so
        // the next user message was wrongly queued as `*_tui_busy_pre_submit`
        // after a ⏳-removal / `!stop` / `/stop` / watchdog stop. Treat the
        // interrupt marker as Idle so the next input starts a fresh turn.
        "user" if claude_user_envelope_is_interrupt_marker(json) => Some(TuiTurnState::Idle),
        "user" => Some(TuiTurnState::UserSubmitted),
        // `permission-mode` envelopes (e.g. `bypassPermissions` adoption after
        // a fresh session start triggered by hard_reset or `/compact`) are not
        // turn-state signals. If we returned `None` here, the tail walker
        // would skip them and fall back to the previous turn's `result`
        // envelope — declaring the new turn already idle and tearing the
        // watcher down before the first assistant line gets written
        // (#2712, #2716). Map them to `Unknown` so the gate keeps waiting
        // for a real turn-state envelope.
        //
        // NOTE (#3030): we intentionally do NOT generalize this to the whole
        // `is_interactive_mode_housekeeping_type` family here. In the *standard*
        // observer (`observe_jsonl_turn_state`) `None` means "walk back to the
        // prior envelope" while `Unknown` means "stop and report Unknown". A
        // completed turn legitimately trails housekeeping like `{"type":"mode"}`
        // (see `structured_jsonl_ready_terminator_with_trailing_housekeeping`),
        // and the observer must walk back across it to the real terminator and
        // report Idle. The `permission-mode` special case is the lone exception
        // because it appears on a *fresh session restart* with only a stale
        // previous `result` beneath it (the #2712 race). The structural
        // mode-family hardening for #3030 lives in the strict offset-behind scan
        // and the torn-write skip, where `None`/`Unknown` are treated
        // identically (both skipped), so it cannot regress this walk-back.
        "permission-mode" => Some(TuiTurnState::Unknown),
        "system" => match json.get("subtype").and_then(Value::as_str) {
            Some("turn_duration" | "stop_hook_summary" | "init") => Some(TuiTurnState::Idle),
            _ => None,
        },
        _ => None,
    }
}

fn claude_partial_turn_state(line: &str) -> Option<TuiTurnState> {
    if !line.trim_start().starts_with('{') {
        return None;
    }
    match top_level_string_field_fragment(line, "type")?.as_str() {
        "assistant" => Some(TuiTurnState::Streaming),
        "user" => Some(TuiTurnState::UserSubmitted),
        "result" => Some(TuiTurnState::Idle),
        // Mirror the full-envelope classifier: do not fall back through
        // permission-mode lines (#2712, #2716).
        "permission-mode" => Some(TuiTurnState::Unknown),
        "system" => match top_level_string_field_fragment(line, "subtype")?.as_str() {
            "turn_duration" | "stop_hook_summary" | "init" => Some(TuiTurnState::Idle),
            _ => None,
        },
        // As with `claude_envelope_turn_state`, the broader mode-family
        // hardening (#3030) is intentionally NOT applied here — the partial
        // classifier feeds the same walk-back observer, which must reach the
        // real terminator beneath a completed turn's trailing `mode` line.
        _ => None,
    }
}

fn top_level_string_field_fragment(line: &str, expected_key: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut depth = 0i32;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                index += 1;
            }
            b'"' if depth == 1 => {
                let (key, next_index, complete_key) = parse_json_string_fragment(bytes, index + 1);
                if !complete_key {
                    return None;
                }
                index = skip_json_whitespace(bytes, next_index);
                if bytes.get(index) != Some(&b':') {
                    continue;
                }
                index = skip_json_whitespace(bytes, index + 1);
                if key == expected_key {
                    if bytes.get(index) != Some(&b'"') {
                        return None;
                    }
                    let (value, _, complete_value) = parse_json_string_fragment(bytes, index + 1);
                    return complete_value.then_some(value);
                }
            }
            b'"' => {
                let (_, next_index, _) = parse_json_string_fragment(bytes, index + 1);
                index = next_index;
            }
            _ => {
                index += 1;
            }
        }
    }
    None
}

fn skip_json_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        index += 1;
    }
    index
}

fn parse_json_string_fragment(bytes: &[u8], mut index: usize) -> (String, usize, bool) {
    let mut value = String::new();
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                if let Some(next) = bytes.get(index + 1) {
                    value.push(*next as char);
                    index += 2;
                } else {
                    return (value, bytes.len(), false);
                }
            }
            b'"' => return (value, index + 1, true),
            byte => {
                value.push(byte as char);
                index += 1;
            }
        }
    }
    (value, index, false)
}

fn codex_envelope_turn_state(json: &Value) -> Option<TuiTurnState> {
    match json.get("type").and_then(Value::as_str)? {
        "session_meta" | "thread.started" => Some(TuiTurnState::Idle),
        "turn.completed" => Some(TuiTurnState::Idle),
        "event_msg" => codex_event_msg_turn_state(json),
        "response_item" => codex_response_item_turn_state(json),
        "item.started" => Some(codex_item_turn_state(json, false)),
        "item.completed" => Some(codex_item_turn_state(json, true)),
        _ => None,
    }
}

fn codex_event_msg_turn_state(json: &Value) -> Option<TuiTurnState> {
    let payload = json.get("payload")?;
    match payload.get("type").and_then(Value::as_str)? {
        "task_complete" => Some(TuiTurnState::Idle),
        "token_count" | "agent_reasoning" => Some(TuiTurnState::Streaming),
        _ => Some(TuiTurnState::Streaming),
    }
}

fn codex_response_item_turn_state(json: &Value) -> Option<TuiTurnState> {
    let payload = json.get("payload")?;
    match payload.get("type").and_then(Value::as_str)? {
        "message" => match payload.get("role").and_then(Value::as_str) {
            Some("user") => Some(TuiTurnState::UserSubmitted),
            Some("assistant") => Some(TuiTurnState::Streaming),
            _ => None,
        },
        "function_call"
        | "custom_tool_call"
        | "function_call_output"
        | "custom_tool_call_output"
        | "reasoning" => Some(TuiTurnState::Streaming),
        _ => None,
    }
}

fn codex_item_turn_state(json: &Value, completed: bool) -> TuiTurnState {
    let item_type = json
        .get("item")
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str);
    match item_type {
        Some("user_message") | Some("user") => TuiTurnState::UserSubmitted,
        Some("agent_message") if completed => TuiTurnState::Idle,
        Some("agent_message") => TuiTurnState::Streaming,
        _ => TuiTurnState::Streaming,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), lines.join("\n")).unwrap();
        file
    }

    #[cfg(unix)]
    #[test]
    fn streaming_is_never_reclaimed_even_when_old_and_marker_present() {
        assert!(!user_submitted_is_stale_stranded(
            TuiTurnState::Streaming,
            STALE_USER_SUBMITTED_RECLAIM_SECS + 1,
            true,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn user_submitted_old_with_marker_is_stale() {
        assert!(user_submitted_is_stale_stranded(
            TuiTurnState::UserSubmitted,
            STALE_USER_SUBMITTED_RECLAIM_SECS,
            true,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn user_submitted_recent_with_marker_is_live() {
        assert!(!user_submitted_is_stale_stranded(
            TuiTurnState::UserSubmitted,
            STALE_USER_SUBMITTED_RECLAIM_SECS - 1,
            true,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn user_submitted_old_without_marker_is_live() {
        assert!(!user_submitted_is_stale_stranded(
            TuiTurnState::UserSubmitted,
            STALE_USER_SUBMITTED_RECLAIM_SECS + 10_000,
            false,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn idle_and_unknown_are_not_reclaimed() {
        for state in [TuiTurnState::Idle, TuiTurnState::Unknown] {
            assert!(!user_submitted_is_stale_stranded(
                state,
                STALE_USER_SUBMITTED_RECLAIM_SECS + 1,
                true,
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_floor_aligns_with_dead_watcher_precedent() {
        assert_eq!(STALE_USER_SUBMITTED_RECLAIM_SECS, 600);
    }

    #[test]
    fn claude_result_marks_idle_even_when_pane_scrape_would_be_ambiguous() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hello"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s"}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    #[test]
    fn claude_user_without_terminal_envelope_is_user_submitted() {
        let file = write_jsonl(&[r#"{"type":"user","message":{"content":"hello"}}"#]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // #3981 (classifier invariant): a turn stopped immediately — before claude
    // could write a terminator (`result`) or the `[Request interrupted by user]`
    // marker — leaves an `assistant` envelope followed by a trailing *bare*
    // `type=user` envelope. Structurally this is indistinguishable from a fresh
    // submission, so the PURE classifier MUST still report `UserSubmitted`.
    // Distinguishing dead (stopped) from live (about to flush) needs runtime
    // evidence (activity mtime + live pane marker) that this pure function does
    // not have — that staleness decision is the busy gate's responsibility
    // (`user_submitted_is_stale_stranded`), NOT the classifier's.
    // This test pins the separation of responsibilities: the #3981 fix must not
    // be pushed down into the classifier.
    #[test]
    fn claude_trailing_bare_user_after_assistant_remains_user_submitted() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"do something"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"and another thing"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // #3221: a turn aborted via a session-preserving interrupt (ESC / wrapper
    // control_request{interrupt}, #3207) leaves a trailing
    // `[Request interrupted by user]` user envelope. That marks the turn ENDED,
    // not a fresh submission, so the JSONL-authoritative busy gate (#3208) must
    // see Idle — otherwise the stopped turn looks in-flight forever and the next
    // message is wrongly queued as `*_tui_busy_pre_submit`.
    #[test]
    fn claude_interrupt_marker_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"do something"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // The `… for tool use` interrupt variant must be recognized too.
    #[test]
    fn claude_interrupt_marker_for_tool_use_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"running tool"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // Realistic tail: claude appends a `file-history-snapshot` after the
    // interrupt marker. The observer walks past the snapshot (no turn-state
    // signal) and still reads the marker as Idle.
    #[test]
    fn claude_interrupt_marker_then_file_history_snapshot_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            r#"{"type":"file-history-snapshot","messageId":"x"}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // Suppression must be one-shot: once the user submits a NEW prompt after the
    // interrupt marker, the turn is genuinely busy again.
    #[test]
    fn claude_new_prompt_after_interrupt_marker_is_user_submitted() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"actually do this instead"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // Precision guard: a real user prompt that merely *mentions* the word
    // interrupted (or a tool_result echoing it) must NOT be mistaken for the
    // turn-end marker.
    #[test]
    fn claude_user_text_mentioning_interrupted_stays_user_submitted() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"why was the request interrupted by user earlier?"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // Precision guard (#3222 codex follow-up): a user-TYPED prompt that merely
    // *starts with* the marker prefix (`[Request interrupted by user story…]`)
    // must NOT match. Exact-equality matching keeps it `UserSubmitted` and not a
    // terminator, so a real in-flight turn is never dropped.
    #[test]
    fn claude_interrupt_marker_prefixed_prose_stays_user_submitted() {
        let prose: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user story]"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_envelope_turn_state(&prose),
            Some(TuiTurnState::UserSubmitted)
        );
        assert!(!envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &prose
        ));

        let file = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user story]"}]}}"#,
        ]);
        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // Precision guard (#3222 codex follow-up): a multi-block user message where
    // one block equals the marker but another carries real user content must NOT
    // match. Single-block-only matching keeps it `UserSubmitted` and not a
    // terminator, so the in-flight turn is preserved.
    #[test]
    fn claude_interrupt_marker_with_extra_block_stays_user_submitted() {
        let multi_block: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"},{"type":"text","text":"actually keep going"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_envelope_turn_state(&multi_block),
            Some(TuiTurnState::UserSubmitted)
        );
        assert!(!envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &multi_block
        ));

        let file = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"},{"type":"text","text":"actually keep going"}]}}"#,
        ]);
        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // The turn-END-only strict scan (finalize `Done` decision) must agree with
    // the standard observer that the interrupt marker is a genuine terminator.
    #[test]
    fn claude_interrupt_marker_is_turn_end_terminator() {
        let marker: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_envelope_turn_state(&marker),
            Some(TuiTurnState::Idle)
        );
        assert!(envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &marker
        ));

        let plain_user: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();
        assert!(!envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &plain_user
        ));
    }

    #[test]
    fn claude_init_without_user_envelope_is_idle() {
        let file = write_jsonl(&[r#"{"type":"system","subtype":"init","session_id":"s"}"#]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    #[test]
    fn claude_assistant_without_terminal_envelope_is_streaming() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Streaming
        );
    }

    // #2712 / #2716: a trailing `permission-mode` envelope from a freshly
    // spawned Claude session must NOT cause the classifier to fall back to
    // the previous turn's `result` and report Idle. Otherwise the watcher
    // tears down before the new turn's assistant output is written.
    #[test]
    fn claude_permission_mode_trailing_does_not_fall_back_to_previous_result() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"prev"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s-prev"}"#,
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s-new"}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Unknown
        );
    }

    // #2712 / #2716: once the new turn actually begins (a `user` envelope
    // follows the permission-mode marker) the classifier should reflect that
    // — the permission-mode line stays a no-op but the user envelope wins.
    #[test]
    fn claude_user_after_permission_mode_is_user_submitted() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s-prev"}"#,
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s-new"}"#,
            r#"{"type":"user","message":{"content":"new prompt"}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    // #2712 / #2716: once the new turn actually streams, the assistant
    // envelope wins over the earlier permission-mode marker.
    #[test]
    fn claude_assistant_after_permission_mode_is_streaming() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s-prev"}"#,
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s-new"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Streaming
        );
    }

    // Partial / unterminated JSON line for the same permission-mode envelope
    // (writer crashed mid-flush) is treated the same way — Unknown, never a
    // fallback to the previous result.
    #[test]
    fn claude_permission_mode_partial_line_classified_as_unknown() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s-prev"}"#,
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionI"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Unknown
        );
    }

    #[test]
    fn codex_task_complete_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"rollout","cwd":"/tmp/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"hi"}}"#,
        ]);

        assert_eq!(
            observe_codex_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    #[test]
    fn codex_response_item_user_marks_user_submitted() {
        let file = write_jsonl(&[
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
        ]);

        assert_eq!(
            observe_codex_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    #[test]
    fn missing_jsonl_is_idle_for_first_entry() {
        let path = std::env::temp_dir().join(format!(
            "agentdesk-missing-turn-state-{}.jsonl",
            uuid::Uuid::new_v4()
        ));

        assert_eq!(observe_claude_jsonl_turn_state(&path), TuiTurnState::Idle);
    }

    #[test]
    fn claude_malformed_latest_line_falls_back_to_previous_envelope() {
        let file = write_jsonl(&[r#"{"type":"result"}"#, "{not-json"]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    #[test]
    fn claude_partial_user_latest_line_marks_user_submitted() {
        let file = write_jsonl(&[
            r#"{"type":"result"}"#,
            r#"{"type":"user","message":{"content":"hello""#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    #[test]
    fn claude_partial_assistant_latest_line_marks_streaming() {
        let file = write_jsonl(&[
            r#"{"type":"result"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text""#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Streaming
        );
    }

    #[test]
    fn claude_partial_user_content_type_text_does_not_override_envelope_type() {
        let file = write_jsonl(&[
            r#"{"type":"result"}"#,
            r#"{"type":"user","message":{"content":"why does this say \"type\":\"assistant\"""#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::UserSubmitted
        );
    }

    #[test]
    fn claude_only_unclassified_malformed_lines_are_unknown() {
        let file = write_jsonl(&["{not-json"]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Unknown
        );
    }

    #[test]
    fn codex_malformed_latest_line_stays_unknown() {
        let file = write_jsonl(&[
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#,
            r#"{"type":"response_item","payload":{"type":"message""#,
        ]);

        assert_eq!(
            observe_codex_jsonl_turn_state(file.path()),
            TuiTurnState::Unknown
        );
    }

    #[test]
    fn codex_turn_completed_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#,
        ]);

        assert_eq!(
            observe_codex_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // The relay's consumed offset gates input readiness: when the relay has
    // not finished consuming the transcript, we cannot assume the next prompt
    // marker has been delivered. But a confirmed terminator envelope
    // (`system/turn_duration`, `result`, `turn.completed`) means the turn is
    // over regardless of the offset — the remaining bytes are just trailing
    // terminator metadata. Holding Busy in that window strands the idle-queue
    // drain (`hosted TUI structured turn state is busy` loop after #2789
    // preserved inflight across quick-exit restarts but left the relay's
    // last_offset frozen behind the trailing `result` envelope).
    #[test]
    fn structured_jsonl_ready_idle_terminator_overrides_offset_behind() {
        let file =
            write_jsonl(&[r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len.saturating_sub(1)),
            ),
            Some(TuiReadyState::Ready),
            "trailing turn_duration envelope must report Ready even when the \
             relay's consumed offset lags the transcript file size"
        );
        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len),
            ),
            Some(TuiReadyState::Ready)
        );
    }

    // Recurring "no active turn but queue stuck" bug: the user opens `/model`
    // (or `/compact`) in the remote TUI and returns to the prompt. Claude
    // writes post-turn housekeeping envelopes *after* the turn terminator —
    // `last-prompt`, `ai-title`, `mode`, `permission-mode`, `pr-link`. With
    // the relay's consumed offset behind the file, #2790's single-latest-line
    // strict check saw `pr-link` (a non-turn-state envelope), failed to prove
    // a terminator, and reported Busy forever — so the deferred idle-queue
    // drain looped to its 150-attempt ceiling and abandoned the queued
    // message. The strict probe must walk back across the housekeeping lines
    // to the real `turn_duration` terminator and report Ready.
    #[test]
    fn structured_jsonl_ready_terminator_with_trailing_housekeeping_is_ready() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"system","subtype":"stop_hook_summary","session_id":"s"}"#,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
            r#"{"type":"last-prompt","prompt":"hi"}"#,
            r#"{"type":"ai-title","title":"chat"}"#,
            r#"{"type":"mode","mode":"default"}"#,
            r#"{"type":"permission-mode","mode":"default"}"#,
            r#"{"type":"pr-link","url":"https://example.com/pr/1"}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                // Relay consumed only through the terminator; the trailing
                // `/model` housekeeping bytes are still unconsumed.
                Some(len / 2),
            ),
            Some(TuiReadyState::Ready),
            "a terminator followed by `/model` housekeeping envelopes \
             (permission-mode, pr-link, …) must still report Ready"
        );
    }

    // The walk-back must not cross a genuine in-flight signal: if a new turn's
    // `user`/`assistant` envelope sits after the last terminator (e.g. a
    // trailing housekeeping line was appended mid-turn), the session is busy.
    #[test]
    fn structured_jsonl_ready_inflight_user_after_terminator_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"type":"user","message":{"content":"next question"}}"#,
            r#"{"type":"attachment","path":"/tmp/x"}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "a new `user` envelope after the terminator must keep the session \
             Busy even when a housekeeping line trails it"
        );
    }

    // Defense-in-depth: when the trailing envelope is still streaming
    // assistant content (no terminator yet) and the relay has not consumed
    // the full file, the session is genuinely mid-turn — must report Busy
    // so we do not send the next input on top of an in-progress response.
    #[test]
    fn structured_jsonl_ready_streaming_with_offset_behind_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len.saturating_sub(1)),
            ),
            Some(TuiReadyState::Busy)
        );
    }

    // Quick-exit restart regression (#2789 follow-up): inflight binding's
    // `last_offset` is preserved at the pre-restart value while the Claude
    // TUI continues writing the turn's terminal `result` envelope. The new
    // bytes past `last_offset` must not make us miss the Idle classification.
    #[test]
    fn structured_jsonl_ready_result_envelope_after_quick_exit_offset_is_ready() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s"}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();
        // Simulate quick-exit restart that froze the binding offset before
        // the `result` envelope was written.
        let stale_offset = len / 2;

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(stale_offset),
            ),
            Some(TuiReadyState::Ready)
        );
    }

    // Race guard: a `result` envelope is complete on disk, but the *next*
    // turn has begun and a partial `{"ty` fragment of the new `user`
    // envelope was just appended. The standard tail walker falls back
    // through the partial to the prior `result` — that would let the
    // strict-classifier mistakenly mark this as Ready and dispatch a
    // racing input onto a session that already has a new user prompt
    // in-flight. The strict terminator predicate must refuse to fall
    // through and keep the state Busy until the new envelope completes.
    #[test]
    fn structured_jsonl_ready_partial_new_user_after_result_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"ty"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len.saturating_sub(5)),
            ),
            Some(TuiReadyState::Busy),
            "partial trailing fragment after a result envelope must not be \
             treated as a turn terminator"
        );
    }

    // Codex parity for the same race: partial fragment after `turn.completed`
    // must keep state Busy.
    #[test]
    fn structured_jsonl_ready_codex_partial_after_turn_completed_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"{"ty"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
                Some(len.saturating_sub(5)),
            ),
            Some(TuiReadyState::Busy)
        );
    }

    // Codex parity: `turn.completed` is Codex's terminator and must follow
    // the same override semantics.
    #[test]
    fn structured_jsonl_ready_codex_turn_completed_overrides_offset_behind() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
                Some(len.saturating_sub(1)),
            ),
            Some(TuiReadyState::Ready)
        );
    }

    #[test]
    fn legacy_wrapper_runtime_does_not_claim_structured_ready_state() {
        let file = write_jsonl(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::LegacyTmuxWrapper),
                file.path(),
                None,
            ),
            None
        );
    }

    #[test]
    fn pane_ready_fallback_is_disabled_for_structured_tui_jsonl() {
        assert!(!pane_ready_fallback_allowed(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui)
        ));
        assert!(!pane_ready_fallback_allowed(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui)
        ));
        assert!(!pane_ready_fallback_allowed(&ProviderKind::Claude, None));
        assert!(pane_ready_fallback_allowed(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::LegacyTmuxWrapper)
        ));
        assert!(pane_ready_fallback_allowed(&ProviderKind::Qwen, None));
    }

    #[test]
    fn codex_function_call_marks_streaming() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"run_cmd","arguments":"{}","call_id":"c1"}}"#,
        ]);

        assert_eq!(
            observe_codex_jsonl_turn_state(file.path()),
            TuiTurnState::Streaming
        );
    }

    // U-6 Policy clause 3: an assistant envelope whose content array carries
    // only a `thinking` block (no terminal `result` after it) keeps the turn
    // in `Streaming` — thinking must never on its own be treated as
    // turn-completion. If this regresses, the relay could close the inflight
    // panel mid-reasoning.
    #[test]
    fn claude_assistant_with_only_thinking_content_stays_streaming() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hello"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"reasoning"}]}}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Streaming
        );
    }

    // U-7 system/turn_duration and system/stop_hook_summary are metadata
    // envelopes that mark the end of a turn — they must classify as Idle
    // so cold-start probes do not mistake the trailing metadata for a
    // mid-stream assistant response.
    #[test]
    fn claude_system_turn_duration_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi back"}]}}"#,
            r#"{"type":"system","subtype":"turn_duration","duration_ms":1234}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    #[test]
    fn claude_system_stop_hook_summary_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"type":"system","subtype":"stop_hook_summary","detail":"ok"}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // U-7 An unknown `system` subtype must not be silently classified as
    // Idle — that would let novel metadata envelopes spuriously close
    // turns. The classifier walks back to the previous envelope instead.
    #[test]
    fn claude_unknown_system_subtype_falls_back_to_previous_envelope() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"system","subtype":"future_unknown_event","note":"x"}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Streaming
        );
    }

    #[test]
    fn codex_completed_agent_message_marks_idle() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#,
        ]);

        assert_eq!(
            observe_codex_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // ----------------------------------------------------------------------
    // #3030 — torn trailing write: a single truncated *housekeeping* trailing
    // line (writer mid-flush) must not pin the strict probe Busy forever. The
    // probe skips just that one trailing partial and reads the prior complete
    // terminator → Ready.
    // ----------------------------------------------------------------------
    #[test]
    fn structured_jsonl_ready_torn_trailing_housekeeping_partial_is_ready() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            // Torn trailing housekeeping write (no closing brace).
            r#"{"type":"permission-mode","mode":"def"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Ready),
            "a torn trailing housekeeping partial must be skipped to reveal the \
             prior result terminator"
        );
    }

    // #3030 false-idle guard: a torn trailing fragment that is too short to
    // identify (could be the start of a new `user`/`assistant` envelope) must
    // NOT be skipped — stay Busy. This is the same race the #2790 guard
    // protected, re-verified under the torn-write skip logic.
    #[test]
    fn structured_jsonl_ready_torn_trailing_unidentifiable_partial_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"ty"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len.saturating_sub(5)),
            ),
            Some(TuiReadyState::Busy),
            "an unidentifiable torn trailing fragment could be a new turn — \
             must stay Busy"
        );
    }

    // #3030 false-idle guard: a torn trailing fragment that *does* identify as
    // an active envelope (a partial new `user`) must keep the session Busy even
    // though its line is truncated — never skip an active-looking partial.
    #[test]
    fn structured_jsonl_ready_torn_trailing_active_user_partial_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"type":"user","message":{"content":"new prompt"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "a torn trailing partial that identifies as a new user envelope \
             must stay Busy"
        );
    }

    // #3030 false-idle guard: only ONE trailing partial may be skipped. A torn
    // housekeeping partial followed (above) by a *second* unparseable interior
    // line must stay Busy — we never skip multiple/interior partials.
    #[test]
    fn structured_jsonl_ready_torn_trailing_then_interior_partial_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{interior-corruption"#,
            r#"{"type":"permission-mode","mode":"def"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "a second (interior) unparseable line after the torn trailing skip \
             must keep the session Busy"
        );
    }

    // #3030 false-idle guard: a torn trailing partial sitting above a still
    // *streaming* assistant envelope must stay Busy — skipping the housekeeping
    // partial must reveal the streaming signal, not an idle one.
    #[test]
    fn structured_jsonl_ready_torn_trailing_above_streaming_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"mode","mode":"def"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "skipping a torn housekeeping partial must reveal the underlying \
             streaming assistant → Busy"
        );
    }

    // ----------------------------------------------------------------------
    // #3030 — unknown-envelope-hiding-terminator: a renamed `/model`-view mode
    // envelope (structurally recognized as housekeeping) must be skipped to
    // reveal the real terminator beneath it. The structural match maps it to
    // Unknown, NOT Idle — so it can only *uncover* a terminator, never create
    // one.
    // ----------------------------------------------------------------------
    #[test]
    fn structured_jsonl_ready_renamed_mode_housekeeping_uncovers_terminator() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            // Hypothetical future rename of the interactive-view housekeeping
            // envelope; structurally still a mode marker.
            r#"{"type":"model-mode","model":"opus"}"#,
            r#"{"type":"permission_mode","permissionMode":"default"}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Ready),
            "renamed mode/permission housekeeping envelopes must be skipped to \
             uncover the real terminator"
        );
    }

    // #3030 false-idle guard: a renamed mode housekeeping envelope must NOT
    // itself be treated as idle when there is no terminator beneath it — the
    // session stays Busy (Unknown, not Idle).
    #[test]
    fn structured_jsonl_ready_renamed_mode_without_terminator_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"model-mode","model":"opus"}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "a mode housekeeping envelope is Unknown, never an idle terminator"
        );
    }

    // #3030 false-idle guard: a genuinely unknown (non-mode) envelope after a
    // streaming assistant must NOT count as idle — it is skipped, the streaming
    // signal beneath wins → Busy. Confirms unknowns are never upgraded to idle.
    #[test]
    fn structured_jsonl_ready_unknown_envelope_above_streaming_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"some-brand-new-envelope","data":1}"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "a genuinely-unknown envelope must never be classified idle"
        );
    }

    // #3030 / reviewer P2: the broader mode-family hardening is intentionally
    // NOT wired into the standard observer's classifier. A completed turn that
    // trails a renamed `/model` housekeeping envelope (`model-mode`) must walk
    // back across it to the real `result` terminator and report Idle — NOT stop
    // at the housekeeping line and report not-ready. (Mapping the whole family
    // to Unknown here would regress completion/readiness paths that pass no
    // consumed offset and rely on this walk-back.) `permission-mode` keeps its
    // narrow Unknown special case for the fresh-restart race (#2712); see the
    // dedicated test above.
    #[test]
    fn claude_trailing_renamed_mode_walks_back_to_terminator() {
        let file = write_jsonl(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"type":"model-mode","model":"opus"}"#,
        ]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Idle
        );
    }

    // ----------------------------------------------------------------------
    // #3030 — tail window: a terminator older than the default 64KB window
    // (pushed out by a large post-terminator housekeeping burst) must still be
    // found by the bounded widened re-scan → Ready, instead of being stuck Busy.
    // ----------------------------------------------------------------------
    #[test]
    fn structured_jsonl_ready_terminator_beyond_default_window_is_ready() {
        let mut lines: Vec<String> = vec![
            r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
                .to_string(),
            r#"{"type":"result","result":"done","session_id":"s"}"#.to_string(),
        ];
        // Append > 64KB of post-terminator housekeeping to push the terminator
        // out of the default tail window.
        let filler = r#"{"type":"pr-link","url":"https://example.com/pr/very-long-path-padding-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        let mut bytes = 0usize;
        while bytes < (TURN_STATE_TAIL_BYTES as usize) + 4096 {
            lines.push(filler.to_string());
            bytes += filler.len() + 1;
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), lines.join("\n")).unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();
        assert!(
            len > TURN_STATE_TAIL_BYTES,
            "fixture must exceed 64KB window"
        );

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Ready),
            "a terminator pushed out of the 64KB window by a housekeeping burst \
             must still be found by the bounded widened re-scan"
        );
    }

    // #3030 false-idle guard for the tail window: if neither the default nor
    // the widened window contains a terminator (only streaming/housekeeping)
    // the session must stay Busy — widening must never manufacture an idle.
    #[test]
    fn structured_jsonl_ready_no_terminator_in_large_file_stays_busy() {
        let mut lines: Vec<String> = vec![
            r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#
                .to_string(),
        ];
        let filler = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"streaming chunk padding aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}"#;
        let mut bytes = 0usize;
        while bytes < (TURN_STATE_TAIL_BYTES as usize) + 4096 {
            lines.push(filler.to_string());
            bytes += filler.len() + 1;
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), lines.join("\n")).unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "no terminator anywhere in a large streaming file must stay Busy"
        );
    }

    // #3030 Codex parity: the torn-trailing skip must NOT apply to Codex (it has
    // no safe-to-skip post-turn housekeeping family) — a torn trailing Codex
    // line stays Busy.
    #[test]
    fn structured_jsonl_ready_codex_torn_trailing_partial_stays_busy() {
        let file = write_jsonl(&[
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_co"#,
        ]);
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            jsonl_ready_for_input(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
                Some(len / 2),
            ),
            Some(TuiReadyState::Busy),
            "Codex has no skippable housekeeping family — torn trailing line \
             stays Busy"
        );
    }

    // #3030 unit coverage for the structural mode-type matcher: it must catch
    // the mode/permission family and never catch a real turn-state type.
    #[test]
    fn interactive_mode_housekeeping_type_matches_only_mode_family() {
        assert!(is_interactive_mode_housekeeping_type("mode"));
        assert!(is_interactive_mode_housekeeping_type("permission-mode"));
        assert!(is_interactive_mode_housekeeping_type("permission_mode"));
        assert!(is_interactive_mode_housekeeping_type("model-mode"));
        assert!(is_interactive_mode_housekeeping_type("compact-mode"));
        assert!(!is_interactive_mode_housekeeping_type("result"));
        assert!(!is_interactive_mode_housekeeping_type("assistant"));
        assert!(!is_interactive_mode_housekeeping_type("user"));
        assert!(!is_interactive_mode_housekeeping_type("system"));
        assert!(!is_interactive_mode_housekeeping_type("turn.completed"));
    }

    #[test]
    fn torn_trailing_fragment_detects_missing_close_brace() {
        assert!(is_torn_trailing_fragment(r#"{"type":"mode","mode":"def"#));
        assert!(!is_torn_trailing_fragment(r#"{"type":"result"}"#));
        assert!(!is_torn_trailing_fragment("not-json-at-all"));
    }

    // =======================================================================
    // #3016 S3 (Concern 1): the STRICTER turn-END-only terminator probe
    // (`jsonl_turn_end_terminator_idle`) vs. the lenient drain probe
    // (`jsonl_strict_terminator_idle`). The finalize `Done` decision must read
    // the turn-END probe so a non-terminator Idle-class envelope (a completed
    // Codex `agent_message` mid-turn, a Claude `system{init}`) cannot
    // over-finalize a LIVE turn.
    // =======================================================================

    // Codex: a completed `agent_message` written right before a tool call is
    // MID-TURN. The lenient drain probe reports Idle (it is ready for input by
    // its definition), but the turn-END probe must NOT — the turn has not ended.
    #[test]
    fn codex_completed_agent_message_is_not_turn_end_but_is_lenient_idle() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"on it"}}"#,
        ]);
        assert!(
            jsonl_strict_terminator_idle(&ProviderKind::Codex, file.path()),
            "lenient drain probe treats a completed agent_message as at-rest"
        );
        assert!(
            !jsonl_turn_end_terminator_idle(&ProviderKind::Codex, file.path()),
            "turn-END probe must NOT treat a completed agent_message as a turn boundary"
        );
    }

    // Codex: the AUTHORITATIVE turn terminator `turn.completed` → BOTH probes
    // Idle. This is the only Codex envelope the turn-END probe accepts.
    #[test]
    fn codex_turn_completed_is_turn_end() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#,
        ]);
        assert!(jsonl_strict_terminator_idle(
            &ProviderKind::Codex,
            file.path()
        ));
        assert!(
            jsonl_turn_end_terminator_idle(&ProviderKind::Codex, file.path()),
            "turn.completed is the authoritative Codex turn-end terminator"
        );
    }

    // Codex: `event_msg{task_complete}` and `session_meta`/`thread.started` are
    // Idle-class to the lenient probe but are NOT turn-end terminators.
    #[test]
    fn codex_task_complete_and_session_markers_are_not_turn_end() {
        let task_complete = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        assert!(jsonl_strict_terminator_idle(
            &ProviderKind::Codex,
            task_complete.path()
        ));
        assert!(
            !jsonl_turn_end_terminator_idle(&ProviderKind::Codex, task_complete.path()),
            "event_msg{{task_complete}} is not the turn record terminator"
        );

        let session_only = write_jsonl(&[r#"{"type":"thread.started","thread_id":"t"}"#]);
        assert!(jsonl_strict_terminator_idle(
            &ProviderKind::Codex,
            session_only.path()
        ));
        assert!(
            !jsonl_turn_end_terminator_idle(&ProviderKind::Codex, session_only.path()),
            "thread.started is session bring-up, not a turn end"
        );
    }

    // Codex: the turn-END probe still walks BACK across trailing non-terminator
    // Idle-class markers to a real `turn.completed` beneath them (housekeeping
    // walk-back preserved). A completed agent_message AFTER the terminator that
    // does NOT belong to a new turn boundary is skipped; the terminator wins.
    #[test]
    fn codex_turn_end_walks_back_across_trailing_session_meta() {
        let file = write_jsonl(&[
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"{"type":"session_meta","payload":{"id":"s2","cwd":"/repo"}}"#,
        ]);
        assert!(
            jsonl_turn_end_terminator_idle(&ProviderKind::Codex, file.path()),
            "a trailing session_meta is walked past to the real turn.completed beneath"
        );
    }

    // Claude: a mid-turn assistant message → neither probe Idle (streaming is
    // Busy). And `system{init}` is a SESSION-start marker the lenient probe
    // trusts as idle, but the turn-END probe must NOT.
    #[test]
    fn claude_mid_turn_assistant_is_not_turn_end() {
        let streaming = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        ]);
        assert!(!jsonl_strict_terminator_idle(
            &ProviderKind::Claude,
            streaming.path()
        ));
        assert!(
            !jsonl_turn_end_terminator_idle(&ProviderKind::Claude, streaming.path()),
            "a mid-turn assistant message is not a turn end"
        );

        let init_only = write_jsonl(&[r#"{"type":"system","subtype":"init","session_id":"s"}"#]);
        assert!(
            jsonl_strict_terminator_idle(&ProviderKind::Claude, init_only.path()),
            "lenient probe treats system{{init}} as at-rest (ready for input)"
        );
        assert!(
            !jsonl_turn_end_terminator_idle(&ProviderKind::Claude, init_only.path()),
            "system{{init}} is a session-start marker, NOT a turn-end terminator"
        );
    }

    // Claude: `result` and `system{turn_duration|stop_hook_summary}` ARE the real
    // turn terminators → turn-END probe Idle.
    #[test]
    fn claude_real_terminators_are_turn_end() {
        for terminator in [
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
            r#"{"type":"system","subtype":"stop_hook_summary","session_id":"s"}"#,
        ] {
            let file = write_jsonl(&[
                r#"{"type":"user","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
                terminator,
            ]);
            assert!(
                jsonl_turn_end_terminator_idle(&ProviderKind::Claude, file.path()),
                "{terminator} is a real Claude turn-end terminator"
            );
        }
    }

    // Claude: the turn-END probe still walks BACK across trailing post-turn
    // housekeeping (`mode`, `permission-mode`, `pr-link`) to the real terminator
    // beneath (the #3030 walk-back is preserved under the stricter mode).
    #[test]
    fn claude_turn_end_walks_back_across_trailing_housekeeping() {
        let file = write_jsonl(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"type":"mode","mode":"default"}"#,
            r#"{"type":"permission-mode","mode":"default"}"#,
            r#"{"type":"pr-link","url":"https://example.com/pr/1"}"#,
        ]);
        assert!(
            jsonl_turn_end_terminator_idle(&ProviderKind::Claude, file.path()),
            "trailing housekeeping is walked past to the real result terminator"
        );
    }

    // The turn-END terminator classifier directly: only the narrow per-provider
    // subset returns true.
    #[test]
    fn envelope_is_turn_end_terminator_narrow_subset() {
        let p = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        // Codex: only turn.completed.
        assert!(envelope_is_turn_end_terminator(
            &ProviderKind::Codex,
            &p(r#"{"type":"turn.completed"}"#)
        ));
        assert!(!envelope_is_turn_end_terminator(
            &ProviderKind::Codex,
            &p(r#"{"type":"session_meta"}"#)
        ));
        assert!(!envelope_is_turn_end_terminator(
            &ProviderKind::Codex,
            &p(r#"{"type":"item.completed","item":{"type":"agent_message"}}"#)
        ));
        // Claude: result + turn_duration/stop_hook_summary, NOT init.
        assert!(envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &p(r#"{"type":"result"}"#)
        ));
        assert!(envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &p(r#"{"type":"system","subtype":"turn_duration"}"#)
        ));
        assert!(!envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &p(r#"{"type":"system","subtype":"init"}"#)
        ));
    }
}
