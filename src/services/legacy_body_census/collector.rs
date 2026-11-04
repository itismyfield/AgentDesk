use super::{evaluation, window::Window};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::Path,
};

const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FILE: u64 = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Collector {
    closed: BTreeMap<String, (u64, String)>,
    known_violation: Option<Value>,
    previous_cut: Option<i64>,
}

fn problem(errors: &mut Vec<String>, reason: &str) {
    if !errors.iter().any(|old| old == reason) {
        errors.push(reason.to_owned());
    }
}

impl Collector {
    pub(super) fn collect(
        &mut self,
        root: &Path,
        current: &str,
        now_ms: i64,
        state: Option<&super::Shared>,
    ) -> Value {
        // Capture the producer watermark before file I/O; a drained prefix can still have queued records.
        let live_journal = state.map(|state| {
            let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            json!({"boot_id":state.window.boot_id,"enqueued_cut":state.window.cut,"invalid":state.window.invalid})
        }).unwrap_or(Value::Null);
        let mut errors = Vec::new();
        let mut truncated = false;
        let mut files = Vec::new();
        let mut budget = MAX_BYTES;
        // Only capture handles and lengths under the writer lock; parsing never holds it.
        let captured = (|| -> std::io::Result<Vec<(String, File, u64)>> {
            let gate = File::open(root.join("legacy_body_census/writer.lock"))?;
            gate.try_lock_shared().map_err(std::io::Error::other)?;
            let mut handles = Vec::new();
            for entry in fs::read_dir(root.join("legacy_body_census"))? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if handles.len() >= 4096 {
                    return Err(std::io::Error::other("inventory limit"));
                }
                let meta = fs::symlink_metadata(&path)?;
                if !meta.is_file() {
                    return Err(std::io::Error::other("nonregular journal"));
                }
                handles.push((
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    File::open(path)?,
                    meta.len(),
                ));
            }
            handles.sort_by_key(|(id, _, _)| id != current);
            Ok(handles)
        })();
        let mut epoch_start = None;
        match captured {
            Err(_) => problem(&mut errors, "JOURNAL_INVENTORY_UNAVAILABLE"),
            Ok(handles) => {
                for (id, file, cut) in handles {
                    let length = cut.min(MAX_FILE).min(budget);
                    budget -= length;
                    if length != cut {
                        truncated = true;
                    }
                    let mut bytes = Vec::new();
                    if file.take(length).read_to_end(&mut bytes).is_err()
                        || bytes.len() as u64 != length
                    {
                        problem(&mut errors, "JOURNAL_READ_FAILED");
                    }
                    let digest = hex::encode(Sha256::digest(&bytes));
                    let mut lines = bytes.split(|byte| *byte == b'\n').peekable();
                    let header = lines
                        .next()
                        .and_then(|line| serde_json::from_slice::<Value>(line).ok())
                        .unwrap_or(Value::Null);
                    if bytes.last() != Some(&b'\n') {
                        problem(&mut errors, "JOURNAL_TRUNCATED");
                    }
                    if header["kind"] != "header"
                        || header["boot_id"] != id
                        || uuid::Uuid::parse_str(&id).is_err()
                    {
                        problem(&mut errors, "JOURNAL_HEADER_INVALID");
                    }
                    if id == current {
                        epoch_start = header["coverage_start"]["started_at_ms"].as_i64();
                    }
                    let mut window = Window::new(id.clone());
                    let (mut targets, mut buckets, mut selection, mut end) =
                        (Vec::new(), Vec::new(), Value::Null, Value::Null);
                    while let Some(line) = lines.next() {
                        if line.is_empty() && lines.peek().is_none() {
                            break;
                        }
                        let Ok(record) = serde_json::from_slice::<Value>(line) else {
                            problem(&mut errors, "JOURNAL_INVALID_JSON");
                            window.invalid = true;
                            continue;
                        };
                        if end.is_object() && record["kind"] != "drain" {
                            window.invalid = true;
                        }
                        window.observe(&record);
                        match record["kind"].as_str() {
                            Some("target") => {
                                if evaluation::is_violation(&record) {
                                    let start = epoch_start;
                                    if start.is_none()
                                        || record["at_ms"]
                                            .as_i64()
                                            .is_none_or(|at| at >= start.unwrap_or(i64::MIN))
                                    {
                                        let earlier =
                                            self.known_violation.as_ref().is_none_or(|old| {
                                                record["at_ms"].as_i64().unwrap_or(i64::MIN)
                                                    < old["at_ms"].as_i64().unwrap_or(i64::MIN)
                                            });
                                        if earlier {
                                            self.known_violation = Some(record.clone());
                                        }
                                    }
                                }
                                if targets.len() < 2048 {
                                    targets.push(record);
                                } else {
                                    truncated = true;
                                }
                            }
                            Some("selection") => {
                                if !selection.is_null() && selection != record {
                                    problem(&mut errors, "SELECTION_HISTORY_CONFLICT");
                                }
                                selection = record;
                            }
                            Some("denominator") => buckets.push(record),
                            Some("end") => end = record,
                            Some("drain" | "boot_ready") => {}
                            Some("observation_invalid" | "invalid") => {
                                problem(&mut errors, "OBSERVATION_INVALID")
                            }
                            _ => problem(&mut errors, "RECORD_SCHEMA_UNKNOWN"),
                        }
                    }
                    if let Some(prior) = self.closed.get(&id)
                        && *prior != (cut, digest.clone())
                    {
                        problem(&mut errors, "CLOSED_JOURNAL_CHANGED");
                    }
                    if window.closed {
                        self.closed
                            .entry(id.clone())
                            .or_insert((cut, digest.clone()));
                    }
                    files.push(json!({"header":header,"cut_bytes":cut,"digest":digest,
                    "window":window.report(Some(current)),"selection":selection,"targets":targets,
                    "buckets":buckets,"end":end}));
                }
            }
        }
        for id in self.closed.keys() {
            if !files.iter().any(|file| file["header"]["boot_id"] == *id) {
                problem(&mut errors, "CLOSED_JOURNAL_MISSING");
            }
        }
        let header = files
            .iter()
            .find(|file| file["header"]["boot_id"] == current)
            .map(|file| file["header"].clone())
            .unwrap_or(Value::Null);
        let anchor = header["coverage_start"].clone();
        let mut starts = Vec::new();
        let inventory = (|| -> Option<()> {
            let directory = root.join("runtime/doctor/startup");
            for (index, entry) in fs::read_dir(directory).ok()?.enumerate() {
                if index >= 4096 {
                    return None;
                }
                let path = entry.ok()?.path();
                if path.extension().and_then(|s| s.to_str()) == Some("lock") {
                    return None;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let meta = fs::symlink_metadata(&path).ok()?;
                if !meta.is_file() || meta.len() > MAX_FILE {
                    return None;
                }
                let data: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
                let id = data["boot_id"].as_str()?;
                let at = chrono::DateTime::parse_from_rfc3339(data["started_at"].as_str()?)
                    .ok()?
                    .timestamp_millis();
                if data["schema_version"] != 1
                    || path.file_stem()?.to_str()? != id
                    || data["run_context"] != "startup_once"
                {
                    return None;
                }
                starts.push(json!({"doctor_boot_id":id,"started_at_ms":at}));
            }
            Some(())
        })();
        if inventory.is_none() || starts.is_empty() {
            problem(&mut errors, "START_INVENTORY_UNAVAILABLE");
        }
        if self.previous_cut.is_some_and(|old| now_ms < old) {
            problem(&mut errors, "CLOCK_REVERSED");
        }
        self.previous_cut = Some(now_ms);
        let mut value = json!({"schema_version":1,"policy_version":1,"live_journal":live_journal,"host":header["host"],
            "process_role":"server","current_boot_id":current,"build_sha":header["build_sha"],
            "census_hash":header["census_hash"],"probe_coverage":header["probe_coverage"],"pending_probes":header["pending_probes"],
            "static_instrumentation":header["static_instrumentation"],
            "started_at_ms":header["started_at_ms"],"coverage_start":anchor,"cut_ms":now_ms,"stale_snapshot":false,
            "read_error":errors.iter().any(|reason| reason.starts_with("JOURNAL_") || reason == "START_INVENTORY_UNAVAILABLE"),
            "inventory_source":{"source":"doctor/startup","version":1,"from_ms":anchor["started_at_ms"],"to_ms":now_ms,"available":inventory.is_some() && !starts.is_empty()},
            "doctor_starts":starts,"boot_files":files,"errors":errors,"evidence_truncated":truncated,
            "known_violation":self.known_violation});
        if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > 1024 * 1024) {
            value["evidence_truncated"] = json!(true);
            value["boot_files"] = json!([]);
        }
        evaluate_host(&mut value, now_ms);
        value
    }
}

fn evaluate_host(value: &mut Value, now: i64) {
    let report = evaluation::host(
        value,
        now,
        value["build_sha"].as_str().unwrap_or(""),
        value["census_hash"].as_str().unwrap_or(""),
    );
    value["local_evaluation"] = report;
}

#[cfg(test)]
impl Collector {
    pub(crate) fn fixture(&mut self, root: &Path, current: &str, now: i64) -> Value {
        let mut value = self.collect(root, current, now, None);
        let cut = value["boot_files"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|file| file["header"]["boot_id"] == current)
            .map(|file| file["window"]["cut"].clone())
            .unwrap_or(Value::Null);
        value["live_journal"] = json!({"boot_id":current,"enqueued_cut":cut,"invalid":false});
        evaluate_host(&mut value, now);
        value
    }
}
