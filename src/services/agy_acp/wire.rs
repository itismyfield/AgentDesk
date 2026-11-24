//! ACP v1 message shapes AgentDesk sends to and reads from `agy acp`.

use serde_json::{Value, json};

pub(super) const PROTOCOL_VERSION: u64 = 1;

/// What `initialize` told us about the agent; unknown capabilities stay unread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Agent {
    pub system_prompt_append: bool,
    pub version: Option<String>,
}

pub(super) fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "clientInfo": {"name": "agentdesk", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// A protocol version other than 1 is refused; v2 is out of scope.
pub(super) fn parse_initialize(result: &Value) -> Result<Agent, String> {
    let version = result.get("protocolVersion").and_then(Value::as_u64);
    if version != Some(PROTOCOL_VERSION) {
        return Err(format!(
            "agy acp speaks ACP protocol version {}, AgentDesk supports only {PROTOCOL_VERSION}",
            result.get("protocolVersion").unwrap_or(&Value::Null)
        ));
    }
    let capabilities = result.get("agentCapabilities").unwrap_or(&Value::Null);
    Ok(Agent {
        system_prompt_append: capabilities
            .pointer("/_meta/antigravity/systemPrompt/append")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        version: result
            .pointer("/agentInfo/version")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

pub(super) fn new_session_params(cwd: &str, system_append: Option<&str>) -> Value {
    let mut params = json!({"cwd": cwd, "mcpServers": []});
    if let Some(system) = system_append {
        params["_meta"] = json!({"antigravity": {"systemPrompt": {"append": system}}});
    }
    params
}

/// Session ids are opaque strings; no UUID shape is assumed.
pub(super) fn parse_session_id(result: &Value) -> Result<String, String> {
    result
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "agy acp session/new returned no sessionId".to_string())
}

/// `session/prompt`; `session/cancel` takes the same `sessionId` alone.
pub(super) fn prompt_params(session: &str, text: &str) -> Value {
    json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]})
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

pub(super) fn parse_stop_reason(result: &Value) -> StopReason {
    match result
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or("")
    {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "max_turn_requests" => StopReason::MaxTurnRequests,
        "refusal" => StopReason::Refusal,
        "cancelled" => StopReason::Cancelled,
        other => StopReason::Other(other.to_owned()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Update {
    Text(String),
    Thought,
    ToolCall {
        id: Option<String>,
        title: String,
        input: String,
    },
    ToolResult {
        id: Option<String>,
        content: String,
        is_error: bool,
    },
    /// Plans, commands, modes, config, usage: diagnostic only in this slice.
    Other,
}

const TOOL_TEXT_BYTES: usize = 2000;

/// Reads one `session/update` notification for `session`; another session's update is `None`.
pub(super) fn parse_update(params: &Value, session: &str) -> Option<Update> {
    if params.get("sessionId")?.as_str()? != session {
        return None;
    }
    let update = params.get("update")?;
    let field = |key: &str| update.get(key).unwrap_or(&Value::Null);
    let id = field("toolCallId").as_str().map(str::to_owned);
    Some(match field("sessionUpdate").as_str().unwrap_or("") {
        "agent_message_chunk" => Update::Text(flatten_content(field("content"))),
        "agent_thought_chunk" => Update::Thought,
        "tool_call" => {
            let title = field("title").as_str().or(field("kind").as_str());
            let input = bounded(&field("rawInput").to_string());
            Update::ToolCall {
                id,
                title: title.unwrap_or("tool").to_owned(),
                input,
            }
        }
        "tool_call_update" => match field("status").as_str() {
            Some(status @ ("completed" | "failed")) => Update::ToolResult {
                id,
                content: bounded(&flatten_content(field("content"))),
                is_error: status == "failed",
            },
            _ => Update::Other,
        },
        _ => Update::Other,
    })
}

fn flatten_content(content: &Value) -> String {
    match content {
        Value::Array(items) => items.iter().map(flatten_content).collect(),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| map.get("content").map(flatten_content))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn bounded(text: &str) -> String {
    if text.len() <= TOOL_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = TOOL_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// D1 = A: pick `allow_once`, else `allow_always`; with neither, or while cancelling, cancel.
pub(super) fn permission_outcome(params: &Value, cancelling: bool) -> Value {
    let options = params.get("options").and_then(Value::as_array);
    let pick = |kind: &str| {
        let mut options = options.into_iter().flatten();
        let option =
            options.find(|option| option.get("kind").and_then(Value::as_str) == Some(kind));
        option.and_then(|option| option.get("optionId").cloned())
    };
    match pick("allow_once").or_else(|| pick("allow_always")) {
        Some(option) if !cancelling => {
            json!({"outcome": {"outcome": "selected", "optionId": option}})
        }
        _ => json!({"outcome": {"outcome": "cancelled"}}),
    }
}
