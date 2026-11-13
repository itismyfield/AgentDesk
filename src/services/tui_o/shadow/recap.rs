//! Claude's out-of-turn recap is a separate immutable unit.

use serde_json::Value;

use crate::domain::message_split::{
    PLAIN_MESSAGE_SPLIT_LIMIT, byte_index_at_discord_message_units,
};

use super::UnitKind;
use super::identity::{RecordFact, UnitContent};

pub(super) fn classify(record: &Value) -> Option<RecordFact> {
    let uuid = record
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())?;
    let text = record.get("content").and_then(Value::as_str)?;
    let text = text.trim_end();
    let text = text
        .strip_suffix("(disable recaps in /config)")
        .unwrap_or(text)
        .trim_end();
    let content = if text.trim().is_empty() {
        UnitContent::Excluded("empty_payload")
    } else {
        let mut rendered = String::from("-# 🧭 recap");
        // Reserve three prefix units and the newline so the splitter keeps complete rows.
        let body_limit = PLAIN_MESSAGE_SPLIT_LIMIT - 3 - 1;
        for line in text.lines() {
            rendered.push_str("\n-# ");
            let mut remaining = line;
            while !remaining.is_empty() {
                let end = byte_index_at_discord_message_units(remaining, body_limit);
                rendered.push_str(&remaining[..end]);
                remaining = &remaining[end..];
                if !remaining.is_empty() {
                    rendered.push_str("\n-# ");
                }
            }
        }
        UnitContent::Payload(rendered)
    };
    Some(RecordFact::Unit(
        format!("away_summary:{uuid}"),
        UnitKind::Recap,
        content,
    ))
}
