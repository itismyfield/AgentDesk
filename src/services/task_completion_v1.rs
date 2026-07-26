//! Closed, observation-only admission for the `task_completion_v1` wire shape.
//!
//! This module deliberately does not feed relay routing, task-card identity, or
//! user-visible rendering. Provider producers are not upgraded by this slice;
//! the parser records whether a future typed completion would be admissible.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use serde_json::Value;

const SCHEMA: &str = "task_completion_v1";
const TYPE: &str = "system";
const SUBTYPE: &str = "task_notification";
const KIND: &str = "background";

/// The only terminal statuses the first typed-completion schema admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskCompletionStatus {
    Completed,
    Failed,
    Killed,
}

impl TaskCompletionStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "killed" => Some(Self::Killed),
            _ => None,
        }
    }
}

/// Closed authority fields admitted by the version-one completion schema.
///
/// Free-form summary and result text is intentionally excluded. It remains
/// provider payload, never completion authority in this shadow slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCompletionV1 {
    pub status: TaskCompletionStatus,
    pub task_id: Option<String>,
    pub tool_use_id: Option<String>,
}

/// A comparison only succeeds if both transports carry the same authority
/// shape. A missing optional identifier is not evidence that it equals one
/// supplied by the other transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskCompletionAuthorityComparison {
    Equivalent,
    NotComparable,
}

impl TaskCompletionV1 {
    pub fn compare_authority(&self, other: &Self) -> TaskCompletionAuthorityComparison {
        if self.status == other.status
            && self.task_id == other.task_id
            && self.tool_use_id == other.tool_use_id
        {
            TaskCompletionAuthorityComparison::Equivalent
        } else {
            TaskCompletionAuthorityComparison::NotComparable
        }
    }
}

/// Closed rejection reasons keep malformed typed candidates from silently
/// degrading into legacy payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskCompletionV1Rejection {
    MalformedJson,
    DuplicateJsonKey,
    JsonRootNotObject,
    UnknownJsonField,
    MissingJsonField,
    InvalidJsonField,
    XmlRootNotBounded,
    XmlRootAttribute,
    DuplicateXmlField,
    UnknownXmlChild,
    NestedStructuralTag,
    MalformedXml,
}

/// Admission result for one raw provider frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCompletionV1Admission {
    Legacy,
    Typed(TaskCompletionV1),
    Rejected(TaskCompletionV1Rejection),
}

/// Aggregate observation counters retained by stream readers only. They are
/// diagnostics, not routing input and are intentionally not emitted as events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskCompletionV1ShadowObservations {
    pub legacy: u64,
    pub typed: u64,
    pub rejected: u64,
}

impl TaskCompletionV1ShadowObservations {
    pub fn observe(&mut self, admission: &TaskCompletionV1Admission) {
        match admission {
            TaskCompletionV1Admission::Legacy => self.legacy += 1,
            TaskCompletionV1Admission::Typed(_) => self.typed += 1,
            TaskCompletionV1Admission::Rejected(_) => self.rejected += 1,
        }
    }
}

/// Strictly parse a raw stream JSON frame. JSON with no `schema` key remains
/// legacy-compatible, even if it is a task notification. A schema-bearing
/// frame has no fallback path.
pub fn parse_raw_json(raw: &str) -> TaskCompletionV1Admission {
    let trimmed = raw.trim();
    if !trimmed.starts_with(['{', '[']) {
        return TaskCompletionV1Admission::Legacy;
    }

    let entries = match parse_json_entries(trimmed) {
        Ok(entries) => entries,
        Err(JsonEntryError::NotObject) => {
            return TaskCompletionV1Admission::Rejected(
                TaskCompletionV1Rejection::JsonRootNotObject,
            );
        }
        Err(JsonEntryError::Malformed) => {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MalformedJson);
        }
    };
    let has_schema = entries.iter().any(|(key, _)| key == "schema");
    if !has_schema {
        return TaskCompletionV1Admission::Legacy;
    }

    let mut seen = BTreeSet::new();
    for (key, _) in &entries {
        if !seen.insert(key.as_str()) {
            return TaskCompletionV1Admission::Rejected(
                TaskCompletionV1Rejection::DuplicateJsonKey,
            );
        }
    }

    let allowed = [
        "schema",
        "type",
        "subtype",
        "kind",
        "status",
        "task_id",
        "tool_use_id",
        "summary",
        "result",
    ];
    if entries
        .iter()
        .any(|(key, _)| !allowed.contains(&key.as_str()))
    {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::UnknownJsonField);
    }

    let field = |name: &str| {
        entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    let required_string = |name: &str| field(name).and_then(Value::as_str);
    let Some(schema) = required_string("schema") else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MissingJsonField);
    };
    let Some(frame_type) = required_string("type") else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MissingJsonField);
    };
    let Some(subtype) = required_string("subtype") else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MissingJsonField);
    };
    let Some(kind) = required_string("kind") else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MissingJsonField);
    };
    let Some(status) = required_string("status") else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MissingJsonField);
    };
    if schema != SCHEMA || frame_type != TYPE || subtype != SUBTYPE || kind != KIND {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::InvalidJsonField);
    }
    let Some(status) = TaskCompletionStatus::parse(status) else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::InvalidJsonField);
    };
    let task_id = match optional_json_string(field("task_id")) {
        Some(value) => value,
        None => {
            return TaskCompletionV1Admission::Rejected(
                TaskCompletionV1Rejection::InvalidJsonField,
            );
        }
    };
    let tool_use_id = match optional_json_string(field("tool_use_id")) {
        Some(value) => value,
        None => {
            return TaskCompletionV1Admission::Rejected(
                TaskCompletionV1Rejection::InvalidJsonField,
            );
        }
    };
    if field("summary").is_some_and(|value| !value.is_string())
        || field("result").is_some_and(|value| !value.is_string())
    {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::InvalidJsonField);
    }

    TaskCompletionV1Admission::Typed(TaskCompletionV1 {
        status,
        task_id,
        tool_use_id,
    })
}

/// Strictly parse a start-anchored, root-bounded XML completion frame.
///
/// The root is the existing task-notification envelope so typed producers can
/// be introduced without changing the surrounding transport. Its exact schema
/// fields are attributes; only the four known children are accepted. Summary
/// and result are opaque prose, so fake tags inside them are never structural.
pub fn parse_xml(raw: &str) -> TaskCompletionV1Admission {
    let text = crate::services::tui_prompt_control::strip_terminal_controls(raw);
    let text = text.trim();
    if !text.starts_with("<task-notification") {
        return TaskCompletionV1Admission::Legacy;
    }

    let (root_end, attributes) = match parse_open_tag(text, "task-notification") {
        Ok(value) => value,
        Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
    };
    if !attributes.contains_key("schema") {
        return TaskCompletionV1Admission::Legacy;
    }
    let allowed = ["schema", "type", "subtype", "kind", "status"];
    if attributes
        .keys()
        .any(|name| !allowed.contains(&name.as_str()))
    {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootAttribute);
    }
    let required = |name: &str| attributes.get(name).map(String::as_str);
    let (Some(schema), Some(frame_type), Some(subtype), Some(kind), Some(status)) = (
        required("schema"),
        required("type"),
        required("subtype"),
        required("kind"),
        required("status"),
    ) else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootAttribute);
    };
    if schema != SCHEMA || frame_type != TYPE || subtype != SUBTYPE || kind != KIND {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootAttribute);
    }
    let Some(status) = TaskCompletionStatus::parse(status) else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootAttribute);
    };

    let close = "</task-notification>";
    let Some(close_at) = text.rfind(close) else {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootNotBounded);
    };
    if close_at < root_end || !text[close_at + close.len()..].trim().is_empty() {
        return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootNotBounded);
    }
    let body = &text[root_end..close_at];
    let mut cursor = 0;
    let mut seen = BTreeSet::new();
    let mut task_id = None;
    let mut tool_use_id = None;
    while cursor < body.len() {
        let remaining = &body[cursor..];
        let whitespace = remaining.len() - remaining.trim_start().len();
        cursor += whitespace;
        if cursor == body.len() {
            break;
        }
        if !body[cursor..].starts_with('<') {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::UnknownXmlChild);
        }
        let (name, child_end) = match parse_child_open(&body[cursor..]) {
            Ok(value) => value,
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        if !matches!(name, "task-id" | "tool-use-id" | "summary" | "result") {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::UnknownXmlChild);
        }
        if !seen.insert(name) {
            return TaskCompletionV1Admission::Rejected(
                TaskCompletionV1Rejection::DuplicateXmlField,
            );
        }
        let content_start = cursor + child_end;
        let child_close = format!("</{name}>");
        let Some(content_end_relative) = body[content_start..].find(&child_close) else {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MalformedXml);
        };
        let content_end = content_start + content_end_relative;
        let content = &body[content_start..content_end];
        if matches!(name, "task-id" | "tool-use-id") && content.contains('<') {
            return TaskCompletionV1Admission::Rejected(
                TaskCompletionV1Rejection::NestedStructuralTag,
            );
        }
        let decoded = match decode_entities_once(content.trim()) {
            Ok(decoded) => decoded,
            Err(()) => {
                return TaskCompletionV1Admission::Rejected(
                    TaskCompletionV1Rejection::MalformedXml,
                );
            }
        };
        match name {
            "task-id" => task_id = nonempty(decoded),
            "tool-use-id" => tool_use_id = nonempty(decoded),
            // Prose is validated but never exposed or used as authority.
            "summary" | "result" => {}
            _ => unreachable!(),
        }
        cursor = content_end + child_close.len();
    }

    TaskCompletionV1Admission::Typed(TaskCompletionV1 {
        status,
        task_id,
        tool_use_id,
    })
}

fn optional_json_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None => Some(None),
        Some(Value::String(value)) => Some(nonempty(value.clone())),
        Some(_) => None,
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

struct JsonEntries(Vec<(String, Value)>);

#[derive(Debug)]
enum JsonEntryError {
    NotObject,
    Malformed,
}

impl<'de> Deserialize<'de> for JsonEntries {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct EntriesVisitor;
        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = JsonEntries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry()? {
                    entries.push((key, value));
                }
                Ok(JsonEntries(entries))
            }
        }
        deserializer.deserialize_map(EntriesVisitor)
    }
}

fn parse_json_entries(raw: &str) -> Result<Vec<(String, Value)>, JsonEntryError> {
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let entries = match JsonEntries::deserialize(&mut deserializer) {
        Ok(JsonEntries(entries)) => entries,
        Err(error) if error.to_string().contains("expected a JSON object") => {
            return Err(JsonEntryError::NotObject);
        }
        Err(_) => return Err(JsonEntryError::Malformed),
    };
    deserializer.end().map_err(|_| JsonEntryError::Malformed)?;
    Ok(entries)
}

fn parse_open_tag(
    input: &str,
    expected_name: &str,
) -> Result<(usize, std::collections::BTreeMap<String, String>), TaskCompletionV1Rejection> {
    let prefix = format!("<{expected_name}");
    if !input.starts_with(&prefix)
        || input[prefix.len()..]
            .chars()
            .next()
            .is_none_or(|character| !(character.is_whitespace() || character == '>'))
    {
        return Err(TaskCompletionV1Rejection::XmlRootNotBounded);
    }
    let end = find_tag_end(input).ok_or(TaskCompletionV1Rejection::MalformedXml)?;
    let source = &input[prefix.len()..end];
    let mut attributes = std::collections::BTreeMap::new();
    let mut rest = source;
    loop {
        if rest.is_empty() {
            break;
        }
        let whitespace = rest.len() - rest.trim_start().len();
        if whitespace == 0 {
            return Err(TaskCompletionV1Rejection::XmlRootAttribute);
        }
        rest = &rest[whitespace..];
        if rest.is_empty() {
            break;
        }
        let name_end = rest
            .find(|character: char| character == '=' || character.is_whitespace())
            .ok_or(TaskCompletionV1Rejection::XmlRootAttribute)?;
        let name = &rest[..name_end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(TaskCompletionV1Rejection::XmlRootAttribute);
        }
        let after_name = rest[name_end..].trim_start();
        let Some(after_equals) = after_name.strip_prefix('=') else {
            return Err(TaskCompletionV1Rejection::XmlRootAttribute);
        };
        let after_equals = after_equals.trim_start();
        let quote = after_equals
            .chars()
            .next()
            .filter(|character| *character == '\'' || *character == '"')
            .ok_or(TaskCompletionV1Rejection::XmlRootAttribute)?;
        let value_start = quote.len_utf8();
        let value_end = after_equals[value_start..]
            .find(quote)
            .ok_or(TaskCompletionV1Rejection::XmlRootAttribute)?;
        let value = decode_entities_once(&after_equals[value_start..value_start + value_end])
            .map_err(|()| TaskCompletionV1Rejection::MalformedXml)?;
        if attributes.insert(name.to_string(), value).is_some() {
            return Err(TaskCompletionV1Rejection::DuplicateXmlField);
        }
        rest = &after_equals[value_start + value_end + quote.len_utf8()..];
    }
    Ok((end + 1, attributes))
}

fn parse_child_open(input: &str) -> Result<(&str, usize), TaskCompletionV1Rejection> {
    let end = find_tag_end(input).ok_or(TaskCompletionV1Rejection::MalformedXml)?;
    let tag = &input[1..end];
    if tag.is_empty() || tag.starts_with('/') || tag.chars().any(char::is_whitespace) {
        return Err(TaskCompletionV1Rejection::NestedStructuralTag);
    }
    Ok((tag, end + 1))
}

fn find_tag_end(input: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in input.char_indices() {
        match quote {
            Some(current) if character == current => quote = None,
            Some(_) => {}
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character == '>' => return Some(index),
            None => {}
        }
    }
    None
}

/// Decodes XML entities exactly once and rejects all entity spellings outside
/// the closed XML subset used by the version-one payload. Both literal and
/// numeric values must be permitted XML scalar values.
fn decode_entities_once(input: &str) -> Result<String, ()> {
    let mut output = String::with_capacity(input.len());
    let mut remainder = input;
    while let Some(start) = remainder.find('&') {
        let literal = &remainder[..start];
        if !literal.chars().all(is_valid_xml_scalar) {
            return Err(());
        }
        output.push_str(literal);
        let entity = &remainder[start + 1..];
        let end = entity.find(';').ok_or(())?;
        let body = &entity[..end];
        let decoded = match body {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => parse_numeric_entity(body).ok_or(())?,
        };
        if !is_valid_xml_scalar(decoded) {
            return Err(());
        }
        output.push(decoded);
        remainder = &entity[end + 1..];
    }
    if !remainder.chars().all(is_valid_xml_scalar) {
        return Err(());
    }
    output.push_str(remainder);
    Ok(output)
}

fn parse_numeric_entity(body: &str) -> Option<char> {
    let digits = body.strip_prefix('#')?;
    let scalar = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
        (!hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(hex, 16).ok())??
    } else {
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse().ok())??
    };
    char::from_u32(scalar)
}

fn is_valid_xml_scalar(character: char) -> bool {
    let scalar = character as u32;
    matches!(scalar, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
        && !(0xFDD0..=0xFDEF).contains(&scalar)
        && scalar & 0xFFFF != 0xFFFE
        && scalar & 0xFFFF != 0xFFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<task-notification schema="task_completion_v1" type="system" subtype="task_notification" kind="background" status="completed"><task-id>task&amp;1</task-id><tool-use-id>tool-1</tool-use-id><summary>finished</summary><result>done</result></task-notification>"#;
    const JSON: &str = r#"{"schema":"task_completion_v1","type":"system","subtype":"task_notification","kind":"background","status":"completed","task_id":"task&1","tool_use_id":"tool-1","summary":"finished","result":"done"}"#;

    #[test]
    fn xml_and_json_admit_equivalent_authority() {
        let TaskCompletionV1Admission::Typed(xml) = parse_xml(XML) else {
            panic!("XML must admit");
        };
        let TaskCompletionV1Admission::Typed(json) = parse_raw_json(JSON) else {
            panic!("JSON must admit");
        };
        assert_eq!(
            xml.compare_authority(&json),
            TaskCompletionAuthorityComparison::Equivalent
        );
    }

    #[test]
    fn authority_asymmetry_is_not_comparable() {
        let TaskCompletionV1Admission::Typed(xml) = parse_xml(XML) else {
            panic!("XML must admit");
        };
        let TaskCompletionV1Admission::Typed(json) = parse_raw_json(
            r#"{"schema":"task_completion_v1","type":"system","subtype":"task_notification","kind":"background","status":"completed","task_id":"task&1"}"#,
        ) else {
            panic!("JSON must admit");
        };
        assert_eq!(
            xml.compare_authority(&json),
            TaskCompletionAuthorityComparison::NotComparable
        );
    }

    #[test]
    fn xml_rejects_duplicate_unknown_and_nested_structural_fields() {
        for (raw, reason) in [
            (
                XML.replacen(
                    "<task-id>task&amp;1</task-id>",
                    "<task-id>a</task-id><task-id>b</task-id>",
                    1,
                ),
                TaskCompletionV1Rejection::DuplicateXmlField,
            ),
            (
                XML.replacen("<summary>finished</summary>", "<unknown>x</unknown>", 1),
                TaskCompletionV1Rejection::UnknownXmlChild,
            ),
            (
                XML.replacen(
                    "<task-id>task&amp;1</task-id>",
                    "<task-id><status>completed</status></task-id>",
                    1,
                ),
                TaskCompletionV1Rejection::NestedStructuralTag,
            ),
        ] {
            assert_eq!(parse_xml(&raw), TaskCompletionV1Admission::Rejected(reason));
        }
    }

    #[test]
    fn xml_prose_fake_tags_are_not_structural() {
        let raw = XML.replacen(
            "<result>done</result>",
            "<result>literal <status>completed</status> and <unknown>x</unknown></result>",
            1,
        );
        assert!(matches!(
            parse_xml(&raw),
            TaskCompletionV1Admission::Typed(_)
        ));
    }

    #[test]
    fn xml_is_start_anchored_and_root_bounded() {
        assert_eq!(
            parse_xml(&format!("quoted {XML}")),
            TaskCompletionV1Admission::Legacy,
        );
        assert_eq!(
            parse_xml(&format!("{XML} trailing")),
            TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootNotBounded),
        );
    }

    #[test]
    fn xml_rejects_concatenated_attributes_and_invalid_entities() {
        let concatenated = XML.replacen("\" type=\"system\"", "\"type=\"system\"", 1);
        assert_eq!(
            parse_xml(&concatenated),
            TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::XmlRootAttribute),
        );
        for raw in [
            XML.replacen("task&amp;1", "task&bogus;1", 1),
            XML.replacen("task&amp;1", "task&amp1", 1),
            XML.replacen("task&amp;1", "task&#0;1", 1),
            XML.replacen("task&amp;1", "task&#xD800;1", 1),
            XML.replacen("task&amp;1", "task&#xFDD0;1", 1),
            XML.replacen("task&amp;1", "task&#12x;1", 1),
            XML.replacen("task&amp;1", "task&#x;1", 1),
        ] {
            assert_eq!(
                parse_xml(&raw),
                TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MalformedXml),
            );
        }
    }

    #[test]
    fn xml_entities_decode_once_and_admit_only_valid_scalars() {
        let raw = XML.replacen("task&amp;1", "task&amp;amp;1", 1);
        let TaskCompletionV1Admission::Typed(typed) = parse_xml(&raw) else {
            panic!("valid entity must admit");
        };
        assert_eq!(typed.task_id.as_deref(), Some("task&amp;1"));
    }

    #[test]
    fn json_rejects_duplicate_unknown_missing_invalid_and_never_falls_back() {
        let cases = [
            (
                r#"{"schema":"task_completion_v1","schema":"task_completion_v1","type":"system","subtype":"task_notification","kind":"background","status":"completed"}"#,
                TaskCompletionV1Rejection::DuplicateJsonKey,
            ),
            (
                r#"{"schema":"task_completion_v1","type":"system","subtype":"task_notification","kind":"background","status":"completed","extra":true}"#,
                TaskCompletionV1Rejection::UnknownJsonField,
            ),
            (
                r#"{"schema":"task_completion_v1","type":"system","subtype":"task_notification","kind":"background"}"#,
                TaskCompletionV1Rejection::MissingJsonField,
            ),
            (
                r#"{"schema":"task_completion_v1","type":"system","subtype":"task_notification","kind":"subagent","status":"completed"}"#,
                TaskCompletionV1Rejection::InvalidJsonField,
            ),
        ];
        for (raw, reason) in cases {
            assert_eq!(
                parse_raw_json(raw),
                TaskCompletionV1Admission::Rejected(reason)
            );
        }
    }

    #[test]
    fn schema_less_frames_remain_legacy() {
        assert_eq!(
            parse_raw_json(
                r#"{"type":"system","subtype":"task_notification","status":"completed"}"#
            ),
            TaskCompletionV1Admission::Legacy,
        );
        assert_eq!(
            parse_xml("<task-notification><status>completed</status></task-notification>"),
            TaskCompletionV1Admission::Legacy,
        );
    }

    #[test]
    fn json_root_and_mid_body_quote_have_closed_outcomes() {
        assert_eq!(
            parse_raw_json("[1, 2, 3]"),
            TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::JsonRootNotObject),
        );
        assert_eq!(
            parse_xml(&format!("prefix {XML}")),
            TaskCompletionV1Admission::Legacy,
        );
    }
}
