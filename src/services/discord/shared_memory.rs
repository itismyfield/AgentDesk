use std::fs;

use super::runtime_store::shared_agent_memory_root;

/// Load agent-specific notes from {role_id}.json's notes[] field.
/// Returns formatted [Shared Agent Memory] section, or None if empty.
pub(super) fn load_agent_notes(role_id: &str) -> Option<String> {
    let root = shared_agent_memory_root()?;
    let path = root.join(format!("{}.json", role_id));
    let content = fs::read_to_string(&path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    let notes = parsed.get("notes")?.as_array()?;
    if notes.is_empty() {
        return None;
    }
    let mut lines = vec!["[Shared Agent Memory]".to_string()];
    for note in notes {
        if let Some(content) = note.get("content").and_then(|v| v.as_str()) {
            let source = note
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let created = note
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!("- [{}|{}] {}", created, source, content));
        }
    }
    if lines.len() <= 1 {
        return None;
    }
    Some(lines.join("\n"))
}

/// Read shared_knowledge.md from the shared_agent_memory directory.
/// Returns the file content wrapped in a [Shared Agent Knowledge] section,
/// or None if the file doesn't exist or is empty.
pub(super) fn load_shared_knowledge() -> Option<String> {
    let root = shared_agent_memory_root()?;
    let path = root.join("shared_knowledge.md");
    let content = fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("[Shared Agent Knowledge]\n{}", trimmed))
}
