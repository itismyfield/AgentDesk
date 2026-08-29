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
        let session_id = self.session_id.clone();
        if session_id.is_none() {
            return Err("terminal success without a valid session id".into());
        }
        Ok(vec![StreamMessage::Done {
            result: String::new(),
            session_id,
        }])
    }
}

/// AGY `event=init|step_update|result` codec.
pub struct AgyCodec {
    session_id: Option<String>,
    saw_text_delta: bool,
    emitted_text: String,
    usage_steps: std::collections::BTreeSet<i64>,
    finished: bool,
}

impl AgyCodec {
    pub fn new() -> Self {
        Self {
            session_id: None,
            saw_text_delta: false,
            emitted_text: String::new(),
            usage_steps: std::collections::BTreeSet::new(),
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
                if !status.is_empty() && !status.eq_ignore_ascii_case("SUCCESS") {
                    self.finished = true;
                    return Ok(vec![StreamMessage::Error {
                        message: json
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or(status)
                            .to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    }]);
                }
                let mut out = Vec::new();
                if !self.saw_text_delta {
                    if let Some(response) = json.get("response").and_then(Value::as_str) {
                        if !response.is_empty() {
                            self.emitted_text = response.to_string();
                            out.push(StreamMessage::Text {
                                content: response.to_string(),
                            });
                        }
                    }
                }
                let session_id = id.ok_or_else(|| {
                    "terminal success without a valid conversation id".to_string()
                })?;
                self.finished = true;
                out.push(StreamMessage::Done {
                    result: self.emitted_text.clone(),
                    session_id: Some(session_id),
                });
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
        if exit_code.unwrap_or(0) != 0 {
            return Ok(vec![StreamMessage::Error {
                message: if stderr.trim().is_empty() {
                    format!("agy exited with status {exit_code:?}")
                } else {
                    stderr.trim().to_string()
                },
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
            }]);
        }
        Err("AGY stream ended without a terminal result".into())
    }
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
        assert!(
            result
                .iter()
                .any(|message| matches!(message, StreamMessage::Done { .. }))
        );
    }
}
