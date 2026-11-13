//! Claude's out-of-turn recap is a separate immutable unit.

use serde_json::Value;

use super::UnitKind;
use super::identity::{RecordFact, UnitContent};

pub(super) fn classify(record: &Value) -> Option<RecordFact> {
    let uuid = record
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())?;
    let text = record.get("content").and_then(Value::as_str)?;
    let content = if text.trim().is_empty() {
        UnitContent::Excluded("empty_payload")
    } else {
        let mut rendered = String::from("-# 🧭 recap");
        for line in text.lines() {
            rendered.push_str("\n-# ");
            rendered.push_str(line);
        }
        UnitContent::Payload(rendered)
    };
    Some(RecordFact::Unit(
        format!("away_summary:{uuid}"),
        UnitKind::Recap,
        content,
    ))
}
