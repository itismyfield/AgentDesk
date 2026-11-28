use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use uuid::Uuid;

/// Read the complete native transcript in order. Unreadable evidence blocks eligibility.
pub(crate) fn pending_schedule(rows: impl BufRead, native_session_id: Uuid) -> bool {
    pending(rows, native_session_id).unwrap_or(true)
}

enum Change {
    Create,
    Delete(String),
}

fn pending(rows: impl BufRead, native_session_id: Uuid) -> Option<bool> {
    let mut calls = HashMap::new();
    let mut jobs = HashMap::new();
    let mut observed_calls = HashSet::new();
    for line in rows.lines() {
        let line = line.ok()?;
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(&line).ok()?;
        if !row.is_object() {
            return None;
        }
        if let Some(id) = row.get("sessionId") {
            let id = Uuid::parse_str(id.as_str()?).ok()?;
            if id != native_session_id {
                continue;
            }
        }
        if row["subtype"] == "scheduled_task_fire" {
            if row["type"] != "system" || row.get("sessionId").is_none() {
                return None;
            }
            let task = row["taskId"].as_str().filter(|id| !id.is_empty())?;
            if jobs.get(task) == Some(&false) {
                jobs.remove(task);
            }
            continue;
        }
        if !matches!(row["type"].as_str(), Some("assistant" | "user")) {
            continue;
        }
        let Some(blocks) = row["message"]["content"].as_array() else {
            continue;
        };
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_use") => {
                    let change = match block["name"].as_str() {
                        Some("CronCreate") => Change::Create,
                        Some("CronDelete") => Change::Delete(block["input"]["id"].as_str()?.into()),
                        _ => continue,
                    };
                    let id = block["id"].as_str()?.to_owned();
                    if observed_calls.insert(id.clone()) {
                        calls.insert(id, change);
                    }
                }
                Some("tool_result") => {
                    let Some(change) = block["tool_use_id"]
                        .as_str()
                        .and_then(|id| calls.remove(id))
                    else {
                        continue;
                    };
                    if block["is_error"] == true {
                        continue;
                    }
                    let result = &row["toolUseResult"];
                    let id = result["id"].as_str().filter(|id| !id.is_empty())?;
                    match change {
                        Change::Create => {
                            let recurring = result["recurring"].as_bool()?;
                            jobs.insert(id.to_owned(), recurring);
                        }
                        Change::Delete(expected) if expected == id && result["deleted"] == true => {
                            jobs.remove(id);
                        }
                        Change::Delete(_) => return None,
                    }
                }
                _ => {}
            }
        }
    }
    Some(!calls.is_empty() || !jobs.is_empty())
}
