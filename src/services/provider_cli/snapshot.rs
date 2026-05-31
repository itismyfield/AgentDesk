use chrono::Utc;
use std::collections::HashMap;

use super::registry::ProviderCliChannel;
use crate::services::platform::{BinaryResolution, probe_provider_binary_version};

/// Probe the currently-resolved binary for `provider` and produce a channel snapshot.
/// Returns `None` when the binary cannot be found.
pub fn snapshot_current_channel(provider: &str) -> Option<ProviderCliChannel> {
    let probe = probe_provider_binary_version(provider);
    probe.resolution.resolved_path.as_ref()?;
    Some(channel_from_probe(
        probe.resolution,
        probe.version_output,
        probe.probe_failure_kind,
        &probe.skipped_candidate_failures,
    ))
}

fn channel_from_probe(
    resolution: BinaryResolution,
    version_output: Option<String>,
    version_probe_error: Option<String>,
    skipped_failures: &[String],
) -> ProviderCliChannel {
    let resolved_path = resolution
        .resolved_path
        .as_ref()
        .expect("channel_from_probe requires a resolved path");
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
    if !skipped_failures.is_empty() {
        evidence.insert(
            "skipped_candidate_failures".to_string(),
            skipped_failures.join(" | "),
        );
    }
    if !resolution.attempts.is_empty() {
        evidence.insert(
            "resolution_attempts".to_string(),
            resolution.attempts.join(" | "),
        );
    }

    ProviderCliChannel {
        path: resolved_path.clone(),
        canonical_path,
        version,
        version_output,
        source,
        checked_at: Utc::now(),
        evidence,
    }
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::platform::resolve_provider_binary;
    use std::sync::MutexGuard;

    fn env_guard() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

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

    #[cfg(unix)]
    #[test]
    fn snapshot_current_channel_skips_failed_first_candidate() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let broken_dir = temp.path().join("broken");
        let working_dir = temp.path().join("working");
        std::fs::create_dir_all(&broken_dir).unwrap();
        std::fs::create_dir_all(&working_dir).unwrap();

        let provider = "agentdesk-test-snapshot-provider";
        let broken = broken_dir.join(provider);
        let working = working_dir.join(provider);
        write_executable(&broken, "#!/bin/sh\nexit 2\n");
        write_executable(&working, "#!/bin/sh\nprintf 'snapshot-provider 1.0\\n'\n");

        let original_path = std::env::var_os("PATH");
        let override_var = "AGENTDESK_AGENTDESK_TEST_SNAPSHOT_PROVIDER_PATH";
        let original_override = std::env::var_os(override_var);
        let path = std::env::join_paths([&broken_dir, &working_dir]).unwrap();
        unsafe {
            std::env::set_var("PATH", path);
            std::env::remove_var(override_var);
        }

        let snap = snapshot_current_channel(provider).unwrap();

        assert_eq!(snap.path, working.to_string_lossy().as_ref());
        assert_eq!(snap.version, "snapshot-provider 1.0");
        assert!(
            snap.evidence
                .get("skipped_candidate_failures")
                .is_some_and(|value| value.contains("version_probe_failed"))
        );

        unsafe {
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_override {
                Some(value) => std::env::set_var(override_var, value),
                None => std::env::remove_var(override_var),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_current_channel_does_not_mark_selected_failed_candidate_skipped() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let broken_dir = temp.path().join("broken");
        std::fs::create_dir_all(&broken_dir).unwrap();

        let provider = "agentdesk-test-snapshot-failed-provider";
        let broken = broken_dir.join(provider);
        write_executable(&broken, "#!/bin/sh\nexit 2\n");

        let original_path = std::env::var_os("PATH");
        let override_var = "AGENTDESK_AGENTDESK_TEST_SNAPSHOT_FAILED_PROVIDER_PATH";
        let original_override = std::env::var_os(override_var);
        unsafe {
            std::env::set_var("PATH", &broken_dir);
            std::env::remove_var(override_var);
        }

        let snap = snapshot_current_channel(provider).unwrap();

        assert_eq!(snap.path, broken.to_string_lossy().as_ref());
        assert_eq!(snap.version, "");
        assert_eq!(
            snap.evidence.get("version_probe_error").map(String::as_str),
            Some("version_probe_failed")
        );
        assert!(!snap.evidence.contains_key("skipped_candidate_failures"));

        unsafe {
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_override {
                Some(value) => std::env::set_var(override_var, value),
                None => std::env::remove_var(override_var),
            }
        }
    }
}
