use std::path::{Path, PathBuf};

fn agentdesk_root() -> Option<PathBuf> {
    crate::config::runtime_root()
}

fn read_trimmed_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Read a bot token from the canonical runtime credential path.
/// Legacy `config/credential/` entries are migrated into `credential/` on read.
pub fn read_bot_token(name: &str) -> Option<String> {
    let root = agentdesk_root()?;
    let _ = crate::runtime_layout::ensure_credential_layout(&root);
    let path = crate::runtime_layout::credential_token_path(&root, name);
    read_trimmed_token(&path)
}
