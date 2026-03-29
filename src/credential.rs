use std::path::PathBuf;

fn agentdesk_root() -> Option<PathBuf> {
    crate::config::runtime_root()
}

/// Read a bot token from `<AGENTDESK_ROOT>/credential/<name>_bot_token`.
/// Returns the trimmed token string, or None if the file doesn't exist or is empty.
pub fn read_bot_token(name: &str) -> Option<String> {
    let root = agentdesk_root()?;
    let path = root.join("credential").join(format!("{name}_bot_token"));
    let token = std::fs::read_to_string(&path).ok()?;
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
