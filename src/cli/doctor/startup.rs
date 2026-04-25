use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

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
