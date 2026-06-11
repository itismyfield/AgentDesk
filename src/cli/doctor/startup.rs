use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub(crate) const LATEST_STARTUP_DOCTOR_ENDPOINT: &str = "/api/doctor/startup/latest";

pub(crate) fn startup_artifact_root() -> Option<PathBuf> {
    crate::cli::agentdesk_runtime_root()
        .map(|root| root.join("runtime").join("doctor").join("startup"))
}

fn boot_id_from_pid_file(pid_file: &std::path::Path) -> Result<String, String> {
    let pid = fs::read_to_string(pid_file)
        .map_err(|error| format!("read dcserver pid file {}: {error}", pid_file.display()))?
        .trim()
        .to_string();
    if pid.is_empty() {
        return Err(format!("dcserver pid file {} is empty", pid_file.display()));
    }
    let mtime = fs::metadata(pid_file)
        .and_then(|meta| meta.modified())
        .map_err(|error| {
            format!(
                "read dcserver pid file metadata {}: {error}",
                pid_file.display()
            )
        })?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("dcserver pid file mtime before epoch: {error}"))?
        .as_secs();
    Ok(format!("{pid}-{mtime}"))
}

pub(crate) fn current_boot_id() -> Result<String, String> {
    let root = crate::cli::agentdesk_runtime_root()
        .ok_or_else(|| "AGENTDESK runtime root is not resolvable".to_string())?;
    boot_id_from_pid_file(&root.join("runtime").join("dcserver.pid"))
}

pub(crate) fn latest_startup_artifact_path() -> Option<PathBuf> {
    let root = startup_artifact_root()?;
    let boot_id = current_boot_id().ok()?;
    Some(root.join(format!("{boot_id}.json")))
}

pub(crate) fn latest_startup_artifact_lock_path() -> Option<PathBuf> {
    let root = startup_artifact_root()?;
    let boot_id = current_boot_id().ok()?;
    Some(root.join(format!("{boot_id}.lock")))
}

enum LatestStartupDoctorArtifact {
    Available {
        path: PathBuf,
        report: Value,
    },
    Pending {
        path: Option<PathBuf>,
        lock_path: Option<PathBuf>,
        reason: &'static str,
    },
    Missing {
        path: Option<PathBuf>,
        reason: &'static str,
    },
    Error {
        path: Option<PathBuf>,
        error: &'static str,
        detail: String,
    },
}

fn load_latest_startup_doctor_artifact() -> LatestStartupDoctorArtifact {
    let Some(path) = latest_startup_artifact_path() else {
        return LatestStartupDoctorArtifact::Missing {
            path: None,
            reason: "startup_doctor_runtime_root_unavailable",
        };
    };

    if !path.exists() {
        let lock_path = latest_startup_artifact_lock_path();
        if lock_path.as_ref().is_some_and(|path| path.exists()) {
            return LatestStartupDoctorArtifact::Pending {
                path: Some(path),
                lock_path,
                reason: "startup_doctor_artifact_in_progress",
            };
        }
        return LatestStartupDoctorArtifact::Missing {
            path: Some(path),
            reason: "startup_doctor_artifact_missing",
        };
    }

    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return LatestStartupDoctorArtifact::Error {
                path: Some(path),
                error: "startup_doctor_artifact_read_failed",
                detail: error.to_string(),
            };
        }
    };

    match serde_json::from_str::<Value>(&content) {
        Ok(report) => LatestStartupDoctorArtifact::Available { path, report },
        Err(error) => LatestStartupDoctorArtifact::Error {
            path: Some(path),
            error: "invalid_startup_doctor_artifact",
            detail: error.to_string(),
        },
    }
}

fn path_json(path: Option<&PathBuf>) -> Value {
    path.map(|path| json!(path.display().to_string()))
        .unwrap_or(Value::Null)
}

fn count_checks_with_status(report: &Value, status: &str) -> u64 {
    report
        .get("checks")
        .and_then(Value::as_array)
        .map(|checks| {
            checks
                .iter()
                .filter(|check| check.get("status").and_then(Value::as_str) == Some(status))
                .count() as u64
        })
        .unwrap_or(0)
}

fn summary_count(report: &Value, key: &str, fallback_status: &str) -> u64 {
    report
        .get("summary")
        .and_then(|summary| summary.get(key))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| count_checks_with_status(report, fallback_status))
}

fn filtered_checks(report: &Value, status: &str) -> Value {
    report
        .get("checks")
        .and_then(Value::as_array)
        .map(|checks| {
            Value::Array(
                checks
                    .iter()
                    .filter(|check| check.get("status").and_then(Value::as_str) == Some(status))
                    .cloned()
                    .collect(),
            )
        })
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

pub(crate) fn is_provider_deferred_hooks_backlog_reason(raw: &str) -> bool {
    let parts: Vec<&str> = raw.split(':').collect();
    matches!(
        parts.as_slice(),
        ["provider", _, "deferred_hooks_backlog", count] if count.parse::<u64>().unwrap_or(0) > 0
    )
}

fn check_only_reports_provider_deferred_hooks_backlog(check: &Value) -> bool {
    let Some(reasons) = check
        .get("evidence")
        .and_then(|evidence| evidence.get("degraded_reasons"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    !reasons.is_empty()
        && reasons.iter().all(|reason| {
            reason
                .get("raw")
                .and_then(Value::as_str)
                .is_some_and(is_provider_deferred_hooks_backlog_reason)
        })
}

fn effective_count_checks_with_status(
    report: &Value,
    status: &str,
    suppress_recovered_provider_deferred_hooks_backlog: bool,
) -> Option<u64> {
    let checks = report.get("checks").and_then(Value::as_array)?;
    Some(
        checks
            .iter()
            .filter(|check| check.get("status").and_then(Value::as_str) == Some(status))
            .filter(|check| {
                !suppress_recovered_provider_deferred_hooks_backlog
                    || !check_only_reports_provider_deferred_hooks_backlog(check)
            })
            .count() as u64,
    )
}

fn startup_doctor_status(failed_count: u64, warned_count: u64) -> &'static str {
    if failed_count > 0 {
        "failed"
    } else if warned_count > 0 {
        "warned"
    } else {
        "passed"
    }
}

fn followup_context() -> &'static str {
    super::contract::RunContext::RestartFollowup.as_str()
}

fn startup_doctor_summary_json(path: &PathBuf, report: &Value, detailed: bool) -> Value {
    let failed_count = summary_count(report, "failed", "fail");
    let warned_count = summary_count(report, "warned", "warn");
    let skipped = report
        .get("skipped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let doctor_status = if skipped {
        "skipped"
    } else {
        startup_doctor_status(failed_count, warned_count)
    };
    // Use "doctor_status" (not "status") to avoid conflicting with the top-level
    // "status" field in /api/health when the no-jq regex fallback takes the first match.
    let mut summary = json!({
        "available": true,
        "doctor_status": doctor_status,
        "started_at": report.get("started_at").cloned().unwrap_or(Value::Null),
        "completed_at": report.get("completed_at").cloned().unwrap_or(Value::Null),
        "summary": report.get("summary").cloned().unwrap_or(Value::Null),
        "failed_count": failed_count,
        "warned_count": warned_count,
        "detail_endpoint": LATEST_STARTUP_DOCTOR_ENDPOINT,
        "skipped": skipped,
        "skipped_reason": report.get("skipped_reason").cloned().unwrap_or(Value::Null),
    });

    if detailed {
        // artifact_path and boot_id are internal metadata; expose only on protected paths.
        summary["artifact_path"] = Value::String(path.display().to_string());
        summary["boot_id"] = report.get("boot_id").cloned().unwrap_or(Value::Null);
        summary["run_context"] = report.get("run_context").cloned().unwrap_or(Value::Null);
        summary["non_fatal"] = report.get("non_fatal").cloned().unwrap_or(Value::Null);
        summary["failed_checks"] = filtered_checks(report, "fail");
        summary["warned_checks"] = filtered_checks(report, "warn");
        summary["followup_context"] = json!(followup_context());
    }

    summary
}

/// Counts startup doctor checks that should still worsen live `/api/health`.
/// A provider deferred-hook backlog found during startup is transient if the
/// current live provider snapshot has already drained it; keep the artifact
/// visible, but do not let that recovered boot-time check latch top-level
/// health forever.
pub(crate) fn latest_startup_doctor_effective_counts(
    suppress_recovered_provider_deferred_hooks_backlog: bool,
) -> (u64, u64) {
    match load_latest_startup_doctor_artifact() {
        LatestStartupDoctorArtifact::Available { report, .. } => {
            let failed = effective_count_checks_with_status(
                &report,
                "fail",
                suppress_recovered_provider_deferred_hooks_backlog,
            )
            .unwrap_or_else(|| summary_count(&report, "failed", "fail"));
            let warned = effective_count_checks_with_status(
                &report,
                "warn",
                suppress_recovered_provider_deferred_hooks_backlog,
            )
            .unwrap_or_else(|| summary_count(&report, "warned", "warn"));
            (failed, warned)
        }
        LatestStartupDoctorArtifact::Pending { .. }
        | LatestStartupDoctorArtifact::Missing { .. }
        | LatestStartupDoctorArtifact::Error { .. } => (0, 0),
    }
}

pub(crate) fn latest_startup_doctor_health_json(detailed: bool) -> Value {
    match load_latest_startup_doctor_artifact() {
        LatestStartupDoctorArtifact::Available { path, report } => {
            startup_doctor_summary_json(&path, &report, detailed)
        }
        LatestStartupDoctorArtifact::Pending {
            path,
            lock_path,
            reason,
        } => {
            let mut summary = json!({
                "available": false,
                "doctor_status": "pending",
                "summary": Value::Null,
                "failed_count": 0,
                "warned_count": 0,
                "detail_endpoint": LATEST_STARTUP_DOCTOR_ENDPOINT,
                "reason": reason,
                "artifact_path": if detailed { path_json(path.as_ref()) } else { Value::Null },
            });
            if detailed {
                summary["lock_path"] = path_json(lock_path.as_ref());
                summary["followup_context"] = json!(followup_context());
            }
            summary
        }
        LatestStartupDoctorArtifact::Missing { path, reason } => json!({
            "available": false,
            "doctor_status": "missing",
            "summary": Value::Null,
            "failed_count": 0,
            "warned_count": 0,
            "detail_endpoint": LATEST_STARTUP_DOCTOR_ENDPOINT,
            "reason": reason,
            // artifact_path only on detailed paths to avoid leaking internal filesystem layout
            "artifact_path": if detailed { path_json(path.as_ref()) } else { Value::Null },
        }),
        LatestStartupDoctorArtifact::Error {
            path,
            error,
            detail: _,
        } => json!({
            "available": false,
            "doctor_status": "error",
            "summary": Value::Null,
            "failed_count": 0,
            "warned_count": 0,
            "detail_endpoint": LATEST_STARTUP_DOCTOR_ENDPOINT,
            "error": error,
            "artifact_path": if detailed { path_json(path.as_ref()) } else { Value::Null },
        }),
    }
}

pub(crate) fn latest_startup_doctor_response_json() -> Value {
    match load_latest_startup_doctor_artifact() {
        LatestStartupDoctorArtifact::Available { path, report } => json!({
            "ok": true,
            "available": true,
            "artifact_path": path.display().to_string(),
            "detail_source": "startup_doctor_artifact",
            "followup_context": followup_context(),
            "summary": report.get("summary").cloned().unwrap_or(Value::Null),
            "artifact": report,
        }),
        LatestStartupDoctorArtifact::Pending {
            path,
            lock_path,
            reason,
        } => json!({
            "ok": true,
            "available": false,
            "artifact_path": path_json(path.as_ref()),
            "lock_path": path_json(lock_path.as_ref()),
            "detail_source": "startup_doctor_artifact",
            "followup_context": followup_context(),
            "reason": reason,
            "artifact": Value::Null,
        }),
        LatestStartupDoctorArtifact::Missing { path, reason } => json!({
            "ok": true,
            "available": false,
            "artifact_path": path_json(path.as_ref()),
            "detail_source": "startup_doctor_artifact",
            "followup_context": followup_context(),
            "reason": reason,
            "artifact": Value::Null,
        }),
        LatestStartupDoctorArtifact::Error {
            path,
            error,
            detail,
        } => json!({
            "ok": false,
            "available": false,
            "artifact_path": path_json(path.as_ref()),
            "detail_source": "startup_doctor_artifact",
            "followup_context": followup_context(),
            "error": error,
            "detail": detail,
            "artifact": Value::Null,
        }),
    }
}

pub(crate) fn run_startup_diagnostic_once() -> Result<Option<PathBuf>, String> {
    write_startup_doctor_artifact(None)
}

/// Record an intentional startup doctor skip as the final artifact for the
/// current boot. This is terminal for the boot because the artifact path is
/// keyed by pid+pidfile mtime and all writers are idempotent.
pub(crate) fn record_startup_diagnostic_skipped(reason: &str) -> Result<Option<PathBuf>, String> {
    write_startup_doctor_artifact(Some(reason))
}

fn write_startup_doctor_artifact(skipped_reason: Option<&str>) -> Result<Option<PathBuf>, String> {
    let artifact_root = startup_artifact_root()
        .ok_or_else(|| "AGENTDESK runtime root is not resolvable".to_string())?;
    fs::create_dir_all(&artifact_root).map_err(|error| {
        format!(
            "create startup doctor dir {}: {error}",
            artifact_root.display()
        )
    })?;
    let boot_id = current_boot_id()?;
    let artifact_path = artifact_root.join(format!("{boot_id}.json"));
    if artifact_path.exists() {
        return Ok(None);
    }

    let in_progress_path = artifact_root.join(format!("{boot_id}.lock"));
    let mut lock = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&in_progress_path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => {
            return Err(format!(
                "preclaim startup doctor lock {}: {error}",
                in_progress_path.display()
            ));
        }
    };
    // Remove lock on all exit paths (early error returns, panics).
    struct LockGuard(std::path::PathBuf);
    impl Drop for LockGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _lock_guard = LockGuard(in_progress_path.clone());

    let started_at = chrono::Local::now().to_rfc3339();
    let _ = writeln!(lock, "started_at={started_at}");
    let payload = if let Some(reason) = skipped_reason {
        let completed_at = chrono::Local::now().to_rfc3339();
        json!({
            "schema_version": 1,
            "version": "doctor/v1",
            "ok": true,
            "boot_id": boot_id,
            "started_at": started_at,
            "completed_at": completed_at,
            "run_context": "startup_once",
            "artifact_path": artifact_path.display().to_string(),
            "profile": Value::Null,
            "fix_requested": false,
            "fix_applied": false,
            "auto_fixes": [],
            "fixes": [],
            "summary": {"passed": 1, "warned": 0, "failed": 0, "total": 1},
            "checks": [{
                "id": "startup_doctor_skipped",
                "group": "server",
                "name": "Startup doctor skipped",
                "status": "pass",
                "severity": "info",
                "subsystem": "startup",
                "ok": true,
                "detail": format!("startup doctor skipped: {reason}"),
                "guidance": Value::Null,
                "path": Value::Null,
                "expected": Value::Null,
                "actual": Value::Null,
                "next_steps": [],
                "evidence": {"reason": reason},
                "fix_safety": "read_only",
                "security_exposure": "none"
            }],
            "skipped": true,
            "skipped_reason": reason,
            "non_fatal": true
        })
    } else {
        let options = super::DoctorOptions {
            fix: true,
            json: true,
            allow_restart: false,
            repair_sqlite_cache: false,
            allow_remote: false,
            profile: None,
            run_context: super::contract::RunContext::StartupOnce,
            artifact_path: Some(artifact_path.clone()),
        };

        let result = super::run_doctor_report(options);
        let completed_at = chrono::Local::now().to_rfc3339();
        match result {
            Ok(report) => serde_json::to_value(report)
                .map(|mut value| {
                    value["schema_version"] = json!(1);
                    value["boot_id"] = json!(boot_id);
                    value["started_at"] = json!(started_at);
                    value["completed_at"] = json!(completed_at);
                    value["non_fatal"] = json!(true);
                    value
                })
                .map_err(|error| format!("serialize startup doctor report: {error}"))?,
            Err(error) => json!({
                "schema_version": 1,
                "boot_id": boot_id,
                "started_at": started_at,
                "completed_at": completed_at,
                "run_context": "startup_once",
                "version": "doctor/v1",
                "ok": false,
                "artifact_path": artifact_path.display().to_string(),
                "profile": Value::Null,
                "fix_requested": true,
                "fix_applied": false,
                "auto_fixes": [],
                "fixes": [],
                "summary": {"passed": 0, "warned": 0, "failed": 1, "total": 1},
                "checks": [],
                "error": error,
                "non_fatal": true
            }),
        }
    };

    let tmp_path = artifact_root.join(format!(
        ".{boot_id}.{}.tmp",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|error| format!("serialize startup doctor artifact: {error}"))?;
    fs::write(&tmp_path, json)
        .map_err(|error| format!("write startup doctor tmp {}: {error}", tmp_path.display()))?;
    fs::rename(&tmp_path, &artifact_path).map_err(|error| {
        format!(
            "commit startup doctor artifact {} -> {}: {error}",
            tmp_path.display(),
            artifact_path.display()
        )
    })?;
    Ok(Some(artifact_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    fn env_lock() -> MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn skipped_startup_doctor_writes_current_boot_artifact() {
        let _env_lock = env_lock();
        let runtime_root = tempfile::tempdir().unwrap();
        let _root_guard = EnvVarGuard::set_path(AGENTDESK_ROOT_DIR_ENV, runtime_root.path());
        let runtime_dir = runtime_root.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("dcserver.pid"), "12345\n").unwrap();

        let artifact_path = record_startup_diagnostic_skipped("no_provider_runtimes_registered")
            .unwrap()
            .expect("first skip should write artifact");
        let content = std::fs::read_to_string(&artifact_path).unwrap();
        let json: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(artifact_path, latest_startup_artifact_path().unwrap());
        assert_eq!(json["skipped"], true);
        assert_eq!(json["skipped_reason"], "no_provider_runtimes_registered");
        assert_eq!(json["summary"]["failed"], 0);
        assert_eq!(json["summary"]["passed"], 1);
        assert_eq!(json["ok"], true);
        assert_eq!(json["version"], "doctor/v1");
        assert_eq!(
            latest_startup_doctor_health_json(true)["doctor_status"],
            "skipped"
        );
        assert_eq!(latest_startup_doctor_health_json(true)["skipped"], true);
        assert!(
            record_startup_diagnostic_skipped("no_provider_runtimes_registered")
                .unwrap()
                .is_none(),
            "second skip for the same boot is idempotent"
        );
    }
}
