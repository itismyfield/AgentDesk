use chrono::Utc;
use std::collections::HashMap;

use super::registry::ProviderCliChannel;
use crate::services::platform::{probe_resolved_binary_version, resolve_provider_binary};

/// Probe the currently-resolved binary for `provider` and produce a channel snapshot.
/// Returns `None` when the binary cannot be found.
pub fn snapshot_current_channel(provider: &str) -> Option<ProviderCliChannel> {
    let resolution = resolve_provider_binary(provider);
    let resolved_path = resolution.resolved_path.as_ref()?;

    let binary_path = std::path::Path::new(resolved_path);
    let (version_output, version_probe_error) =
        probe_resolved_binary_version(binary_path, &resolution);

    let version = version_output
        .as_deref()
        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        .unwrap_or_default();

    let canonical_path = resolution
        .canonical_path
        .clone()
        .unwrap_or_else(|| resolved_path.clone());

    let source = resolution.source.clone().unwrap_or_default();

    let mut evidence = HashMap::new();
    if let Some(output) = &version_output {
        evidence.insert("version_output_len".to_string(), output.len().to_string());
    }
    if let Some(error) = &version_probe_error {
        evidence.insert("version_probe_error".to_string(), error.clone());
    }
    if let Some(failure) = &resolution.failure_kind {
        evidence.insert("failure_kind".to_string(), failure.clone());
    }

    Some(ProviderCliChannel {
        path: resolved_path.clone(),
        canonical_path,
        version,
        version_output,
        source,
        checked_at: Utc::now(),
        evidence,
    })
}

/// Returns SHA-256 hex of the file at `path`, or `None` on I/O error.
pub fn file_sha256(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = sha2_hasher();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex_encode(hasher.finalize()))
}

fn sha2_hasher() -> Sha256State {
    Sha256State::new()
}

struct Sha256State {
    inner: sha2::Sha256,
}

impl Sha256State {
    fn new() -> Self {
        use sha2::Digest;
        Self {
            inner: sha2::Sha256::new(),
        }
    }

    fn update(&mut self, data: &[u8]) {
        use sha2::Digest;
        self.inner.update(data);
    }

    fn finalize(self) -> Vec<u8> {
        use sha2::Digest;
        self.inner.finalize().to_vec()
    }
}

fn hex_encode(bytes: Vec<u8>) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_unknown_provider_returns_none() {
        // A provider name that will never resolve to a real binary.
        let result = snapshot_current_channel("__nonexistent_provider_xyz__");
        assert!(result.is_none());
    }

    #[test]
    fn snapshot_has_expected_fields_when_binary_found() {
        // Only runs if `codex` or `claude` is actually on PATH.
        for provider in &["codex", "claude"] {
            let resolution = resolve_provider_binary(provider);
            if resolution.resolved_path.is_none() {
                continue;
            }
            let snap = snapshot_current_channel(provider).unwrap();
            assert!(!snap.path.is_empty());
            assert!(!snap.canonical_path.is_empty());
            assert!(!snap.source.is_empty());
        }
    }
}
