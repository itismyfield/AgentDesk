use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::services::provider::ProviderKind;

const TURN_STATE_TAIL_BYTES: u64 = 64 * 1024;

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
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TURN_STATE_TAIL_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut lines = buf.lines().map(ToString::to_string).collect::<Vec<_>>();
    if start > 0 && !buf.starts_with('\n') && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

fn claude_envelope_turn_state(json: &Value) -> Option<TuiTurnState> {
    match json.get("type").and_then(Value::as_str)? {
        "result" => Some(TuiTurnState::Idle),
        "assistant" => Some(TuiTurnState::Streaming),
        "user" => Some(TuiTurnState::UserSubmitted),
        // `permission-mode` envelopes (e.g. `bypassPermissions` adoption after
        // a fresh session start triggered by hard_reset or `/compact`) are not
        // turn-state signals. If we returned `None` here, the tail walker
        // would skip them and fall back to the previous turn's `result`
        // envelope — declaring the new turn already idle and tearing the
        // watcher down before the first assistant line gets written
        // (#2712, #2716). Map them to `Unknown` so the gate keeps waiting
        // for a real turn-state envelope.
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
}
