use serde_json::Value;

use crate::domain::memory::AnchorRecord;

pub(super) fn anchor_record(value: &Value, content: &str) -> AnchorRecord {
    let field = |name| value.get(name).and_then(Value::as_str).map(str::to_owned);
    AnchorRecord {
        id: field("id"),
        kind: field("type"),
        scope: field("workspace").or_else(|| field("scope")),
        limit: value.get("charLimit").and_then(Value::as_u64),
        content: content.to_owned(),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionAnchorPrompt {
    pub text: String,
    pub records: Vec<AnchorRecord>,
}
