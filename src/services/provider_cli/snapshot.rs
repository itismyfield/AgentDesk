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
