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
    observe_jsonl_turn_state(path, claude_envelope_turn_state)
}

pub(crate) fn observe_codex_jsonl_turn_state(path: &Path) -> TuiTurnState {
    observe_jsonl_turn_state(path, codex_envelope_turn_state)
}

fn observe_jsonl_turn_state(
    path: &Path,
    classify: fn(&Value) -> Option<TuiTurnState>,
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
        let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
            return TuiTurnState::Unknown;
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
        "system" => match json.get("subtype").and_then(Value::as_str) {
            Some("turn_duration" | "stop_hook_summary" | "init") => Some(TuiTurnState::Idle),
            _ => None,
        },
        _ => None,
    }
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
        Some("agent_message") if completed => TuiTurnState::Streaming,
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
    fn malformed_latest_line_is_unknown() {
        let file = write_jsonl(&[r#"{"type":"result"}"#, "{not-json"]);

        assert_eq!(
            observe_claude_jsonl_turn_state(file.path()),
            TuiTurnState::Unknown
        );
    }
}
