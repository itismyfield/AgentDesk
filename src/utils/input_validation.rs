use std::path::{Component, Path, PathBuf};

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
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("resolve runtime root '{}': {error}", root.display()))?;
    let raw_root = std::path::absolute(root)
        .map_err(|error| format!("resolve runtime root '{}': {error}", root.display()))?;
    let raw_root = normalize_lexical_path(&raw_root)?;
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        raw_root.join(candidate)
    };
    // Managed symlinks are operator-owned and may point to an external vault.
    let path = normalize_lexical_path(&candidate)?;
    if !path.starts_with(&raw_root) && !path.starts_with(&canonical_root) {
        return Err("path must stay under runtime root".to_string());
    }
    if !path.is_file() {
        return Err(format!(
            "path '{}' must be an existing file",
            path.display()
        ));
    }
    Ok(path)
}

pub(crate) fn normalize_lexical_path(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("unresolved parent path is outside runtime root".to_string());
    }
    Ok(normalized)
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
            root.join("inside.md")
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
    fn managed_path_keeps_symlinks_and_rejects_lexical_escape() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(parent.path().join("outside.md"), "outside").unwrap();
        std::fs::write(root.join("inside.md"), "inside").unwrap();
        std::os::unix::fs::symlink(parent.path().join("outside.md"), root.join("escape.md"))
            .unwrap();
        std::os::unix::fs::symlink(root.join("inside.md"), root.join("alias.md")).unwrap();
        let managed = canonicalize_under_root(&root, &root.join("escape.md")).unwrap();
        assert_eq!(managed, root.join("escape.md"));
        assert_eq!(std::fs::read(&managed).unwrap(), b"outside");
        assert!(canonicalize_under_root(&root, &root.join("../outside.md")).is_err());
        assert_eq!(
            canonicalize_under_root(&root, &root.join("alias.md")).unwrap(),
            root.join("alias.md")
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_path_accepts_raw_and_canonical_root_aliases() {
        let parent = tempfile::tempdir().unwrap();
        let actual = parent.path().join("actual");
        let alias = parent.path().join("alias");
        std::fs::create_dir(&actual).unwrap();
        std::fs::write(actual.join("inside.md"), "inside").unwrap();
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        for path in [alias.join("inside.md"), actual.join("inside.md")] {
            assert_eq!(canonicalize_under_root(&alias, &path).unwrap(), path);
        }
        assert_eq!(
            canonicalize_under_root(&alias, Path::new("inside.md")).unwrap(),
            alias.join("inside.md")
        );
    }
}
