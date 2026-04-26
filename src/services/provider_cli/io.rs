use std::path::Path;

use super::paths;
use super::registry::{
    LaunchArtifact, ProviderCliMigrationState, ProviderCliRegistry, SmokeResult,
};

#[derive(Debug)]
pub enum IoError {
    Io(std::io::Error),
    Json(serde_json::Error),
    NoRuntimeRoot,
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoError::Io(e) => write!(f, "io: {e}"),
            IoError::Json(e) => write!(f, "json: {e}"),
            IoError::NoRuntimeRoot => write!(f, "runtime root not configured"),
        }
    }
}

impl From<std::io::Error> for IoError {
    fn from(e: std::io::Error) -> Self {
        IoError::Io(e)
    }
}

impl From<serde_json::Error> for IoError {
    fn from(e: serde_json::Error) -> Self {
        IoError::Json(e)
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), IoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, IoError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(serde_json::from_str(&content)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IoError::Io(e)),
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

pub fn load_registry(root: &Path) -> Result<Option<ProviderCliRegistry>, IoError> {
    read_json(&paths::registry_path(root))
}

pub fn save_registry(root: &Path, registry: &ProviderCliRegistry) -> Result<(), IoError> {
    write_json(&paths::registry_path(root), registry)
}

// ── Migration state ───────────────────────────────────────────────────────────

pub fn load_migration_state(
    root: &Path,
    provider: &str,
) -> Result<Option<ProviderCliMigrationState>, IoError> {
    read_json(&paths::migration_state_path(root, provider))
}

pub fn save_migration_state(root: &Path, state: &ProviderCliMigrationState) -> Result<(), IoError> {
    write_json(&paths::migration_state_path(root, &state.provider), state)
}

// ── Launch artifact ───────────────────────────────────────────────────────────

pub fn save_launch_artifact(root: &Path, artifact: &LaunchArtifact) -> Result<(), IoError> {
    let key = artifact.session_key.as_deref().unwrap_or("default");
    write_json(&paths::launch_artifact_path(root, key), artifact)
}

pub fn load_launch_artifact(
    root: &Path,
    session_key: &str,
) -> Result<Option<LaunchArtifact>, IoError> {
    read_json(&paths::launch_artifact_path(root, session_key))
}

// ── Smoke result ──────────────────────────────────────────────────────────────

pub fn save_smoke_result(root: &Path, result: &SmokeResult) -> Result<(), IoError> {
    write_json(
        &paths::smoke_result_path(root, &result.provider, &result.channel),
        result,
    )
}

pub fn load_smoke_result(
    root: &Path,
    provider: &str,
    channel: &str,
) -> Result<Option<SmokeResult>, IoError> {
    read_json(&paths::smoke_result_path(root, provider, channel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider_cli::registry::ProviderCliRegistry;

    #[test]
    fn registry_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ProviderCliRegistry::default();
        save_registry(dir.path(), &registry).unwrap();
        let loaded = load_registry(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.schema_version, 1);
    }

    #[test]
    fn missing_registry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_registry(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn migration_state_round_trip() {
        use crate::services::provider_cli::registry::{MigrationState, ProviderCliMigrationState};
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();
        let state = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::Planned,
            selected_agent_id: None,
            current_channel: None,
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &state).unwrap();
        let loaded = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        assert_eq!(loaded.provider, "codex");
    }
}
