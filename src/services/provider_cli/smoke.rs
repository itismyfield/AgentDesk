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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, script: &str) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        let mut perms = f.metadata().unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn smoke_version_ok_for_working_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakeprovider");
        write_executable(&bin, "#!/bin/sh\necho 'fakeprovider 1.2.3'\n");
        let path = bin.to_str().unwrap();
        let result = run_smoke("fakeprovider", "current", path, path);
        assert!(matches!(result.checks.version, SmokeCheckStatus::Ok));
        assert!(smoke_passed(&result));
    }

    #[test]
    fn smoke_version_failed_for_missing_binary() {
        let result = run_smoke(
            "fakeprovider",
            "current",
            "/nonexistent/binary/xyz",
            "/nonexistent/binary/xyz",
        );
        assert!(matches!(result.checks.version, SmokeCheckStatus::Failed));
        assert!(!smoke_passed(&result));
    }

    #[cfg(unix)]
    #[test]
    fn smoke_timeout_returns_failed() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);

        let result = run_command_status_with_timeout(command, Duration::from_millis(50)).unwrap();

        assert!(!result);
    }

    #[test]
    fn overall_status_partial_when_some_skipped() {
        let checks = SmokeChecks {
            version: SmokeCheckStatus::Ok,
            auth: SmokeCheckStatus::Skipped,
            simple: SmokeCheckStatus::Skipped,
            structured: SmokeCheckStatus::Skipped,
            stream: SmokeCheckStatus::Skipped,
            resume: SmokeCheckStatus::Skipped,
            cancel: SmokeCheckStatus::Skipped,
        };
        assert_eq!(overall_status(&checks), "partial");
    }

    #[test]
    fn overall_status_failed_when_any_failed() {
        let checks = SmokeChecks {
            version: SmokeCheckStatus::Failed,
            auth: SmokeCheckStatus::Skipped,
            simple: SmokeCheckStatus::Skipped,
            structured: SmokeCheckStatus::Skipped,
            stream: SmokeCheckStatus::Skipped,
            resume: SmokeCheckStatus::Skipped,
            cancel: SmokeCheckStatus::Skipped,
        };
        assert_eq!(overall_status(&checks), "failed");
    }
}
