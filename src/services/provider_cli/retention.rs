use std::path::{Path, PathBuf};

/// Paths currently referenced by any active migration channel; these must
/// never be deleted by a cleanup pass.
#[derive(Debug, Default)]
pub struct RetentionSet {
    protected: Vec<PathBuf>,
}

impl RetentionSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn protect(&mut self, path: impl AsRef<Path>) {
        let p = path.as_ref().to_path_buf();
        if !self.protected.contains(&p) {
            self.protected.push(p);
        }
    }

    pub fn is_protected(&self, path: impl AsRef<Path>) -> bool {
        self.protected.contains(&path.as_ref().to_path_buf())
    }

    pub fn is_protected_or_contains_protected(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        self.protected
            .iter()
            .any(|protected| protected == path || protected.starts_with(path))
    }

    pub fn protected_paths(&self) -> &[PathBuf] {
        &self.protected
    }
}

/// Build a retention set from a registry and migration state.
pub fn build_retention_set(
    registry: &crate::services::provider_cli::registry::ProviderCliRegistry,
    migration_states: &[crate::services::provider_cli::registry::ProviderCliMigrationState],
) -> RetentionSet {
    let mut set = RetentionSet::new();

    for channels in registry.providers.values() {
        for ch in [
            channels.current.as_ref(),
            channels.candidate.as_ref(),
            channels.default.as_ref(),
            channels.previous.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            set.protect(&ch.path);
            set.protect(&ch.canonical_path);
        }
    }

    for state in migration_states {
        for ch in [
            state.current_channel.as_ref(),
            state.candidate_channel.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            set.protect(&ch.path);
            set.protect(&ch.canonical_path);
        }
        if let Some(path) = state.rollback_target.as_deref() {
            set.protect(path);
            set.protect(
                crate::services::provider_cli::paths::preserved_previous_tree_path(Path::new(path)),
            );
        }
    }

    set
}

/// Dry-run: list paths under `scan_dir` that are NOT in the retention set.
/// Does not delete anything.
pub fn cleanup_dry_run(scan_dir: &Path, set: &RetentionSet) -> std::io::Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    let entries = match std::fs::read_dir(scan_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !set.is_protected_or_contains_protected(&path) {
            candidates.push(path);
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider_cli::registry::{
        MigrationState, ProviderChannels, ProviderCliChannel, ProviderCliMigrationState,
        ProviderCliRegistry,
    };
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_channel(path: &str) -> ProviderCliChannel {
        ProviderCliChannel {
            path: path.to_string(),
            canonical_path: path.to_string(),
            version: "1.0.0".to_string(),
            version_output: None,
            source: "test".to_string(),
            checked_at: Utc::now(),
            evidence: HashMap::new(),
        }
    }

    #[test]
    fn protected_paths_not_in_cleanup_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let protected = dir.path().join("protected_binary");
        let removable = dir.path().join("old_binary");
        std::fs::write(&protected, b"bin").unwrap();
        std::fs::write(&removable, b"bin").unwrap();

        let mut set = RetentionSet::new();
        set.protect(&protected);

        let candidates = cleanup_dry_run(dir.path(), &set).unwrap();
        assert!(candidates.contains(&removable));
        assert!(!candidates.contains(&protected));
    }

    #[test]
    fn cleanup_dry_run_treats_missing_scan_dir_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let set = RetentionSet::new();

        let candidates = cleanup_dry_run(&missing, &set).unwrap();

        assert!(candidates.is_empty());
    }

    #[test]
    fn cleanup_dry_run_keeps_dirs_that_contain_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let protected_dir = dir.path().join("previous-package");
        let protected_binary = protected_dir.join("bin").join("codex");
        let removable_dir = dir.path().join("old-package");
        std::fs::create_dir_all(protected_binary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&removable_dir).unwrap();
        std::fs::write(&protected_binary, b"bin").unwrap();

        let mut set = RetentionSet::new();
        set.protect(&protected_binary);

        let candidates = cleanup_dry_run(dir.path(), &set).unwrap();
        assert!(candidates.contains(&removable_dir));
        assert!(!candidates.contains(&protected_dir));
    }

    #[test]
    fn build_retention_set_includes_registry_channels() {
        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels::default();
        channels.current = Some(make_channel("/usr/local/bin/codex"));
        channels.previous = Some(make_channel("/usr/local/bin/codex.prev"));
        registry.providers.insert("codex".to_string(), channels);

        let set = build_retention_set(&registry, &[]);
        assert!(set.is_protected("/usr/local/bin/codex"));
        assert!(set.is_protected("/usr/local/bin/codex.prev"));
    }

    #[test]
    fn build_retention_set_includes_rollback_target() {
        let state = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryActive,
            selected_agent_id: None,
            current_channel: None,
            candidate_channel: None,
            rollback_target: Some("/tmp/codex.rollback".to_string()),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };

        let set = build_retention_set(&ProviderCliRegistry::default(), &[state]);
        assert!(set.is_protected("/tmp/codex.rollback"));
        assert!(set.is_protected("/tmp/codex.rollback.tree"));
    }
}
