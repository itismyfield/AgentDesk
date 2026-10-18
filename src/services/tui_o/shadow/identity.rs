//! Transcript identity rules: which records carry output units, how each unit is keyed,
//! and which records open or close a turn. Changing a rule here requires `IDENTITY_VERSION`.

use serde_json::Value;

use super::{ShadowProvider, UnitKind};
use crate::services::discord::response_sanitizer::is_standalone_tui_response_chrome;
// Variant values only: the write-zero allowlist audits these two, not ProviderKind's methods.
use crate::services::provider::ProviderKind::{Claude as ClaudeKind, Codex as CodexKind};
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

fn claude(record: &Value) -> Vec<RecordFact> {
    // Strict E wins over any other reading of the row, e.g. an interrupt marker user row.
    if envelope_is_turn_end_terminator(&ClaudeKind, record) {
        return vec![RecordFact::Idle(None)];
    }
    match str_at(record, "type") {
        Some("assistant") => std::iter::once(RecordFact::Assistant)
            .chain(claude_blocks(record))
            .collect(),
        Some("user") => claude_user(record),
        _ => Vec::new(),
    }
}

/// Each block is keyed `(message.id, apiBlockIndex + position)`, so a one-block row keeps its key;
/// any unplaceable block blocks the whole row.
fn claude_blocks(record: &Value) -> Vec<RecordFact> {
    let message = record.get("message").unwrap_or(&Value::Null);
    let content = message.get("content").and_then(Value::as_array);
    #[cfg(test)]
    if V3_RULE.get() && content.map(Vec::len) != Some(1) {
        return vec![blocked("assistant row without exactly one content block")];
    }
    let blocks = match content {
        Some(blocks) if !blocks.is_empty() => blocks,
        _ => return vec![blocked("assistant row without content blocks")],
    };
    // Locally synthesized rows such as API errors have no block index; the row uuid keys them.
    let api_error = record.get("isApiErrorMessage") == Some(&Value::Bool(true));
    let synthetic = api_error || str_at(message, "model") == Some("<synthetic>");
    let resumed = synthetic && !api_error;
    let index = record.get("apiBlockIndex").and_then(Value::as_u64);
    let mut facts = Vec::new();
    for (position, block) in (0u64..).zip(blocks) {
        let (kind, content) = match str_at(block, "type") {
            Some("thinking" | "redacted_thinking") => continue,
            Some("text") => (UnitKind::Body, claude_text(resumed, block)),
            Some("tool_use") => (UnitKind::Tool, tool_call(block, &["input"])),
            other => return vec![blocked(&format!("unsupported assistant block {other:?}"))],
        };
        let at = index.map(|index| index.checked_add(position));
        let native_key = match (str_at(message, "id"), at, row_key(record)) {
            (Some(id), Some(Some(at)), _) => format!("{id}:{at}"),
            // A uuid keys only one unit, so a synthetic row with several blocks stays unplaced.
            (_, None, Some(uuid)) if synthetic && blocks.len() == 1 => uuid,
            _ => return vec![blocked("assistant row without a supported identity")],
        };
        facts.push(RecordFact::Unit(native_key, kind, content));
    }
    facts
}

#[cfg(test)]
thread_local! {
    static V3_RULE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While held, this thread classifies Claude rows by the v3 rule, which blocked every assistant
/// row not holding exactly one block, so tests can build the store a v3 writer stopped on.
#[cfg(test)]
pub(crate) struct V3Rule(());

#[cfg(test)]
impl V3Rule {
    pub(crate) fn hold() -> Self {
        V3_RULE.set(true);
        Self(())
    }
}

#[cfg(test)]
impl Drop for V3Rule {
    fn drop(&mut self) {
        V3_RULE.set(false);
    }
}

/// A cold resume writes a synthetic non-error row holding only TUI chrome; Legacy strips it, so it
/// is not posted. API error rows always post.
fn claude_text(resumed: bool, block: &Value) -> UnitContent {
    let text = str_at(block, "text").unwrap_or("");
    if resumed && is_standalone_tui_response_chrome(text) {
        return UnitContent::Excluded("claude_synthetic_resume_chrome");
    }
    payload(text)
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

/// Each `tool_result` keeps its native key and historical seal; the writer excludes its body.
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
    if envelope_is_turn_end_terminator(&CodexKind, record) {
        return vec![RecordFact::Idle(None)];
    }
    let payload = record.get("payload").unwrap_or(&Value::Null);
    let turn_id = str_at(payload, "turn_id").map(str::to_owned);
    let fact = match (str_at(record, "type"), str_at(payload, "type")) {
        (Some("response_item"), Some(item_type)) => codex_item(payload, item_type),
        (Some("event_msg"), Some("task_started")) => Some(RecordFact::TurnStart(turn_id)),
        (Some("event_msg"), Some("task_complete")) => Some(RecordFact::Idle(turn_id)),
        // An abort without a turn id cannot be tied to the parent turn, so only a named one closes.
        (Some("event_msg"), Some("turn_aborted")) => turn_id.map(|id| RecordFact::Idle(Some(id))),
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
            // Native input keeps its text items even beside non-text items such as images.
            let parts = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            let texts: Vec<&str> = parts.filter_map(|part| str_at(part, "text")).collect();
            return Some(RecordFact::Prompt(false, texts.join("\n")));
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

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::{Value, json};

    use crate::services::tui_o::shadow::capture::file_identity;
    use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
    use crate::services::tui_o::writer::input_facts::{InputFacts, TurnState};

    fn event(kind: &str, turn_id: Option<&str>) -> Value {
        let mut payload = json!({"type": kind, "reason": "interrupted"});
        if let Some(turn_id) = turn_id {
            payload["turn_id"] = turn_id.into();
        }
        json!({"type": "event_msg", "payload": payload})
    }

    /// Only a synthetic non-error row that is nothing but resume chrome is excluded; API errors
    /// (even chrome-worded), chrome followed by an answer and model text still post under their keys.
    #[test]
    fn claude_synthetic_resume_chrome_alone_is_excluded() {
        use super::{RecordFact, UnitContent, classify};
        use crate::services::tui_o::shadow::UnitKind;
        let row = |uuid: &str, synthetic: bool, text: &str| {
            let (model, index) = if synthetic {
                ("<synthetic>", Value::Null)
            } else {
                ("claude-opus", json!(0))
            };
            json!({"type": "assistant", "uuid": uuid, "isApiErrorMessage": false,
                "apiBlockIndex": index, "message": {"id": "m1", "model": model,
                "content": [{"type": "text", "text": text}]}})
        };
        let api_error = |uuid: &str, text: &str| {
            let mut error = row(uuid, true, text);
            error["isApiErrorMessage"] = true.into();
            error
        };
        let payload = |text: &str| UnitContent::Payload(text.into());
        let excluded = UnitContent::Excluded("claude_synthetic_resume_chrome");
        let cases = [
            (row("s", true, " No response requested.\n"), "s", excluded),
            (
                api_error("err", "API Error: 500"),
                "err",
                payload("API Error: 500"),
            ),
            (
                api_error("err-chrome", "No response requested."),
                "err-chrome",
                payload("No response requested."),
            ),
            (
                row("more", true, "No response requested. But here is more."),
                "more",
                payload("No response requested. But here is more."),
            ),
            (
                row("model", false, "No response requested."),
                "m1:0",
                payload("No response requested."),
            ),
        ];
        for (record, key, content) in cases {
            let unit = RecordFact::Unit(key.into(), UnitKind::Body, content);
            let facts = classify(ShadowProvider::Claude, &record);
            assert_eq!(facts, [RecordFact::Assistant, unit], "{record}");
        }
    }

    /// A row holding a whole message keys each block from its `apiBlockIndex`, exactly as one-block
    /// rows of that message would; thinking yields nothing and an unplaceable block blocks the row.
    #[test]
    fn claude_multi_block_row_keys_each_block_like_one_block_rows() {
        use super::{RecordFact, UnitContent, classify};
        use crate::services::tui_o::shadow::UnitKind::{Body, Tool};
        let thinking = json!({"type": "thinking", "thinking": "t", "signature": "s"});
        let tool = json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"c": 1}});
        let text = |text: &str| json!({"type": "text", "text": text});
        let row = |index: Value, content: Vec<Value>| {
            json!({"type": "assistant", "uuid": "u", "apiBlockIndex": index,
                "message": {"id": "m", "model": "claude-opus", "content": content}})
        };
        let units = |record: &Value| -> Vec<(String, _, UnitContent)> {
            let facts = classify(ShadowProvider::Claude, record);
            assert_eq!(facts.first(), Some(&RecordFact::Assistant), "{record}");
            let unit = |fact: &RecordFact| match fact {
                RecordFact::Unit(key, kind, content) => (key.clone(), *kind, content.clone()),
                other => panic!("{record}: {other:?}"),
            };
            facts[1..].iter().map(unit).collect()
        };
        let call = UnitContent::Payload(r#"Bash: {"c":1}"#.into());
        let body = |text: &str| UnitContent::Payload(text.into());
        let incident = row(
            json!(0),
            vec![thinking.clone(), thinking.clone(), tool.clone()],
        );
        assert_eq!(units(&incident), [("m:2".into(), Tool, call.clone())]);
        assert_eq!(units(&row(json!(2), vec![tool.clone()])), units(&incident));
        assert_eq!(units(&row(json!(0), vec![thinking.clone()])), []);
        assert_eq!(
            units(&row(json!(3), vec![text("a")])),
            [("m:3".into(), Body, body("a"))]
        );
        let mixed = row(json!(1), vec![text("a"), thinking.clone(), tool, text("b")]);
        let expected = [
            ("m:1".into(), Body, body("a")),
            ("m:3".into(), Tool, call),
            ("m:4".into(), Body, body("b")),
        ];
        assert_eq!(units(&mixed), expected);

        let blocked = |record: Value, reason: &str| {
            let facts = classify(ShadowProvider::Claude, &record);
            let expected = [RecordFact::Assistant, RecordFact::Blocked(reason.into())];
            assert_eq!(facts, expected, "{record}");
        };
        let unknown = vec![text("a"), json!({"type": "image"})];
        blocked(
            row(json!(0), unknown),
            r#"unsupported assistant block Some("image")"#,
        );
        blocked(
            row(json!(0), vec![]),
            "assistant row without content blocks",
        );
        let unindexed = row(Value::Null, vec![text("a"), text("b")]);
        blocked(unindexed, "assistant row without a supported identity");
        let mut synthetic = row(Value::Null, vec![text("a"), text("b")]);
        synthetic["isApiErrorMessage"] = true.into();
        blocked(synthetic, "assistant row without a supported identity");
    }

    #[test]
    fn codex_turn_aborted_closes_only_the_named_parent_turn() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("parent.jsonl");
        let header = json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}});
        std::fs::write(&path, format!("{header}\n")).unwrap();
        let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
        let session_id = "parent".to_string();
        let source = SourceId {
            session_id,
            path: path.clone(),
            dev,
            ino,
        };
        let (channel_id, provider) = (24, ShadowProvider::Codex);
        let binding = SourceBinding {
            channel_id,
            provider,
            source,
        };
        let mut facts = InputFacts::open(binding).unwrap();
        let mut state_after = |records: &[Value]| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            for record in records {
                writeln!(file, "{record}").unwrap();
            }
            facts.poll(u64::MAX).unwrap().state
        };
        let open = |id: &str| TurnState::Open {
            native_turn_id: Some(id.into()),
        };
        let started = state_after(&[event("task_started", Some("parent"))]);
        assert_eq!(started, open("parent"));
        // An unnamed abort, a child turn's abort and a foreign one leave the parent running.
        for closer in [None, Some("child"), Some("foreign")] {
            let state = state_after(&[event("turn_aborted", closer)]);
            assert_eq!(state, open("parent"), "abort {closer:?}");
        }
        let aborted = state_after(&[event("turn_aborted", Some("parent"))]);
        assert_eq!(aborted, TurnState::Idle);
        state_after(&[event("task_started", Some("next"))]);
        let restarted = state_after(&[
            event("turn_aborted", Some("next")),
            event("task_started", Some("last")),
        ]);
        assert_eq!(restarted, open("last"), "a start after the abort wins");
    }
}
