use std::fs;

use super::runtime_store::shared_agent_knowledge_path;

/// Read shared_knowledge.md from the managed SAK path.
/// Returns the file content wrapped in a [Shared Agent Knowledge] section,
/// or None if the file doesn't exist or is empty.
pub(crate) fn load_shared_knowledge() -> Option<String> {
    let path = shared_agent_knowledge_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("[Shared Agent Knowledge]\n{}", trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&std::path::Path),
    {
        let _guard = super::super::runtime_store::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".adk");
        let sak_dir = root
            .join("config")
            .join("memories")
            .join("shared-agent-knowledge");
        std::fs::create_dir_all(&sak_dir).unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f(&sak_dir);
        match prev {
            Some(v) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", v) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn test_load_shared_knowledge_empty_returns_none() {
        with_temp_root(|sam| {
            std::fs::write(sam.join("shared_knowledge.md"), "   ").unwrap();
            assert!(load_shared_knowledge().is_none());
        });
    }

    #[test]
    fn test_load_shared_knowledge_returns_wrapped() {
        with_temp_root(|sam| {
            std::fs::write(sam.join("shared_knowledge.md"), "Some knowledge").unwrap();
            let result = load_shared_knowledge().unwrap();
            assert_eq!(result, "[Shared Agent Knowledge]\nSome knowledge");
        });
    }
}
