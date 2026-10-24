//! Pure native-row classification; it never publishes prompts or binding observations.

use std::path::Path;
use std::time::SystemTime;

use chrono::DateTime;
use serde_json::Value;

use crate::services::tui_o::shadow::SourceId;

pub(super) fn last_dialogue(bytes: &[u8], source: &SourceId, root: &Path) -> Option<SystemTime> {
    let id = uuid::Uuid::parse_str(&source.session_id).ok()?;
    if id.hyphenated().to_string() != source.session_id {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    let native_name = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
    };
    let canonical = source.path.canonicalize().ok()?;
    if !mutant("age_root")
        && (!canonical.starts_with(root.canonicalize().ok()?)
            || !native_name(&source.path)
            || !native_name(&canonical))
    {
        return None;
    }
    let mut rows = bytes.strip_suffix(b"\n")?.split(|byte| *byte == b'\n');
    let header: Value = serde_json::from_slice(rows.next()?).ok()?;
    let meta = header.get("payload")?;
    if !mutant("age_header")
        && (header["type"] != "session_meta"
            || meta["id"].as_str() != Some(source.session_id.as_str())
            || !meta["cwd"]
                .as_str()
                .is_some_and(|cwd| Path::new(cwd).is_absolute())
            || !matches!(meta["source"].as_str(), Some("cli" | "exec" | "vscode"))
            || meta
                .get("parent_thread_id")
                .is_some_and(|parent| !parent.is_null()))
    {
        return None;
    }
    let mut last = None;
    for line in rows {
        let row: Value = serde_json::from_slice(line).ok()?;
        if dialogue(&row)? {
            let timestamp =
                SystemTime::from(DateTime::parse_from_rfc3339(row["timestamp"].as_str()?).ok()?);
            last = Some(if mutant("age_latest") {
                timestamp
            } else {
                last.map_or(timestamp, |previous: SystemTime| previous.max(timestamp))
            });
        }
    }
    last
}

fn dialogue(row: &Value) -> Option<bool> {
    let payload = &row["payload"];
    let native = || payload.is_object().then_some(false);
    match row["type"].as_str()? {
        "turn_context" | "compacted" | "world_state" | "token_usage_record" => native(),
        "inter_agent_communication_metadata" => (payload["trigger_turn"] == false).then_some(false),
        "turn.completed" => Some(false),
        "item.completed" => completed(&row["item"]),
        "response_item" => match payload["type"].as_str()? {
            "message" => {
                let dialogue = match payload["role"].as_str()? {
                    "user" | "assistant" => true,
                    "developer" | "system" => false,
                    _ => return None,
                };
                content(&payload["content"]).then_some(dialogue)
            }
            "agent_message" => payload["text"].is_string().then_some(true),
            "reasoning"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "tool_search_call"
            | "tool_search_output" => native(),
            _ => mutant("age_unknown").then_some(false),
        },
        "event_msg" => match payload["type"].as_str()? {
            "user_message" | "agent_message" => payload["message"].is_string().then_some(true),
            "item_completed" => completed(&payload["item"]),
            "task_complete" => match payload.get("last_agent_message") {
                None | Some(Value::Null) => Some(false),
                Some(Value::String(body)) => Some(!body.trim().is_empty()),
                _ => None,
            },
            "token_count"
            | "agent_reasoning"
            | "composer_ready"
            | "task_started"
            | "turn_aborted"
            | "thread_settings_applied" => native(),
            _ => mutant("age_unknown").then_some(false),
        },
        _ => mutant("age_unknown").then_some(false),
    }
}

fn completed(item: &Value) -> Option<bool> {
    match item["type"].as_str()? {
        // Native mirrors may carry only an id; their timestamp is conservative activity evidence.
        "UserMessage" | "AgentMessage" => {
            if let Some(body) = item.get("content") {
                content(body).then_some(true)
            } else {
                item["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(|_| true)
            }
        }
        "user_message" | "agent_message" => item["text"].is_string().then_some(true),
        "Reasoning" | "CommandExecution" | "FileChange" | "McpToolCall" | "WebSearch"
        | "ContextCompaction" | "EnteredReviewMode" | "ExitedReviewMode" | "reasoning"
        | "command_execution" | "file_change" | "mcp_tool_call" | "web_search" => Some(false),
        _ => mutant("age_unknown").then_some(false),
    }
}

fn content(value: &Value) -> bool {
    value.as_array().is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|item| match item["type"].as_str() {
                Some("text" | "input_text" | "output_text") => item["text"].is_string(),
                Some("input_image" | "image") => {
                    item["image_url"].is_string() || item["url"].is_string()
                }
                _ => false,
            })
    })
}

#[cfg(test)]
fn mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_BA_MUTANT").is_ok_and(|value| value == name)
}

#[cfg(not(test))]
fn mutant(_name: &str) -> bool {
    false
}
