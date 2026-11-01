use super::{Shared, invalidate, note};
use crate::services::process_boot::Boot;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

pub(super) fn append_line(file: &mut File, record: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *file, record).map_err(std::io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn read_header(path: &Path, state: &Shared) -> Result<Value, &'static str> {
    let raw = fs::read_to_string(path).map_err(|_| "HISTORY_READ_FAILED")?;
    if !raw.ends_with('\n') {
        return Err("HISTORY_TRUNCATED");
    }
    let mut records = raw.lines();
    let header: Value = serde_json::from_str(records.next().ok_or("HISTORY_HEADER_MISSING")?)
        .map_err(|_| "HISTORY_INVALID_JSON")?;
    if header["kind"] != "header" || header["schema_version"] != 1 {
        return Err("HISTORY_HEADER_MISSING");
    }
    let id = header["boot_id"]
        .as_str()
        .ok_or("HISTORY_BOOT_ID_MISSING")?;
    if uuid::Uuid::parse_str(id).is_err()
        || path.file_stem().and_then(|s| s.to_str()) != Some(id)
        || header["started_at_ms"].as_i64().is_none()
    {
        return Err("HISTORY_HEADER_INVALID");
    }
    for line in records {
        let record: Value = serde_json::from_str(line).map_err(|_| "HISTORY_INVALID_JSON")?;
        if record["kind"] == "observation_invalid" {
            invalidate(state, "HISTORY_INVALIDATED");
        }
    }
    Ok(header)
}

pub(super) fn prepare(
    root: &Path,
    boot: &Boot,
    doctor: Option<String>,
    state: &Shared,
) -> Result<File, &'static str> {
    let dir = root.join("legacy_body_census");
    fs::create_dir_all(&dir).map_err(|_| "JOURNAL_DIRECTORY_FAILED")?;
    let _lock = lock_writer(root).map_err(|_| "JOURNAL_LOCK_FAILED")?;
    let mut headers = std::collections::BTreeMap::new();
    let mut previous: Option<Value> = None;
    for entry in fs::read_dir(&dir).map_err(|_| "HISTORY_READ_FAILED")? {
        let entry = entry.map_err(|_| "HISTORY_READ_FAILED")?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let result = entry
            .file_type()
            .map_err(|_| "HISTORY_READ_FAILED")
            .and_then(|kind| {
                if kind.is_file() {
                    read_header(&entry.path(), state)
                } else {
                    Err("HISTORY_NONREGULAR_FILE")
                }
            });
        match result {
            Ok(header) => {
                let newer = previous.as_ref().is_none_or(|old| {
                    (header["started_at_ms"].as_i64(), header["boot_id"].as_str())
                        > (old["started_at_ms"].as_i64(), old["boot_id"].as_str())
                });
                if newer {
                    previous = Some(header.clone());
                }
                if let Some(id) = header["boot_id"].as_str().map(str::to_owned) {
                    headers.insert(id, header);
                } else {
                    invalidate(state, "HISTORY_HEADER_INVALID");
                }
            }
            Err(reason) => {
                #[cfg(test)]
                if super::tests::mutant("ignore_bad_history") {
                    continue;
                }
                invalidate(state, reason);
            }
        }
    }
    if !valid_chain(&headers) {
        invalidate(state, "HISTORY_CHAIN_INVALID");
    }
    let predecessor = previous.as_ref().and_then(|h| h["boot_id"].as_str());
    if predecessor.is_none() {
        #[cfg(test)]
        let skip = super::tests::mutant("accept_empty_chain");
        #[cfg(not(test))]
        let skip = false;
        if !skip {
            note(state, "PREDECESSOR_MISSING");
        }
    }
    let doctor = doctor.or_else(|| {
        crate::cli::doctor::startup::current_boot_id_at(root)
            .ok()
            .filter(|id| id.split('-').next() == Some(std::process::id().to_string().as_str()))
    });
    if doctor.is_none() {
        invalidate(state, "DOCTOR_BOOT_ID_UNAVAILABLE");
    }
    let probes = super::manifest::snapshot();
    let started = boot.started_at_ms;
    #[cfg(test)]
    let started = if super::tests::mutant("lazy_start") {
        chrono::Utc::now().timestamp_millis()
    } else {
        started
    };
    #[cfg(test)]
    let doctor = if super::tests::mutant("swap_doctor_id") {
        Some(boot.id.clone())
    } else {
        doctor
    };
    let header = json!({
        "kind":"header", "schema_version":1, "process_role":"server",
        "host":crate::services::platform::hostname_short(), "boot_id":boot.id,
        "doctor_boot_id":doctor, "predecessor_boot_id":predecessor,
        "started_at_ms":started,
        "build_sha":crate::services::release_source::health_json(false)["deployed_repo_head"],
        "census_hash":probes["census_hash"], "probe_coverage":probes["coverage"],
        "pending_probes":probes["pending"], "startup_reasons":super::reasons(state),
    });
    let path = dir.join(format!("{}.jsonl", boot.id));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "JOURNAL_CREATE_FAILED")?;
    append_line(&mut file, &header).map_err(|_| "HEADER_WRITE_FAILED")?;
    #[cfg(unix)]
    crate::services::discord::runtime_store::fsync_parent_dir(&path)
        .map_err(|_| "DIRECTORY_SYNC_FAILED")?;
    let mut status = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.header = header;
    status.ready = true;
    Ok(file)
}

pub(super) fn lock_writer(root: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("legacy_body_census/writer.lock"))?;
    file.lock()?;
    Ok(file)
}

fn valid_chain(headers: &std::collections::BTreeMap<String, Value>) -> bool {
    #[cfg(test)]
    if super::tests::mutant("accept_bad_chain") {
        return true;
    }
    for start in headers.keys() {
        let mut visited = std::collections::BTreeSet::new();
        let mut cursor = Some(start.as_str());
        while let Some(id) = cursor {
            if !visited.insert(id) {
                return false;
            }
            let Some(header) = headers.get(id) else {
                return false;
            };
            cursor = match header.get("predecessor_boot_id") {
                Some(Value::Null) => None,
                Some(Value::String(parent)) if uuid::Uuid::parse_str(parent).is_ok() => {
                    Some(parent)
                }
                _ => return false,
            };
        }
    }
    true
}
