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

enum LatestStartupDoctorArtifact {
    Available {
        path: PathBuf,
        report: Value,
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
    // Use "doctor_status" (not "status") to avoid conflicting with the top-level
    // "status" field in /api/health when the no-jq regex fallback takes the first match.
    let mut summary = json!({
        "available": true,
        "doctor_status": startup_doctor_status(failed_count, warned_count),
        "started_at": report.get("started_at").cloned().unwrap_or(Value::Null),
        "completed_at": report.get("completed_at").cloned().unwrap_or(Value::Null),
        "summary": report.get("summary").cloned().unwrap_or(Value::Null),
        "failed_count": failed_count,
        "warned_count": warned_count,
        "detail_endpoint": LATEST_STARTUP_DOCTOR_ENDPOINT,
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

/// #2049 Finding 4: Surface failed/warned counts from the latest startup
/// doctor artifact so `/api/health` can worsen `status` + `degraded_reasons`
/// when boot-time checks regressed. Returns `(failed_count, warned_count)`;
/// missing/error artifacts return `(0, 0)` so the doctor signal cannot mask
/// other degradation signals via false positives.
pub(crate) fn latest_startup_doctor_counts() -> (u64, u64) {
    match load_latest_startup_doctor_artifact() {
        LatestStartupDoctorArtifact::Available { report, .. } => {
            let failed = summary_count(&report, "failed", "fail");
            let warned = summary_count(&report, "warned", "warn");
            (failed, warned)
        }
        LatestStartupDoctorArtifact::Missing { .. } | LatestStartupDoctorArtifact::Error { .. } => {
            (0, 0)
        }
    }
}

pub(crate) fn latest_startup_doctor_health_json(detailed: bool) -> Value {
    match load_latest_startup_doctor_artifact() {
        LatestStartupDoctorArtifact::Available { path, report } => {
            startup_doctor_summary_json(&path, &report, detailed)
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
    let payload = match result {
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
            "fix_applied": false,
            "auto_fixes": [],
            "summary": {"passed": 0, "warned": 0, "failed": 1, "total": 1},
            "checks": [],
            "error": error,
            "non_fatal": true
        }),
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
