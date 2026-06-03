use chrono::Utc;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::registry::{SmokeCheckStatus, SmokeChecks, SmokeResult};

const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

struct CheckRunner<'a> {
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
