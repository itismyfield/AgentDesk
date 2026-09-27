//! Transcript identity rules: which records carry output units, how each unit is keyed,
//! and which records open or close a turn. Changing a rule here requires `IDENTITY_VERSION`.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{ShadowProvider, UnitKind};
use crate::services::provider::ProviderKind;
use crate::services::tui_turn_state::envelope_is_turn_end_terminator;

/// What a unit would post; `Excluded` units are recorded but never posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitContent {
    Payload(String),
    Excluded(&'static str),
}

/// One thing a transcript record tells the derive step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordFact {
    /// An output unit: native key, kind and what it would post.
    Unit(String, UnitKind, UnitContent),
    /// Output-shaped record without a supported identity; it is never sealed.
    Blocked(String),
    /// A unit whose sealing record has not been captured yet.
    Announced(String, UnitKind),
    /// Native user input; `true` when the row itself may open a turn.
    Prompt(bool, String),
    /// Assistant row; after an idle boundary it opens an autonomous turn.
    Assistant,
    TurnStart(Option<String>),
    /// Idle observation (strict E or native completion); it carries no turn authority.
    Idle(Option<String>),
}

pub fn classify(provider: ShadowProvider, record: &Value) -> Vec<RecordFact> {
    match provider {
        ShadowProvider::Claude => claude(record),
        ShadowProvider::Codex => codex(record),
    }
}

/// Row identity used for turn openers and closers.
pub fn row_key(record: &Value) -> Option<String> {
    str_at(record, "uuid").map(str::to_owned)
}

pub fn native_time(record: &Value) -> Option<DateTime<Utc>> {
    str_at(record, "timestamp")?.parse().ok()
}

fn claude(record: &Value) -> Vec<RecordFact> {
    // Strict E wins over any other reading of the row, e.g. an interrupt marker user row.
    if envelope_is_turn_end_terminator(&ProviderKind::Claude, record) {
        return vec![RecordFact::Idle(None)];
    }
    match str_at(record, "type") {
        Some("assistant") => [Some(RecordFact::Assistant), claude_block(record)]
            .into_iter()
            .flatten()
            .collect(),
        Some("user") => claude_user(record),
        _ => Vec::new(),
    }
}

/// One content block per assistant row, keyed by `(message.id, apiBlockIndex)`.
fn claude_block(record: &Value) -> Option<RecordFact> {
    let message = record.get("message").unwrap_or(&Value::Null);
    let blocks = message.get("content").and_then(Value::as_array);
    let Some([block]) = blocks.map(Vec::as_slice) else {
        return Some(blocked("assistant row without exactly one content block"));
    };
    let (kind, content) = match str_at(block, "type") {
        Some("thinking" | "redacted_thinking") => return None,
        Some("text") => (UnitKind::Body, payload(str_at(block, "text").unwrap_or(""))),
        Some("tool_use") => (UnitKind::Tool, tool_call(block, &["input"])),
        other => return Some(blocked(&format!("unsupported assistant block {other:?}"))),
    };
    // Locally synthesized rows such as API errors have no block index; the row uuid keys them.
    let synthetic = record.get("isApiErrorMessage") == Some(&Value::Bool(true))
        || str_at(message, "model") == Some("<synthetic>");
    let index = record.get("apiBlockIndex").and_then(Value::as_u64);
    let native_key = match (str_at(message, "id"), index, row_key(record)) {
        (Some(id), Some(index), _) => format!("{id}:{index}"),
        (_, None, Some(uuid)) if synthetic => uuid,
        _ => return Some(blocked("assistant row without a supported identity")),
    };
    Some(RecordFact::Unit(native_key, kind, content))
}

fn claude_user(record: &Value) -> Vec<RecordFact> {
    let native = record.get("isMeta") != Some(&Value::Bool(true));
    match record
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(text)) if native => vec![RecordFact::Prompt(true, text.clone())],
        Some(Value::Array(items)) => {
            let is_result = |item: &&Value| str_at(item, "type") == Some("tool_result");
            let results = items.iter().filter(is_result).map(claude_tool_result);
            let mut facts: Vec<RecordFact> = results.collect();
            // A row made only of tool results is not user input and opens no turn.
            if native && !items.iter().all(|item| is_result(&item)) {
                let texts: Vec<&str> = items
                    .iter()
                    .filter_map(|item| str_at(item, "text"))
                    .collect();
                facts.push(RecordFact::Prompt(true, texts.join("\n")));
            }
            facts
        }
        _ => Vec::new(),
    }
}

/// Each `tool_result` is its own unit keyed by `tool_use_id`; only error results are posted.
fn claude_tool_result(item: &Value) -> RecordFact {
    let Some(id) = str_at(item, "tool_use_id").filter(|id| !id.is_empty()) else {
        return blocked("tool_result without tool_use_id");
    };
    let content = match (item.get("is_error"), item.get("content")) {
        (Some(Value::Bool(true)), Some(Value::String(text))) => payload(text),
        (Some(Value::Bool(true)), Some(Value::Array(parts))) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| str_at(part, "text"))
                .collect();
            payload(&texts.join("\n"))
        }
        (Some(Value::Bool(true)), _) => payload(""),
        _ => UnitContent::Excluded("normal_tool_result"),
    };
    RecordFact::Unit(id.to_owned(), UnitKind::ToolResult, content)
}

fn codex(record: &Value) -> Vec<RecordFact> {
    if envelope_is_turn_end_terminator(&ProviderKind::Codex, record) {
        return vec![RecordFact::Idle(None)];
    }
    let payload = record.get("payload").unwrap_or(&Value::Null);
    let turn_id = str_at(payload, "turn_id").map(str::to_owned);
    let fact = match (str_at(record, "type"), str_at(payload, "type")) {
        (Some("response_item"), Some(item_type)) => codex_item(payload, item_type),
        (Some("event_msg"), Some("task_started")) => Some(RecordFact::TurnStart(turn_id)),
        (Some("event_msg"), Some("task_complete")) => Some(RecordFact::Idle(turn_id)),
        // `item_completed` usually precedes the response_item it mirrors: it announces, never seals.
        (Some("event_msg"), Some("item_completed")) => payload
            .get("item")
            .filter(|item| str_at(item, "type") == Some("AgentMessage"))
            .and_then(|item| str_at(item, "id"))
            .map(|id| RecordFact::Announced(id.to_owned(), UnitKind::Body)),
        _ => None,
    };
    fact.into_iter().collect()
}

fn codex_item(item: &Value, item_type: &str) -> Option<RecordFact> {
    let (kind, content) = match (item_type, str_at(item, "role")) {
        ("message", Some("assistant")) => match codex_text(item) {
            Some(text) => (UnitKind::Body, payload(&text)),
            None => return Some(blocked("assistant message with non-text content")),
        },
        ("message", Some("user")) => {
            return Some(RecordFact::Prompt(
                false,
                codex_text(item).unwrap_or_default(),
            ));
        }
        // Reasoning, other roles and inter-agent traffic are not channel output.
        ("message" | "reasoning" | "agent_message", _) => return None,
        ("function_call" | "custom_tool_call" | "tool_search_call", _) => (
            UnitKind::Tool,
            tool_call(item, &["arguments", "input", "action"]),
        ),
        ("function_call_output" | "custom_tool_call_output" | "tool_search_output", _) => {
            let excluded = UnitContent::Excluded("codex_tool_output");
            return Some(match str_at(item, "call_id").filter(|id| !id.is_empty()) {
                Some(id) => RecordFact::Unit(id.to_owned(), UnitKind::ToolResult, excluded),
                None => blocked("tool output without call_id"),
            });
        }
        (other, _) => return Some(blocked(&format!("unsupported response_item {other}"))),
    };
    Some(match str_at(item, "id").filter(|id| !id.is_empty()) {
        Some(id) => RecordFact::Unit(id.to_owned(), kind, content),
        None => blocked(&format!("response_item {item_type} without payload.id")),
    })
}

fn codex_text(item: &Value) -> Option<String> {
    let parts = item.get("content")?.as_array()?;
    let texts = parts.iter().map(|part| match str_at(part, "type") {
        Some("output_text" | "input_text" | "text") => str_at(part, "text"),
        _ => None,
    });
    texts
        .collect::<Option<Vec<&str>>>()
        .map(|texts| texts.concat())
}

/// Tool calls post their name and raw input until the writer fixes a richer rendering.
fn tool_call(item: &Value, input_fields: &[&str]) -> UnitContent {
    let name = str_at(item, "name").unwrap_or("");
    match input_fields.iter().find_map(|field| item.get(*field)) {
        Some(Value::String(input)) => payload(&format!("{name}: {input}")),
        Some(input) => payload(&format!("{name}: {input}")),
        None => payload(name),
    }
}

fn payload(text: &str) -> UnitContent {
    UnitContent::Payload(text.to_owned())
}

fn blocked(reason: &str) -> RecordFact {
    RecordFact::Blocked(reason.to_owned())
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
