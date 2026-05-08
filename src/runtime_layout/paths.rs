use std::path::{Path, PathBuf};

pub(super) fn current_home_dir() -> Option<PathBuf> {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        if let Ok(slot) = test_home_dir_override_slot().lock() {
            if let Some(override_path) = slot.clone() {
                return override_path;
            }
        }
    }
    dirs::home_dir()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn test_home_dir_override_slot() -> &'static std::sync::Mutex<Option<Option<PathBuf>>> {
    static OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<Option<PathBuf>>>> =
        std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn set_test_home_dir_override(path: Option<PathBuf>) {
    if let Ok(mut slot) = test_home_dir_override_slot().lock() {
        *slot = Some(path);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn clear_test_home_dir_override() {
    if let Ok(mut slot) = test_home_dir_override_slot().lock() {
        *slot = None;
    }
}

pub fn config_dir(root: &Path) -> PathBuf {
    root.join("config")
}

pub fn credential_dir(root: &Path) -> PathBuf {
    root.join("credential")
}

pub fn legacy_credential_dir(root: &Path) -> PathBuf {
    config_dir(root).join("credential")
}

pub fn credential_token_path(root: &Path, bot_name: &str) -> PathBuf {
    credential_dir(root).join(format!("{bot_name}_bot_token"))
}

pub fn config_file_path(root: &Path) -> PathBuf {
    config_dir(root).join("agentdesk.yaml")
}

pub fn legacy_config_file_path(root: &Path) -> PathBuf {
    root.join("agentdesk.yaml")
}

pub fn role_map_path(root: &Path) -> PathBuf {
    config_dir(root).join("role_map.json")
}

pub fn org_schema_path(root: &Path) -> PathBuf {
    config_dir(root).join("org.yaml")
}

pub fn memory_backend_path(root: &Path) -> PathBuf {
    config_dir(root).join("memory-backend.json")
}

pub fn managed_agents_root(root: &Path) -> PathBuf {
    config_dir(root).join("agents")
}

pub fn shared_prompt_path(root: &Path) -> PathBuf {
    managed_agents_root(root).join("_shared.prompt.md")
}

pub fn managed_memories_root(root: &Path) -> PathBuf {
    config_dir(root).join("memories")
}

pub fn shared_agent_knowledge_dir(root: &Path) -> PathBuf {
    managed_memories_root(root).join("shared-agent-knowledge")
}

pub(super) fn default_shared_agent_knowledge_path(root: &Path) -> PathBuf {
    shared_agent_knowledge_dir(root).join("shared_knowledge.md")
}

pub(super) fn default_shared_agent_memory_root(root: &Path) -> PathBuf {
    managed_memories_root(root).join("shared-agent-memory")
}

pub(super) fn default_long_term_memory_root(root: &Path) -> PathBuf {
    managed_memories_root(root).join("long-term")
}

pub fn memories_archive_root(root: &Path) -> PathBuf {
    managed_memories_root(root).join("archive")
}

pub fn managed_skills_root(root: &Path) -> PathBuf {
    root.join("skills")
}

pub fn managed_skills_manifest_path(root: &Path) -> PathBuf {
    managed_skills_root(root).join("manifest.json")
}

pub fn resolve_memory_path(root: &Path, raw: &str) -> PathBuf {
    let raw_path = expand_user_path(raw).unwrap_or_else(|| PathBuf::from(raw));
    if raw_path.is_absolute() {
        raw_path
    } else {
        config_dir(root).join(raw_path)
    }
}

pub fn expand_user_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(stripped) = trimmed.strip_prefix("~/") {
        let home = current_home_dir()?;
        return Some(home.join(stripped));
    }
    if trimmed == "~" {
        return current_home_dir();
    }
    Some(PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_resolve_memory_path_absolute() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let raw = if cfg!(windows) {
            "C:\\absolute\\path"
        } else {
            "/absolute/path"
        };
        let resolved = resolve_memory_path(root, raw);
        assert_eq!(resolved, PathBuf::from(raw));
    }

    #[test]
    fn test_resolve_memory_path_relative() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let resolved = resolve_memory_path(root, "relative/path");
        assert_eq!(resolved, config_dir(root).join("relative/path"));
    }
}
