use super::{Shared, inherited, invalidate};
use crate::services::process_boot::Boot;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
};

pub(super) fn append_line(file: &mut impl Write, record: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    file.write_all(&bytes)
}

pub(super) fn append_synced(file: &mut File, record: &Value) -> std::io::Result<()> {
    append_line(file, record)?;
    file.sync_data()
}

fn predecessor(dir: &Path, boot: &Boot, host: &Value, state: &Shared) -> Option<Value> {
    let mut identity = None;
    let result = (|| -> Result<Value, &'static str> {
        let head = dir.join("HEAD");
        if !fs::symlink_metadata(&head).is_ok_and(|meta| meta.is_file()) {
            return Err("HEAD_UNAVAILABLE");
        }
        let pointer: Value =
            serde_json::from_slice(&fs::read(head).map_err(|_| "HEAD_UNREADABLE")?)
                .map_err(|_| "HEAD_INVALID")?;
        let id = pointer["boot_id"].as_str().ok_or("HEAD_INVALID")?;
        if uuid::Uuid::parse_str(id).is_err() || id == boot.id {
            return Err("HEAD_INVALID");
        }
        identity = Some(id.to_owned());
        let path = dir.join(format!("{id}.jsonl"));
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
            return Err("HISTORY_NONREGULAR_FILE");
        }
        let mut reader = BufReader::new(File::open(&path).map_err(|_| "HISTORY_READ_FAILED")?);
        let mut first = Vec::new();
        reader
            .read_until(b'\n', &mut first)
            .map_err(|_| "HISTORY_READ_FAILED")?;
        if !first.ends_with(b"\n") {
            return Err("HISTORY_HEADER_MISSING");
        }
        if pointer["header_digest"] != hex::encode(Sha256::digest(&first)) {
            return Err("HISTORY_HEADER_DIGEST_MISMATCH");
        }
        let header: Value = serde_json::from_slice(&first).map_err(|_| "HISTORY_INVALID_JSON")?;
        if header["kind"] != "header"
            || header["schema_version"] != 1
            || header["observation_policy_version"] != 1
            || header["boot_id"] != id
            || header["started_at_ms"].as_i64().is_none()
        {
            return Err("HISTORY_HEADER_INVALID");
        }
        if host.as_str().is_none() || header["host"] != *host || header["process_role"] != "server"
        {
            return Err("HISTORY_HOST_OR_ROLE_MISMATCH");
        }
        let mut window = super::window::Window::new(id.to_owned());
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) if !line.ends_with(b"\n") => {
                    inherited(state, Some(id), "HISTORY_TRUNCATED");
                    window.invalid = true;
                    break;
                }
                Err(_) => {
                    inherited(state, Some(id), "HISTORY_READ_FAILED");
                    window.invalid = true;
                    break;
                }
                Ok(_) => {}
            }
            let record = serde_json::from_slice::<Value>(&line);
            if let Ok(record) = &record {
                window.observe(record);
            } else {
                window.invalid = true;
            }
            match record {
                Ok(record) if record["kind"] == "observation_invalid" => {
                    inherited(state, Some(id), "HISTORY_INVALIDATED")
                }
                Ok(record) if record["kind"].is_string() => {}
                _ => inherited(state, Some(id), "HISTORY_INVALID_JSON"),
            }
        }
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .predecessor_window = window.report(None);
        Ok(header)
    })();
    match result {
        Ok(header) => Some(header),
        Err(cause) => {
            inherited(state, identity.as_deref(), "PREDECESSOR_UNKNOWN");
            inherited(state, identity.as_deref(), cause);
            None
        }
    }
}

pub(super) fn prepare(
    root: &Path,
    boot: &Boot,
    doctor: Option<String>,
    state: &Shared,
) -> Result<File, &'static str> {
    prepare_with_sync(
        root,
        boot,
        doctor,
        state,
        crate::services::discord::runtime_store::fsync_parent_dir,
    )
}

pub(super) fn prepare_with_sync(
    root: &Path,
    boot: &Boot,
    doctor: Option<String>,
    state: &Shared,
    sync_parent: fn(&Path) -> std::io::Result<()>,
) -> Result<File, &'static str> {
    let dir = root.join("legacy_body_census");
    fs::create_dir_all(&dir).map_err(|_| "JOURNAL_DIRECTORY_FAILED")?;
    let _lock = lock_writer(root).map_err(|_| "JOURNAL_LOCK_FAILED")?;
    let host = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .header["host"]
        .clone();
    let previous = predecessor(&dir, boot, &host, state);
    let predecessor = previous
        .as_ref()
        .and_then(|header| header["boot_id"].as_str());
    let doctor = doctor.or_else(|| {
        crate::cli::doctor::startup::current_boot_id_at(root)
            .ok()
            .filter(|id| id.split('-').next() == Some(std::process::id().to_string().as_str()))
    });
    if doctor.is_none() {
        invalidate(state, "DOCTOR_BOOT_ID_UNAVAILABLE");
    }
    let probes = super::manifest::snapshot();
    let status = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let header = json!({
        "kind":"header", "schema_version":1, "observation_policy_version":1,
        "process_role":"server", "host":host, "boot_id":boot.id,
        "doctor_boot_id":doctor, "predecessor_boot_id":predecessor,
        "root_reason":if predecessor.is_none() { json!("PREDECESSOR_UNKNOWN") } else { Value::Null },
        "coverage_start":previous.as_ref().map(|header| &header["coverage_start"]),
        "started_at_ms":boot.started_at_ms,
        "build_sha":crate::services::release_source::health_json(false)["deployed_repo_head"],
        "census_hash":probes["census_hash"], "probe_coverage":probes["coverage"],
        "pending_probes":probes["pending"], "manifest_error":probes["error"],
        "startup_reasons":status.own_reasons, "history_problems":status.history_problems,
    });
    drop(status);
    let path = dir.join(format!("{}.jsonl", boot.id));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "JOURNAL_CREATE_FAILED")?;
    let header_ok = append_synced(&mut file, &header).is_ok();
    if !header_ok {
        invalidate(state, "HEADER_WRITE_FAILED");
    }
    let synced = sync_parent(&path).is_ok();
    if !synced {
        invalidate(state, "DIRECTORY_SYNC_FAILED");
    }
    let pointer = json!({"boot_id":boot.id,"header_digest":hex::encode(Sha256::digest(format!("{header}\n").as_bytes()))});
    let published = header_ok
        && synced
        && crate::services::discord::runtime_store::atomic_write(
            &dir.join("HEAD"),
            &pointer.to_string(),
        )
        .is_ok()
        && sync_parent(&dir.join("HEAD")).is_ok();
    if header_ok && synced && !published {
        invalidate(state, "HEAD_WRITE_FAILED");
    }
    let mut status = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.header = header;
    status.ready = published;
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
