//! Stateful StreamJson codecs.

use std::sync::mpsc;

use serde_json::Value;

use crate::services::agent_protocol::StreamMessage;
use crate::services::session_backend::{StreamLineState, process_stream_line};

pub trait StreamJsonCodec: Send {
    fn push_stdout_line(&mut self, line: &str) -> Result<Vec<StreamMessage>, String>;
    fn finish(
        &mut self,
        exit_code: Option<i32>,
        stderr: &str,
    ) -> Result<Vec<StreamMessage>, String>;
}

/// Grok `streaming-messages-json` uses the shared Messages accumulator.
pub struct MessagesJsonCodec {
    state: StreamLineState,
    session_id: Option<String>,
    emitted_done: bool,
}

impl MessagesJsonCodec {
    pub fn new() -> Self {
        Self {
            state: StreamLineState::new(),
            session_id: None,
            emitted_done: false,
        }
    }
}

impl Default for MessagesJsonCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamJsonCodec for MessagesJsonCodec {
    fn push_stdout_line(&mut self, line: &str) -> Result<Vec<StreamMessage>, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let json: Value = serde_json::from_str(trimmed)
            .map_err(|error| format!("malformed StreamJson line: {error}"))?;
        if let Some(session_id) = json
            .get("session_id")
            .or_else(|| json.get("sessionId"))
            .and_then(Value::as_str)
        {
            self.session_id = Some(session_id.to_string());
        }
        let (tx, rx) = mpsc::channel();
        let keep_going = process_stream_line(trimmed, &tx, &mut self.state);
        drop(tx);
        let emitted: Vec<StreamMessage> = rx.try_iter().collect();
        if emitted
            .iter()
            .any(|message| matches!(message, StreamMessage::Done { .. }))
        {
            self.emitted_done = true;
        }
        if !keep_going && emitted.is_empty() {
            return Err("StreamJson codec stopped without messages".into());
        }
        Ok(emitted)
    }

    fn finish(
        &mut self,
        exit_code: Option<i32>,
        stderr: &str,
    ) -> Result<Vec<StreamMessage>, String> {
        if self.emitted_done {
            return Ok(Vec::new());
        }
        if let Some((message, stdout)) = self.state.stdout_error.take() {
            return Ok(vec![StreamMessage::Error {
                message,
                stdout,
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        if exit_code.unwrap_or(0) != 0 {
            return Ok(vec![StreamMessage::Error {
                message: if stderr.trim().is_empty() {
                    format!("provider exited with status {exit_code:?}")
                } else {
                    stderr.trim().to_string()
                },
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        if self.session_id.is_none() {
            return Err("terminal success without a valid session id".into());
        }
        Err("StreamJson stream ended without a terminal result".into())
    }
}

/// AGY `event=init|step_update|result` codec.
pub struct AgyCodec {
    session_id: Option<String>,
    saw_text_delta: bool,
    emitted_text: String,
    usage_steps: std::collections::BTreeSet<i64>,
    last_step_error: Option<String>,
    terminal: Option<Result<(), String>>,
    finished: bool,
}

impl AgyCodec {
    pub fn new() -> Self {
        Self {
            session_id: None,
            saw_text_delta: false,
            emitted_text: String::new(),
            usage_steps: std::collections::BTreeSet::new(),
            last_step_error: None,
            terminal: None,
            finished: false,
        }
    }
}

impl Default for AgyCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamJsonCodec for AgyCodec {
    fn push_stdout_line(&mut self, line: &str) -> Result<Vec<StreamMessage>, String> {
        if self.finished || self.terminal.is_some() {
            return Ok(Vec::new());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let json: Value = serde_json::from_str(trimmed)
            .map_err(|error| format!("malformed AGY StreamJson line: {error}"))?;
        let event = json
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event {
            "init" => {
                let id = json
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "AGY init missing conversation_id".to_string())?;
                self.session_id = Some(id.to_string());
                Ok(vec![StreamMessage::Init {
                    session_id: id.to_string(),
                    raw_session_id: Some(id.to_string()),
                }])
            }
            "step_update" => {
                let step_type = json.get("step_type").and_then(Value::as_str).unwrap_or("");
                self.remember_step_error(&json, step_type);
                let mut out = Vec::new();
                if step_type == "agent_response" {
                    if let Some(delta) = json.get("text_delta").and_then(Value::as_str) {
                        if !delta.is_empty() {
                            self.saw_text_delta = true;
                            self.emitted_text.push_str(delta);
                            out.push(StreamMessage::Text {
                                content: delta.to_string(),
                            });
                        }
                    }
                }
                if let Some(step_index) = json.get("step_index").and_then(Value::as_i64) {
                    let status = json.get("status").and_then(Value::as_str).unwrap_or("");
                    if status.eq_ignore_ascii_case("DONE")
                        || status.eq_ignore_ascii_case("terminal")
                    {
                        self.usage_steps.insert(step_index);
                    }
                }
                Ok(out)
            }
            "result" => {
                let id = json
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| self.session_id.clone());
                if let (Some(expected), Some(actual)) = (self.session_id.as_deref(), id.as_deref())
                {
                    if expected != actual {
                        return Err(format!(
                            "AGY conversation id mismatch: requested {expected}, got {actual}"
                        ));
                    }
                }
                self.session_id = id.clone();
                let status = json.get("status").and_then(Value::as_str).unwrap_or("");
                if !status.eq_ignore_ascii_case("SUCCESS") {
                    self.terminal = Some(Err(json_error_detail(&json)
                        .or_else(|| self.last_step_error.clone())
                        .unwrap_or_else(|| {
                            if status.is_empty() {
                                "AGY result missing status".to_string()
                            } else {
                                status.to_string()
                            }
                        })));
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                if !self.saw_text_delta {
                    if let Some(response) = json.get("response").and_then(Value::as_str) {
                        if !response.trim().is_empty() {
                            self.emitted_text = response.to_string();
                            out.push(StreamMessage::Text {
                                content: response.to_string(),
                            });
                        }
                    }
                }
                // Finalization needs both the terminal record and process exit.
                // In particular, SUCCESS may accompany a permission denial on
                // stderr, or precede a failing exit after partial output.
                self.terminal = Some(Ok(()));
                Ok(out)
            }
            "" => Err("AGY line missing event field".into()),
            _ => Ok(vec![StreamMessage::StatusUpdate {
                model: Some(event.to_string()),
                cost_usd: None,
                total_cost_usd: None,
                duration_ms: None,
                num_turns: None,
                input_tokens: None,
                cache_create_tokens: None,
                cache_read_tokens: None,
                output_tokens: None,
            }]),
        }
    }

    fn finish(
        &mut self,
        exit_code: Option<i32>,
        stderr: &str,
    ) -> Result<Vec<StreamMessage>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let terminal = self.terminal.take();
        if exit_code != Some(0) {
            let detail = terminal
                .and_then(Result::err)
                .or_else(|| self.last_step_error.take());
            return Ok(vec![StreamMessage::Error {
                message: detail.unwrap_or_else(|| {
                    if stderr.trim().is_empty() {
                        format!("agy exited without success (status {exit_code:?})")
                    } else {
                        stderr.trim().to_string()
                    }
                }),
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        if let Some(Err(message)) = terminal {
            return Ok(vec![StreamMessage::Error {
                message,
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        if terminal.is_none() {
            return Ok(vec![StreamMessage::Error {
                message: self
                    .last_step_error
                    .take()
                    .unwrap_or_else(|| "AGY stream ended without a terminal result".to_string()),
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        if self.emitted_text.trim().is_empty() {
            return Ok(vec![StreamMessage::Error {
                message: empty_success_message(self.last_step_error.as_deref(), stderr),
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        let session_id = self
            .session_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| "terminal success without a valid conversation id".to_string())?;
        Ok(vec![StreamMessage::Done {
            result: self.emitted_text.clone(),
            session_id: Some(session_id),
        }])
    }
}

impl AgyCodec {
    fn remember_step_error(&mut self, json: &Value, step_type: &str) {
        let state = json
            .get("status")
            .or_else(|| json.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_failure = matches!(
            state.to_ascii_lowercase().as_str(),
            "error" | "failed" | "failure" | "denied"
        );
        let detail = ["error", "error_message", "reason"]
            .into_iter()
            .find_map(|key| {
                json.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
            })
            .or_else(|| {
                is_failure
                    .then(|| json.get("message").and_then(Value::as_str))
                    .flatten()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
            })
            .or_else(|| {
                is_failure.then(|| format!("AGY step {step_type} ended with status {state}"))
            });
        if let Some(detail) = detail {
            self.last_step_error = Some(detail);
        }
    }
}

fn json_error_detail(json: &Value) -> Option<String> {
    ["error", "error_message", "reason", "message"]
        .into_iter()
        .find_map(|key| {
            json.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
}

fn empty_success_message(step_detail: Option<&str>, stderr: &str) -> String {
    let mut message = if step_detail.is_some_and(is_permission_denial)
        || is_permission_denial(stderr)
    {
        "AGY returned an empty response because a tool permission was denied in headless mode. Configure a narrowly scoped permissions.allow rule for the AGY project; do not use --dangerously-skip-permissions.".to_string()
    } else {
        "AGY returned SUCCESS without any response text; no usable assistant response was produced."
            .to_string()
    };
    if let Some(detail) = step_detail {
        message.push_str("\nProvider detail: ");
        message.push_str(detail);
    }
    message
}

fn is_permission_denial(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("permission")
        && (lower.contains("denied")
            || lower.contains("headless")
            || lower.contains("approval")
            || lower.contains("request-review"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_messages_codec_emits_init_and_done() {
        let mut codec = MessagesJsonCodec::new();
        let init = codec
            .push_stdout_line(
                r#"{"type":"system","subtype":"init","session_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        assert!(matches!(init.first(), Some(StreamMessage::Init { .. })));
        let _ = codec
            .push_stdout_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            )
            .unwrap();
        let done = codec
            .push_stdout_line(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","session_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        assert!(
            done.iter()
                .any(|message| matches!(message, StreamMessage::Done { .. }))
        );
    }

    #[test]
    fn grok_messages_codec_rejects_clean_eof_after_init() {
        let mut codec = MessagesJsonCodec::new();
        let init = codec
            .push_stdout_line(
                r#"{"type":"system","subtype":"init","session_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        assert!(matches!(init.first(), Some(StreamMessage::Init { .. })));
        let error = codec.finish(Some(0), "").unwrap_err();
        assert_eq!(error, "StreamJson stream ended without a terminal result");
    }

    #[test]
    fn grok_messages_codec_preserves_terminal_error() {
        let mut codec = MessagesJsonCodec::new();
        let _ = codec
            .push_stdout_line(
                r#"{"type":"system","subtype":"init","session_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        let emitted = codec
            .push_stdout_line(
                r#"{"type":"result","subtype":"error","is_error":true,"result":"provider rejected the turn","session_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        assert!(emitted.is_empty());
        let error = codec.finish(Some(0), "").unwrap();
        assert!(matches!(
            error.as_slice(),
            [StreamMessage::Error { message, .. }] if message == "provider rejected the turn"
        ));
    }

    #[test]
    fn agy_codec_does_not_duplicate_aggregate_after_delta() {
        let mut codec = AgyCodec::new();
        let _ = codec
            .push_stdout_line(
                r#"{"event":"init","conversation_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        let _ = codec
            .push_stdout_line(
                r#"{"event":"step_update","step_type":"agent_response","text_delta":"hello","step_index":1,"status":"DONE"}"#,
            )
            .unwrap();
        let result = codec
            .push_stdout_line(
                r#"{"event":"result","status":"SUCCESS","conversation_id":"01234567-89ab-cdef-0123-456789abcdef","response":"hello","num_turns":2}"#,
            )
            .unwrap();
        let texts: Vec<_> = result
            .iter()
            .filter_map(|message| match message {
                StreamMessage::Text { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.is_empty(), "aggregate must not re-emit after deltas");
        assert!(result.is_empty(), "terminal success waits for process exit");
        assert!(matches!(codec.finish(Some(0), "").unwrap().as_slice(),
            [StreamMessage::Done { result, .. }] if result == "hello"));
    }

    #[test]
    fn agy_codec_turns_empty_success_into_permission_error_with_stderr() {
        let mut codec = AgyCodec::new();
        let _ = codec
            .push_stdout_line(
                r#"{"event":"init","conversation_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        let result = codec
            .push_stdout_line(
                r#"{"event":"result","status":"SUCCESS","conversation_id":"01234567-89ab-cdef-0123-456789abcdef","response":""}"#,
            )
            .unwrap();
        assert!(result.is_empty(), "empty success must wait for stderr");

        let messages = codec
            .finish(
                Some(0),
                "a tool required the command permission that headless mode cannot prompt for; it was auto-denied",
            )
            .unwrap();
        let Some(StreamMessage::Error {
            message, stderr, ..
        }) = messages.first()
        else {
            panic!("expected an explicit AGY error, got {messages:?}");
        };
        assert!(message.contains("permission was denied"));
        assert!(message.contains("permissions.allow"));
        assert!(stderr.contains("headless mode"));
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, StreamMessage::Done { .. }))
        );
    }

    #[test]
    fn agy_codec_preserves_step_failure_when_stderr_is_empty() {
        let mut codec = AgyCodec::new();
        let _ = codec
            .push_stdout_line(
                r#"{"event":"init","conversation_id":"01234567-89ab-cdef-0123-456789abcdef"}"#,
            )
            .unwrap();
        let _ = codec
            .push_stdout_line(
                r#"{"event":"step_update","step_type":"run_command","state":"ERROR","message":"command permission denied"}"#,
            )
            .unwrap();
        let _ = codec
            .push_stdout_line(
                r#"{"event":"result","status":"SUCCESS","conversation_id":"01234567-89ab-cdef-0123-456789abcdef","response":""}"#,
            )
            .unwrap();

        let messages = codec.finish(Some(0), "").unwrap();
        let Some(StreamMessage::Error { message, .. }) = messages.first() else {
            panic!("expected an explicit AGY error, got {messages:?}");
        };
        assert!(message.contains("permission was denied"));
        assert!(message.contains("command permission denied"));
    }

    #[test]
    fn agy_codec_rejects_blank_success_with_or_without_session_identity() {
        for response in ["", " \n\t"] {
            for id in [None, Some("01234567-89ab-cdef-0123-456789abcdef")] {
                let mut codec = AgyCodec::new();
                let event = serde_json::json!({
                    "event": "result", "status": "SUCCESS",
                    "conversation_id": id, "response": response
                });
                assert!(
                    codec
                        .push_stdout_line(&event.to_string())
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    matches!(codec.finish(Some(0), "command permission denied").unwrap().as_slice(),
                    [StreamMessage::Error { message, .. }] if message.contains("permission was denied"))
                );
                assert!(codec.finish(Some(0), "").unwrap().is_empty());
            }
        }
    }

    #[test]
    fn agy_codec_keeps_step_failure_alongside_unrelated_stderr() {
        let mut codec = AgyCodec::new();
        codec.push_stdout_line(r#"{"event":"step_update","step_type":"run_command","status":"DENIED","reason":"command permission denied"}"#).unwrap();
        codec
            .push_stdout_line(r#"{"event":"result","status":"SUCCESS","response":""}"#)
            .unwrap();
        assert!(
            matches!(codec.finish(Some(0), "update available").unwrap().as_slice(),
            [StreamMessage::Error { message, stderr, .. }]
                if message.contains("command permission denied") && stderr == "update available")
        );
    }

    #[test]
    fn agy_codec_failure_wins_over_partial_text_and_process_status() {
        for exit_code in [Some(0), Some(1), None] {
            let mut codec = AgyCodec::new();
            codec.push_stdout_line(r#"{"event":"step_update","step_type":"agent_response","text_delta":"working"}"#).unwrap();
            assert!(codec.push_stdout_line(r#"{"event":"result","status":"FAILED","error":"provider rejected command"}"#).unwrap().is_empty());
            assert!(
                matches!(codec.finish(exit_code, "diagnostic").unwrap().as_slice(),
                [StreamMessage::Error { message, stderr, exit_code: actual, .. }]
                    if message == "provider rejected command" && stderr == "diagnostic" && *actual == exit_code)
            );
        }
    }

    #[test]
    fn agy_codec_success_requires_successful_exit_and_emits_done_once() {
        for exit_code in [Some(0), Some(1), None] {
            let mut codec = AgyCodec::new();
            let event = r#"{"event":"result","status":"SUCCESS","response":"hello","conversation_id":"01234567-89ab-cdef-0123-456789abcdef"}"#;
            assert!(matches!(codec.push_stdout_line(event).unwrap().as_slice(),
                [StreamMessage::Text { content }] if content == "hello"));
            assert!(codec.push_stdout_line(event).unwrap().is_empty());
            let result = codec.finish(exit_code, "").unwrap();
            if exit_code == Some(0) {
                assert!(
                    matches!(result.as_slice(), [StreamMessage::Done { result, .. }] if result == "hello")
                );
            } else {
                assert!(matches!(result.as_slice(), [StreamMessage::Error { .. }]));
            }
            assert!(codec.finish(exit_code, "").unwrap().is_empty());
        }
    }

    #[test]
    fn agy_codec_preserves_diagnostics_without_terminal_result() {
        let mut codec = AgyCodec::new();
        codec.push_stdout_line(r#"{"event":"step_update","step_type":"run_command","status":"FAILED","error":"tool unavailable"}"#).unwrap();
        assert!(
            matches!(codec.finish(Some(0), "provider diagnostic").unwrap().as_slice(),
            [StreamMessage::Error { message, stderr, .. }]
                if message == "tool unavailable" && stderr == "provider diagnostic")
        );
    }
}
