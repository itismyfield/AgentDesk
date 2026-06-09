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
