use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const ANCHOR_CONTEXT_START: &str = "[Memento anchor context]\n";
pub(crate) const ANCHOR_CONTEXT_END: &str = "\n\n[End Memento anchor context]";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct AnchorRecord {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub scope: Option<String>,
    pub limit: Option<u64>,
    pub content: String,
}

impl AnchorRecord {
    pub(super) fn from_memento(value: &Value, content: &str) -> Self {
        let field = |name| value.get(name).and_then(Value::as_str).map(str::to_owned);
        Self {
            id: field("id"),
            kind: field("type"),
            scope: field("workspace").or_else(|| field("scope")),
            limit: value.get("charLimit").and_then(Value::as_u64),
            content: content.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionAnchorPrompt {
    pub text: String,
    pub records: Vec<AnchorRecord>,
}
