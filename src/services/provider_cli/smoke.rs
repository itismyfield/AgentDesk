use chrono::Utc;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::registry::{SmokeCheckStatus, SmokeChecks, SmokeResult};

const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

struct CheckRunner<'a> {
    provider: &'a str,
    binary: &'a str,
    canonical_path: &'a str,
}

impl<'a> CheckRunner<'a> {
    fn run_version(&self) -> SmokeCheckStatus {
        let mut command = Command::new(self.binary);
        command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::services::platform::augment_exec_path(&mut command, self.canonical_path);
        configure_version_probe_command(&mut command, self.provider);

        match run_command_status_with_timeout(command, SMOKE_TIMEOUT) {
            Ok(true) => SmokeCheckStatus::Ok,
            _ => SmokeCheckStatus::Failed,
        }
    }

    /// All checks beyond `version` require an actual authenticated session and
    /// are marked Skipped unless a live environment is detected. This avoids
    /// network calls or token usage during unit/integration tests.
    fn run_auth(&self) -> SmokeCheckStatus {
        SmokeCheckStatus::Skipped
    }

    fn run_simple(&self) -> SmokeCheckStatus {
        SmokeCheckStatus::Skipped
    }

    fn run_structured(&self) -> SmokeCheckStatus {
        SmokeCheckStatus::Skipped
    }

    fn run_stream(&self) -> SmokeCheckStatus {
        SmokeCheckStatus::Skipped
    }

    fn run_resume(&self) -> SmokeCheckStatus {
        SmokeCheckStatus::Skipped
    }

    fn run_cancel(&self) -> SmokeCheckStatus {
        SmokeCheckStatus::Skipped
    }
}

fn configure_version_probe_command(command: &mut Command, provider: &str) {
    if provider == "claude" {
        // `--version` never routes models or spawns subagents, so probes always run
        // native (Scrub), independent of gateway/config state. Turn launches use
        // `resolve_for_launch` elsewhere.
        crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv::Scrub
            .apply_to_command(command);
    }
}

fn run_command_status_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> std::io::Result<bool> {
    crate::services::process::configure_child_process_group(&mut command);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                crate::services::process::kill_child_tree(&mut child);
                return Ok(false);
            }
            Err(error) => {
                crate::services::process::kill_child_tree(&mut child);
                return Err(error);
            }
        }
    }
}

fn overall_status(checks: &SmokeChecks) -> &'static str {
    let statuses = [
        &checks.version,
        &checks.auth,
        &checks.simple,
        &checks.structured,
        &checks.stream,
        &checks.resume,
        &checks.cancel,
    ];
    let failed = statuses
        .iter()
        .any(|s| matches!(s, SmokeCheckStatus::Failed));
    let all_ok = statuses.iter().all(|s| matches!(s, SmokeCheckStatus::Ok));
    if failed {
        "failed"
    } else if all_ok {
        "ok"
    } else {
        "partial"
    }
}

/// Run smoke checks against `binary_path` for `provider`/`channel`.
///
/// The `version` check requires the binary to be executable. All other checks
/// are `Skipped` unless a full live environment is available (PR-3 scope
/// intentionally limits to version gate only to avoid auth/network dependency).
pub fn run_smoke(
    provider: &str,
    channel: &str,
    binary_path: &str,
    canonical_path: &str,
) -> SmokeResult {
    let runner = CheckRunner {
        provider,
        binary: binary_path,
        canonical_path,
    };

    let checks = SmokeChecks {
        version: runner.run_version(),
        auth: runner.run_auth(),
        simple: runner.run_simple(),
        structured: runner.run_structured(),
        stream: runner.run_stream(),
        resume: runner.run_resume(),
        cancel: runner.run_cancel(),
    };

    let status = overall_status(&checks).to_string();
    let mut evidence = HashMap::new();
    evidence.insert("binary_path".to_string(), binary_path.to_string());
    evidence.insert("canonical_path".to_string(), canonical_path.to_string());

    SmokeResult {
        provider: provider.to_string(),
        channel: channel.to_string(),
        candidate_path: binary_path.to_string(),
        canonical_path: canonical_path.to_string(),
        checks,
        overall_status: status,
        evidence,
        checked_at: Utc::now(),
    }
}

/// Returns `true` when the smoke result is acceptable for promotion gating.
/// The version check must be `Ok`; everything else may be `Skipped`.
pub fn smoke_passed(result: &SmokeResult) -> bool {
    matches!(result.checks.version, SmokeCheckStatus::Ok)
        && !matches!(result.overall_status.as_str(), "failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_version_probe_env(provider: &str) -> HashMap<String, Option<String>> {
        let mut command = Command::new(provider);
        command
            .env("ANTHROPIC_BASE_URL", "http://inherited.example:9999")
            .env(
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
                "inherited-value",
            );
        configure_version_probe_command(&mut command, provider);
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn version_probe_scrubs_gateway_env_only_for_claude() {
        let claude_env = configured_version_probe_env("claude");
        assert_eq!(claude_env.get("ANTHROPIC_BASE_URL"), Some(&None));
        assert_eq!(
            claude_env.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&None)
        );

        let codex_env = configured_version_probe_env("codex");
        assert_eq!(
            codex_env.get("ANTHROPIC_BASE_URL"),
            Some(&Some("http://inherited.example:9999".to_string()))
        );
        assert_eq!(
            codex_env.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&Some("inherited-value".to_string()))
        );
    }
}
