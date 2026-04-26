use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `~/.adk/{env}/config/provider-cli-registry.json`
pub fn registry_path(root: &Path) -> PathBuf {
    root.join("config").join("provider-cli-registry.json")
}

/// `~/.adk/{env}/state/provider-cli-migration-{provider}.json`
pub fn migration_state_path(root: &Path, provider: &str) -> PathBuf {
    root.join("state").join(format!(
        "provider-cli-migration-{}.json",
        sanitize_file_component(provider)
    ))
}

/// `~/.adk/{env}/runtime/provider-cli-launch/{session_key}.json`
pub fn launch_artifacts_dir(root: &Path) -> PathBuf {
    root.join("runtime").join("provider-cli-launch")
}

/// `~/.adk/{env}/runtime/provider-cli-launch/{session_key}.json`
pub fn launch_artifact_path(root: &Path, session_key: &str) -> PathBuf {
    launch_artifacts_dir(root).join(format!("{}.json", session_key_file_stem(session_key)))
}

/// `~/.adk/{env}/runtime/provider-cli-diagnostics/{timestamp}.json`
pub fn diagnostics_snapshot_path(root: &Path, timestamp_ms: u128) -> PathBuf {
    root.join("runtime")
        .join("provider-cli-diagnostics")
        .join(format!("{timestamp_ms}.json"))
}

/// `~/.adk/{env}/runtime/provider-cli-smoke/{provider}-{channel}.json`
pub fn smoke_result_path(root: &Path, provider: &str, channel: &str) -> PathBuf {
    root.join("runtime")
        .join("provider-cli-smoke")
        .join(format!(
            "{}-{}.json",
            sanitize_file_component(provider),
            sanitize_file_component(channel)
        ))
}

pub fn preserved_previous_tree_path(entry_path: &Path) -> PathBuf {
    let mut name = entry_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("previous-binary"));
    name.push(".tree");

    entry_path
        .parent()
        .map(|parent| parent.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn sanitize_file_component(raw: &str) -> String {
    let component: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let component = component.trim_matches(|ch| matches!(ch, '.' | '_'));
    if component.is_empty() {
        "default".to_string()
    } else {
        component.to_string()
    }
}

fn session_key_file_stem(session_key: &str) -> String {
    let digest = session_key_digest(session_key);
    format!("{}-{}", sanitize_file_component(session_key), &digest[..8])
}

fn session_key_digest(session_key: &str) -> String {
    use sha2::Digest;

    let mut hasher = sha2::Sha256::new();
    hasher.update(session_key.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_artifact_path_sanitizes_session_key_component() {
        let root = Path::new("/tmp/adk-root");
        let path = launch_artifact_path(root, "../codex/live session");

        assert_eq!(
            path.parent().unwrap(),
            root.join("runtime").join("provider-cli-launch")
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("codex_live_session-")
        );
    }

    #[test]
    fn launch_artifact_path_disambiguates_lossy_session_keys() {
        let root = Path::new("/tmp/adk-root");

        assert_ne!(
            launch_artifact_path(root, "codex/live session"),
            launch_artifact_path(root, "codex_live_session")
        );
    }

    #[test]
    fn migration_state_path_sanitizes_provider_component() {
        let root = Path::new("/tmp/adk-root");
        let path = migration_state_path(root, "../CoDeX");

        assert_eq!(
            path,
            root.join("state").join("provider-cli-migration-codex.json")
        );
    }

    #[test]
    fn smoke_result_path_sanitizes_provider_and_channel_components() {
        let root = Path::new("/tmp/adk-root");
        let path = smoke_result_path(root, "../codex", "candidate/live");

        assert_eq!(
            path,
            root.join("runtime")
                .join("provider-cli-smoke")
                .join("codex-candidate_live.json")
        );
    }

    #[test]
    fn preserved_previous_tree_path_uses_entry_sibling_tree() {
        let path = preserved_previous_tree_path(Path::new("/tmp/runtime/codex-previous-binary"));

        assert_eq!(path, Path::new("/tmp/runtime/codex-previous-binary.tree"));
    }
}
