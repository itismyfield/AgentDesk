use std::path::{Path, PathBuf};

/// Agent IDs are also directory names; preserve the setup API's identifier rules.
pub(crate) fn validate_agent_id(value: &str) -> Result<(), &'static str> {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        Ok(())
    } else {
        Err("agent_id must be a non-empty path-safe identifier")
    }
}

pub(crate) fn canonicalize_under_root(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("resolve runtime root '{}': {error}", root.display()))?;
    let path = candidate
        .canonicalize()
        .map_err(|error| format!("resolve path '{}': {error}", candidate.display()))?;
    if !path.starts_with(&root) {
        return Err("path must stay under runtime root".to_string());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_preserves_every_legacy_ascii_identifier_character() {
        for byte in 0u8..=127 {
            let id = (byte as char).to_string();
            let expected = byte.is_ascii_alphanumeric() || b"-_".contains(&byte);
            assert_eq!(validate_agent_id(&id).is_ok(), expected, "{byte}");
        }
        for id in ["project-agentdesk", "agent_2", "GPT-6_1", "123", "-", "_"] {
            assert!(validate_agent_id(id).is_ok(), "{id}");
        }
        assert!(validate_agent_id(&"a-_Z9".repeat(100)).is_ok());
        for id in [
            "", ".", "..", "../bad", "a/b", "a\\b", "a.b", "한글", "a\0b",
        ] {
            assert!(validate_agent_id(id).is_err(), "{id:?}");
        }
    }

    #[test]
    fn canonical_path_rejects_outside_parent_and_sibling_prefix() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runtime");
        let sibling = parent.path().join("runtime-other");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(root.join("inside.md"), "inside").unwrap();
        std::fs::write(sibling.join("outside.md"), "outside").unwrap();
        assert_eq!(
            canonicalize_under_root(&root, &root.join("inside.md")).unwrap(),
            root.join("inside.md").canonicalize().unwrap()
        );
        for path in [
            sibling.join("outside.md"),
            root.join("../runtime-other/outside.md"),
            root.join("missing.md"),
        ] {
            assert!(
                canonicalize_under_root(&root, &path).is_err(),
                "{}",
                path.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_path_resolves_symlinks_before_containment() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(parent.path().join("outside.md"), "outside").unwrap();
        std::fs::write(root.join("inside.md"), "inside").unwrap();
        std::os::unix::fs::symlink(parent.path().join("outside.md"), root.join("escape.md"))
            .unwrap();
        std::os::unix::fs::symlink(root.join("inside.md"), root.join("alias.md")).unwrap();
        assert!(canonicalize_under_root(&root, &root.join("escape.md")).is_err());
        assert_eq!(
            canonicalize_under_root(&root, &root.join("alias.md")).unwrap(),
            root.join("inside.md").canonicalize().unwrap()
        );
    }
}
