//! The Decision source at the request boundaries. Its authority is an approved PG event, so a
//! request that names the source in its own body or metadata is refused before any backend.

use crate::db::decisions::SOURCE;

/// Whether a request body claims the Decision source, in its source or its input metadata.
pub(crate) fn self_declared(source: Option<&str>, metadata: Option<&serde_json::Value>) -> bool {
    let named = |value: &str| value.trim().eq_ignore_ascii_case(SOURCE);
    let field = |key| {
        metadata
            .and_then(|metadata| metadata.pointer(key))
            .and_then(serde_json::Value::as_str)
    };
    source.is_some_and(named)
        || field("/human_input/source").is_some_and(named)
        || field("/human_input/origin_id").is_some_and(|origin| origin.starts_with("decision:"))
}

pub(crate) const SOURCE_REFUSED: &str = "decision_source_not_allowed";

/// A handoff never carries a Decision input: both its HTTP route and its service entry ask here.
pub(crate) fn reject_handoff_source(
    source: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> Result<(), &'static str> {
    if self_declared(source, metadata) {
        return Err(SOURCE_REFUSED);
    }
    Ok(())
}

#[cfg(test)]
#[path = "decision_drain_tests.rs"]
mod tests;
