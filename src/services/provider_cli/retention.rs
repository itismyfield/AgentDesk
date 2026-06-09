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

    pub fn is_protected_or_contains_protected(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        self.protected
            .iter()
            .any(|protected| protected == path || protected.starts_with(path))
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
