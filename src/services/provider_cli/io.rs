use std::path::Path;

use super::paths;
use super::registry::{
    LaunchArtifact, ProviderCliMigrationState, ProviderCliRegistry, SmokeResult,
};

#[derive(serde::Deserialize)]
struct LaunchArtifactProviderHint {
    provider: String,
}

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

pub fn load_launch_artifacts(root: &Path, provider: &str) -> Result<Vec<LaunchArtifact>, IoError> {
    let dir = paths::launch_artifacts_dir(root);
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                let path_provider = launch_artifact_provider_hint_from_path(&path);
                if path_provider
                    .as_deref()
                    .is_some_and(|value| value != provider)
                {
                    continue;
                }
                return Err(error.into());
            }
        };
        if launch_artifact_provider_hint(&content)
            .as_deref()
            .is_some_and(|value| value != provider)
        {
            continue;
        }
        let artifact: LaunchArtifact = serde_json::from_str(&content)?;
        if artifact.provider == provider {
            artifacts.push(artifact);
        }
    }
    Ok(artifacts)
}

fn launch_artifact_provider_hint_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    ["codex", "claude", "gemini", "opencode", "qwen"]
        .into_iter()
        .find(|provider| stem.starts_with(&format!("{provider}-")))
        .map(str::to_string)
}

fn launch_artifact_provider_hint(content: &str) -> Option<String> {
    serde_json::from_str::<LaunchArtifactProviderHint>(content)
        .map(|hint| hint.provider)
        .ok()
        .or_else(|| launch_artifact_provider_hint_from_malformed_json(content))
}

fn launch_artifact_provider_hint_from_malformed_json(content: &str) -> Option<String> {
    // `provider` is the first serialized field for launch artifacts, so partially written
    // artifacts usually still expose enough context to scope parse failures to one provider.
    let (_, after_key) = content.split_once("\"provider\"")?;
    let (_, after_colon) = after_key.split_once(':')?;
    let start = after_colon.find('"')?;
    let candidate = &after_colon[start..];
    let mut escaped = false;

    for (index, ch) in candidate.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return serde_json::from_str::<String>(&candidate[..=index]).ok(),
            _ => {}
        }
    }

    None
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
