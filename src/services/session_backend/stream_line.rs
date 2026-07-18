//! #3405: stream-line state machine cluster, split verbatim out of
//! `session_backend.rs`. Owns the per-line JSONL parse state
//! (`StreamLineState`/`TaskStartInfo`), the single-line processor
//! (`process_stream_line`), and the synchronous envelope parsers that turn one
//! normalized wrapper frame into a `StreamMessage`. The terminal-usage adoption
//! gate it calls (`adopt_terminal_result_usage`) lives in the sibling
//! `terminal_usage` child and resolves through the parent glob below.

use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamLineState {
    pub last_session_id: Option<String>,
    pub last_model: Option<String>,
    /// #1918: input/cache_read/cache_create record the **last** API call's
    /// per-message usage so the status panel Context line reflects current
    /// context occupancy (sum across multi-call turns inflated past the
    /// window). `accum_output_tokens` stays cumulative because turn analytics
    /// and persisted token totals expect the sum across all calls.
    pub accum_input_tokens: u64,
    pub accum_cache_create_tokens: u64,
    pub accum_cache_read_tokens: u64,
    pub accum_output_tokens: u64,
    /// True once any per-message `usage` block has been observed in the
    /// stream. Lets the result-event handler fall back to `result.usage`
    /// only for providers (e.g. Qwen) that emit token counts solely on the
    /// terminal result event.
    pub saw_per_message_usage: bool,
    pub final_result: Option<String>,
    pub stdout_error: Option<(String, String)>,
    pub tool_use_names: HashMap<String, String>,
    pub task_starts: HashMap<String, TaskStartInfo>,
    /// #3281 observability-only harvest counters (never gate delivery); see
    /// [`ReadHarvestStats`] for the counting rules.
    pub forwarded_message_count: u64,
    pub forwarded_assistant_text_bytes: u64,
}

impl StreamLineState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskStartInfo {
    pub tool_use_id: Option<String>,
    pub task_type: Option<String>,
}

/// Process a single normalized wrapper JSONL line.
///
/// Unknown or malformed Claude envelope types are non-terminal: they are
/// ignored and return `true` so future TUI history metadata cannot end the
/// turn reader early. `false` is reserved for a disconnected sender channel.
pub fn process_stream_line(
    line: &str,
    sender: &Sender<StreamMessage>,
    state: &mut StreamLineState,
) -> bool {
    if line.trim().is_empty() {
        return true;
    }

    let json = match serde_json::from_str::<Value>(line) {
        Ok(json) => json,
        Err(_) => return true,
    };

    let msg_type = json
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");

    if msg_type == "assistant" {
        if let Some(message) = json.get("message") {
            let current_model = message
                .get("model")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            if let Some(model) = current_model.as_ref() {
                state.last_model = Some(model.clone());
            }
            if let Some(usage) = message.get("usage") {
                // #1918: input/cache_read/cache_create replace so persisted
                // analytics reflect the LAST API call's prompt; the previous
                // sum across multi-call (tool-use loop) turns inflated the
                // recorded context tokens past the window. output_tokens stays
                // accumulated for the cumulative output metric analytics
                // expect.
                state.saw_per_message_usage = true;
                let input_tokens = usage.get("input_tokens").and_then(|value| value.as_u64());
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(|value| value.as_u64());
                let cache_creation = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|value| value.as_u64());
                state.accum_input_tokens = input_tokens.unwrap_or(0);
                state.accum_cache_read_tokens = cache_read.unwrap_or(0);
                state.accum_cache_create_tokens = cache_creation.unwrap_or(0);
                if let Some(output_tokens) =
                    usage.get("output_tokens").and_then(|value| value.as_u64())
                {
                    state.accum_output_tokens =
                        state.accum_output_tokens.saturating_add(output_tokens);
                }

                if let (Some(input_tokens), Some(cache_creation), Some(cache_read)) =
                    (input_tokens, cache_creation, cache_read)
                {
                    if sender
                        .send(StreamMessage::ActiveUsageSnapshot {
                            model: current_model,
                            input_tokens,
                            cache_create_tokens: cache_creation,
                            cache_read_tokens: cache_read,
                        })
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
    }

    if msg_type == "result" {
        // #1918/#3344: provenance + magnitude gated. Codex-legacy terminal usage
        // is known-cumulative and suppressed so analytics re-parse never carries
        // poisoned context numbers. See [`adopt_terminal_result_usage`].
        adopt_terminal_result_usage(&json, state);

        let cost_usd = json.get("cost_usd").and_then(|value| value.as_f64());
        let total_cost_usd = json.get("total_cost_usd").and_then(|value| value.as_f64());
        let duration_ms = json.get("duration_ms").and_then(|value| value.as_u64());
        let num_turns = json
            .get("num_turns")
            .and_then(|value| value.as_u64())
            .map(|value| value as u32);
        if cost_usd.is_some() || total_cost_usd.is_some() || state.last_model.is_some() {
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: state.last_model.clone(),
                cost_usd,
                total_cost_usd,
                duration_ms,
                num_turns,
                input_tokens: (state.accum_input_tokens > 0).then_some(state.accum_input_tokens),
                cache_create_tokens: (state.accum_cache_create_tokens > 0)
                    .then_some(state.accum_cache_create_tokens),
                cache_read_tokens: (state.accum_cache_read_tokens > 0)
                    .then_some(state.accum_cache_read_tokens),
                output_tokens: (state.accum_output_tokens > 0).then_some(state.accum_output_tokens),
            });
        }
    }

    observe_stream_context(&json, state);
    if !emit_status_events_from_stream_json(&json, sender) {
        return false;
    }

    let Some(message) = parse_stream_message_with_state(&json, state) else {
        return true;
    };

    match &message {
        StreamMessage::Init { session_id, .. } => {
            state.last_session_id = Some(session_id.clone());
        }
        StreamMessage::Done { result, session_id } => {
            state.final_result = Some(result.clone());
            if session_id.is_some() {
                state.last_session_id = session_id.clone();
            }
        }
        StreamMessage::Error { message, .. } => {
            state.stdout_error = Some((message.clone(), line.to_string()));
            return true;
        }
        _ => {}
    }

    // #3281: harvest accounting, observation-only (counted on successful send).
    // `StatusUpdate` (turn_duration housekeeping) is metadata, not content.
    let (harvested, text_bytes) = match &message {
        StreamMessage::Text { content } => (1, content.len() as u64),
        StreamMessage::Done { .. }
        | StreamMessage::StatusUpdate { .. }
        | StreamMessage::ActiveUsageSnapshot { .. } => (0, 0),
        _ => (1, 0),
    };
    if sender.send(message).is_err() {
        return false;
    }
    state.forwarded_message_count += harvested;
    state.forwarded_assistant_text_bytes += text_bytes;

    for extra in parse_assistant_extra_tool_uses(&json) {
        if sender.send(extra).is_err() {
            return false;
        }
        state.forwarded_message_count += 1;
    }

    true
}

pub(crate) fn emit_status_events_from_stream_json(
    json: &Value,
    sender: &Sender<StreamMessage>,
) -> bool {
    let events = status_events_from_workflow_json(json);
    if events.is_empty() {
        return true;
    }
    sender.send(StreamMessage::StatusEvents { events }).is_ok()
}

pub fn parse_stream_message(json: &Value) -> Option<StreamMessage> {
    let mut state = StreamLineState::new();
    observe_stream_context(json, &mut state);
    parse_stream_message_with_state(json, &state)
}

fn json_str<'a>(json: &'a Value, key: &str) -> Option<&'a str> {
    json.get(key).and_then(Value::as_str)
}

pub(crate) fn parse_stream_message_with_state(
    json: &Value,
    state: &StreamLineState,
) -> Option<StreamMessage> {
    let msg_type = json.get("type")?.as_str()?;

    match msg_type {
        "system" => {
            let subtype = json.get("subtype").and_then(|value| value.as_str())?;
            match subtype {
                "init" => {
                    let session_id = json.get("session_id")?.as_str()?.to_string();
                    Some(StreamMessage::Init {
                        session_id,
                        raw_session_id: None,
                    })
                }
                "task_notification" => Some(StreamMessage::TaskNotification {
                    task_id: json_str(json, "task_id").unwrap_or("").to_string(),
                    tool_use_id: ["tool_use_id", "tool-use-id", "toolUseId"]
                        .into_iter()
                        .find_map(|key| json_str(json, key))
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                    status: json_str(json, "status").unwrap_or("").to_string(),
                    summary: json_str(json, "summary").unwrap_or("").to_string(),
                    kind: classify_task_notification_kind(json, state),
                }),
                "stop_hook_summary" => Some(StreamMessage::Done {
                    result: String::new(),
                    session_id: claude_session_id(json),
                }),
                "turn_duration" => Some(StreamMessage::StatusUpdate {
                    model: state.last_model.clone(),
                    cost_usd: None,
                    total_cost_usd: None,
                    duration_ms: json
                        .get("durationMs")
                        .or_else(|| json.get("duration_ms"))
                        .and_then(|value| value.as_u64()),
                    num_turns: json
                        .get("messageCount")
                        .or_else(|| json.get("num_turns"))
                        .and_then(|value| value.as_u64())
                        .map(|value| value as u32),
                    input_tokens: (state.accum_input_tokens > 0)
                        .then_some(state.accum_input_tokens),
                    cache_create_tokens: (state.accum_cache_create_tokens > 0)
                        .then_some(state.accum_cache_create_tokens),
                    cache_read_tokens: (state.accum_cache_read_tokens > 0)
                        .then_some(state.accum_cache_read_tokens),
                    output_tokens: (state.accum_output_tokens > 0)
                        .then_some(state.accum_output_tokens),
                }),
                _ => None,
            }
        }
        "assistant" => {
            let content = json.get("message")?.get("content")?.as_array()?;
            let mut has_thinking = false;

            for item in content {
                let item_type = match item.get("type").and_then(|value| value.as_str()) {
                    Some(item_type) => item_type,
                    None => continue,
                };
                match item_type {
                    "text" => {
                        let text = item
                            .get("text")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            return Some(StreamMessage::Text {
                                content: text.to_string(),
                            });
                        }
                    }
                    "tool_use" => {
                        let name = item
                            .get("name")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if !name.is_empty() {
                            let input = item
                                .get("input")
                                .map(|value| {
                                    serde_json::to_string_pretty(value).unwrap_or_default()
                                })
                                .unwrap_or_default();
                            let tool_use_id = item
                                .get("id")
                                .and_then(|value| value.as_str())
                                .map(str::to_string);
                            return Some(StreamMessage::ToolUse {
                                name: name.to_string(),
                                input,
                                tool_use_id,
                            });
                        }
                    }
                    "thinking" => {
                        has_thinking = true;
                    }
                    _ => {}
                }
            }

            if has_thinking {
                return Some(StreamMessage::redacted_thinking());
            }
            None
        }
        "user" => {
            let content = json.get("message")?.get("content")?.as_array()?;
            for item in content {
                let item_type = item.get("type")?.as_str()?;
                if item_type == "tool_result" {
                    let content_text = if let Some(text) =
                        item.get("content").and_then(|value| value.as_str())
                    {
                        text.to_string()
                    } else if let Some(items) =
                        item.get("content").and_then(|value| value.as_array())
                    {
                        items
                            .iter()
                            .filter_map(|value| value.get("text").and_then(|text| text.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    };
                    let is_error = item
                        .get("is_error")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let tool_use_id = item
                        .get("tool_use_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    return Some(StreamMessage::ToolResult {
                        content: content_text,
                        is_error,
                        tool_use_id,
                    });
                }
            }
            None
        }
        "result" => {
            let is_error = json
                .get("is_error")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if is_error {
                let error_message = json
                    .get("errors")
                    .and_then(|value| value.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|value| value.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .or_else(|| {
                        json.get("result")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "Unknown error".to_string());
                return Some(StreamMessage::Error {
                    message: error_message,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Some(StreamMessage::Done {
                result: json
                    .get("result")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                session_id: claude_session_id(json),
            })
        }
        _ => None,
    }
}

fn claude_session_id(json: &Value) -> Option<String> {
    json.get("session_id")
        .or_else(|| json.get("sessionId"))
        .and_then(|value| value.as_str())
        .map(String::from)
}

pub(crate) fn observe_stream_context(json: &Value, state: &mut StreamLineState) {
    let Some(msg_type) = json.get("type").and_then(|value| value.as_str()) else {
        return;
    };

    match msg_type {
        "assistant" => {
            let Some(content) = json
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_array())
            else {
                return;
            };

            for item in content {
                if item.get("type").and_then(|value| value.as_str()) != Some("tool_use") {
                    continue;
                }
                let Some(tool_use_id) = item.get("id").and_then(|value| value.as_str()) else {
                    continue;
                };
                let Some(tool_name) = item.get("name").and_then(|value| value.as_str()) else {
                    continue;
                };
                state
                    .tool_use_names
                    .insert(tool_use_id.to_string(), tool_name.to_string());
            }
        }
        "system" => {
            if json.get("subtype").and_then(|value| value.as_str()) != Some("task_started") {
                return;
            }
            let Some(task_id) = json.get("task_id").and_then(|value| value.as_str()) else {
                return;
            };
            state.task_starts.insert(
                task_id.to_string(),
                TaskStartInfo {
                    tool_use_id: json
                        .get("tool_use_id")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                    task_type: json
                        .get("task_type")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                },
            );
        }
        _ => {}
    }
}

pub(crate) fn classify_task_notification_kind(
    json: &Value,
    state: &StreamLineState,
) -> TaskNotificationKind {
    if let Some(kind) = json
        .get("task_notification_kind")
        .and_then(|value| value.as_str())
        .and_then(TaskNotificationKind::from_str)
    {
        return kind;
    }

    let task_id = json.get("task_id").and_then(|value| value.as_str());
    let task_info = task_id.and_then(|id| state.task_starts.get(id));
    let tool_use_id = json
        .get("tool_use_id")
        .and_then(|value| value.as_str())
        .or_else(|| task_info.and_then(|info| info.tool_use_id.as_deref()));
    let tool_name = tool_use_id.and_then(|id| state.tool_use_names.get(id).map(String::as_str));
    let task_type = task_info.and_then(|info| info.task_type.as_deref());
    let summary = json
        .get("summary")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("");

    if tool_name == Some("Monitor")
        || task_type == Some("monitor")
        || summary.starts_with("Monitor event:")
    {
        return TaskNotificationKind::MonitorAutoTurn;
    }

    if task_type == Some("local_agent") {
        return TaskNotificationKind::Subagent;
    }

    TaskNotificationKind::Background
}

/// Extract tool_use blocks that appear after an initial text block in a single
/// assistant event so downstream relay logic sees both narration and tools.
pub fn parse_assistant_extra_tool_uses(json: &Value) -> Vec<StreamMessage> {
    if json.get("type").and_then(|value| value.as_str()) != Some("assistant") {
        return Vec::new();
    }

    let content = match json
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
    {
        Some(content) => content,
        None => return Vec::new(),
    };

    let mut saw_text_first = false;
    let mut extras = Vec::new();
    for item in content {
        let item_type = match item.get("type").and_then(|value| value.as_str()) {
            Some(item_type) => item_type,
            None => continue,
        };
        match item_type {
            "text" if extras.is_empty() => {
                let text = item
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    saw_text_first = true;
                }
            }
            "tool_use" if saw_text_first => {
                let name = item
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if !name.is_empty() {
                    let input = item
                        .get("input")
                        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
                        .unwrap_or_default();
                    let tool_use_id = item
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    extras.push(StreamMessage::ToolUse {
                        name: name.to_string(),
                        input,
                        tool_use_id,
                    });
                }
            }
            _ => {}
        }
    }

    extras
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// #3292 non-regression: normal assistant text still counts as harvested
    /// content, while the following `Done` terminator does not inflate the
    /// count.
    #[test]
    fn assistant_text_plus_done_counts_text_only() {
        let (sender, receiver) = mpsc::channel();
        let mut state = StreamLineState::new();
        assert!(process_stream_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
            &sender,
            &mut state,
        ));
        assert!(process_stream_line(
            r#"{"type":"system","subtype":"stop_hook_summary","session_id":"sess-text"}"#,
            &sender,
            &mut state,
        ));

        assert_eq!(state.forwarded_message_count, 1);
        assert_eq!(state.forwarded_assistant_text_bytes, 5);
        let messages: Vec<_> = receiver.try_iter().collect();
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Text { content } if content == "hello")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Done { session_id, .. } if session_id.as_deref() == Some("sess-text"))
        ));
    }

    #[test]
    fn assistant_usage_emits_complete_active_snapshot_before_done() {
        let (sender, receiver) = mpsc::channel();
        let mut state = StreamLineState::new();

        assert!(process_stream_line(
            r#"{"type":"assistant","message":{"model":"routed-sonnet[1m]","usage":{"input_tokens":560000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}"#,
            &sender,
            &mut state,
        ));

        let messages: Vec<_> = receiver.try_iter().collect();
        assert!(matches!(
            messages.as_slice(),
            [StreamMessage::ActiveUsageSnapshot {
                model,
                input_tokens: 560_000,
                cache_create_tokens: 0,
                cache_read_tokens: 0,
            }] if model.as_deref() == Some("routed-sonnet[1m]")
        ));
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, StreamMessage::Done { .. }))
        );
    }

    #[test]
    fn active_snapshot_does_not_inherit_a_previous_record_model() {
        let (sender, receiver) = mpsc::channel();
        let mut state = StreamLineState::new();
        assert!(process_stream_line(
            r#"{"type":"assistant","message":{"model":"old-route[1m]","content":[]}}"#,
            &sender,
            &mut state,
        ));
        assert!(process_stream_line(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":560000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}"#,
            &sender,
            &mut state,
        ));

        let messages: Vec<_> = receiver.try_iter().collect();
        assert!(matches!(
            messages.as_slice(),
            [StreamMessage::ActiveUsageSnapshot { model: None, .. }]
        ));
    }

    #[test]
    fn assistant_usage_without_complete_triple_does_not_emit_active_snapshot() {
        let (sender, receiver) = mpsc::channel();
        let mut state = StreamLineState::new();
        assert!(process_stream_line(
            r#"{"type":"assistant","message":{"model":"routed-sonnet[1m]","usage":{"input_tokens":560000,"cache_read_input_tokens":0},"content":[]}}"#,
            &sender,
            &mut state,
        ));
        assert!(receiver.try_iter().next().is_none());
    }

    /// #3281 zero side: a transcript window containing ONLY housekeeping lines
    /// (queued-prompt attachment / last-prompt / ai-title / mode records /
    /// `turn_duration` telemetry) harvests nothing — the counters stay 0 even
    /// though `turn_duration` still reaches the bridge as a `StatusUpdate`.
    /// This is the substrate of the `Completed`-only zero-harvest gate: such a
    /// read may complete via an uncounted synthetic idle-timeout `Done`, and
    /// the producer-exit log must then report `lines=0` as a REAL measurement.
    #[test]
    fn housekeeping_only_window_harvests_nothing() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut state = StreamLineState::new();
        for line in [
            r#"{"type":"attachment","attachment":{"kind":"queued-prompt"}}"#,
            r#"{"type":"last-prompt","prompt":"hi"}"#,
            r#"{"type":"ai-title","title":"greeting"}"#,
            r#"{"type":"mode","mode":"default"}"#,
            r#"{"type":"permission-mode","mode":"default"}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":10298}"#,
        ] {
            assert!(process_stream_line(line, &sender, &mut state));
        }
        assert_eq!(
            state.forwarded_message_count, 0,
            "housekeeping-only window must count zero harvested messages"
        );
        assert_eq!(
            state.forwarded_assistant_text_bytes, 0,
            "housekeeping-only window must harvest zero assistant text bytes"
        );
        let forwarded: Vec<_> = receiver.try_iter().collect();
        assert!(
            forwarded
                .iter()
                .all(|message| matches!(message, StreamMessage::StatusUpdate { .. })),
            "only status telemetry may reach the bridge from housekeeping lines: {forwarded:?}"
        );
        assert!(
            !forwarded.is_empty(),
            "turn_duration must still reach the bridge as StatusUpdate telemetry"
        );
    }

    #[test]
    fn process_stream_line_emits_workflow_status_events() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut state = StreamLineState::new();

        assert!(process_stream_line(
            r#"{"type":"system","subtype":"task_progress","task_id":"wf-1","workflow_progress":[{"type":"workflow_phase","index":1,"title":"P1"},{"type":"workflow_agent","index":1,"label":"pinger","phaseIndex":1,"phaseTitle":"P1","state":"progress"}]}"#,
            &sender,
            &mut state,
        ));

        let message = receiver
            .try_iter()
            .find(|message| matches!(message, StreamMessage::StatusEvents { .. }))
            .expect("workflow status events");
        let StreamMessage::StatusEvents { events } = message else {
            panic!("expected StatusEvents");
        };
        assert_eq!(
            events,
            vec![
                crate::services::agent_protocol::StatusEvent::WorkflowPhase {
                    task_id: Some("wf-1".to_string()),
                    index: 1,
                    title: "P1".to_string()
                },
                crate::services::agent_protocol::StatusEvent::WorkflowAgent {
                    task_id: Some("wf-1".to_string()),
                    index: 1,
                    label: "pinger".to_string(),
                    phase_index: Some(1),
                    phase_title: Some("P1".to_string()),
                    state: "progress".to_string()
                }
            ]
        );
    }
}
