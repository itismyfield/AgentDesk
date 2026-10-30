use super::*;

pub(super) fn update_org_yaml_prompt_paths(root: &Path) -> Result<(), String> {
    let path = org_schema_path(root);
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?;
    rewrite_prompt_paths_yaml(&mut yaml, root);
    let rendered = serde_yaml::to_string(&yaml)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(&path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

fn rewrite_prompt_paths_yaml(value: &mut serde_yaml::Value, root: &Path) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, child) in map.iter_mut() {
                let key_str = key.as_str().unwrap_or_default();
                if matches!(key_str, "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = serde_yaml::Value::String(rewrite_prompt_path(raw));
                    }
                } else if key_str == "shared_prompt" {
                    *child =
                        serde_yaml::Value::String(shared_prompt_path(root).display().to_string());
                } else {
                    rewrite_prompt_paths_yaml(child, root);
                }
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for child in items {
                rewrite_prompt_paths_yaml(child, root);
            }
        }
        _ => {}
    }
}

fn rewrite_prompt_path(raw: &str) -> String {
    raw.replace("role-context/", "agents/")
        .replace("role-context\\", "agents\\")
}

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[test]
    fn layout_does_not_create_role_map_and_ignores_leftover_json() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        ensure_runtime_layout(root.path()).unwrap();
        let legacy = root.path().join("config/role_map.json");
        assert!(!legacy.exists());
        std::fs::write(&legacy, "malformed legacy JSON must not be parsed").unwrap();
        ensure_runtime_layout(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&legacy).unwrap(),
            "malformed legacy JSON must not be parsed"
        );
        assert!(!legacy.with_extension("json.migrated").exists());
    }
}
