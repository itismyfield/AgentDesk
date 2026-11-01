//! Owns the policy maintenance contract; automatic and keyed sweeps keep their own gates.
use std::collections::HashMap;

use serde_json::{Value, json};

const WORKING_GRACE_MINUTES: i32 = 10;
const DEADLOCK_SCAN_MINUTES: i32 = 30;
const DEADLOCK_SCAN_LIMIT: i32 = 50;
const HISTORY_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const FAIL_REASON: &str = "Stale working session recovery — no active tmux session after 10min";

// These are the existing typed DB/host/file operations. Only this owner chooses
// their thresholds, candidate order, observations and repair arguments.
pub(crate) trait PolicyMaintenanceIo {
    fn clear_fresh(&mut self, minutes: i32) -> Result<(), String>;
    fn working(&mut self, minutes: i32) -> Result<Vec<Value>, String>;
    fn deadlocks(&mut self, minutes: i32, limit: i32) -> Result<Vec<Value>, String>;
    fn observe(&mut self, key: &str) -> Result<Value, String>;
    fn inflights(&mut self) -> Result<Vec<Value>, String>;
    fn repair(&mut self, key: &str, request: Value) -> Result<Value, String>;
    fn delete_counter(&mut self, key: &str) -> Result<(), String>;
    fn cleanup_inactive(&mut self) -> Result<(), String>;
    fn cleanup_history(&mut self, cutoff_ms: i64) -> Result<(), String>;
    fn log(&mut self, level: &str, message: String) -> Result<(), String>;
}

fn host(
    io: &mut impl PolicyMaintenanceIo,
    hosts: &mut HashMap<Option<String>, Value>,
    key: Option<&str>,
) -> Result<Value, String> {
    let cache_key = key.map(str::to_owned);
    if let Some(host) = hosts.get(&cache_key) {
        return Ok(host.clone());
    }
    let host = match key.filter(|key| !key.trim().is_empty()) {
        None => json!({"state": "unknown", "reason": "session_missing"}),
        Some(key) => match io.observe(key) {
            Ok(host) if matches!(host["state"].as_str(), Some("live" | "dead")) => host,
            Ok(host) => {
                json!({"state": "unknown", "reason": host["reason"].as_str().filter(|reason| !reason.is_empty()).unwrap_or("unknown")})
            }
            Err(error) => json!({"state": "unknown", "reason": format!("error: {error}")}),
        },
    };
    if host["state"] == "unknown" {
        io.log(
            "warn",
            format!(
                "[deadlock] Session host unknown ({}); deferring {}",
                host["reason"].as_str().unwrap_or("unknown"),
                key.unwrap_or("null")
            ),
        )?;
    }
    hosts.insert(cache_key, host.clone());
    Ok(host)
}

fn zero(value: &Value) -> bool {
    value == &json!(0) || value == &json!("0")
}

// The policy only consumed existence, never the newest matching row's timestamp.
// Keep the old full-key OR tmux matching and synthetic reattach exclusion.
fn has_inflight(rows: &[Value], key: &str, tmux: Option<&str>) -> bool {
    rows.iter().any(|row| {
        let placeholder = row["rebind_origin"] == true
            && row["session_id"].is_null()
            && zero(&row["request_owner_user_id"])
            && zero(&row["user_msg_id"])
            && row["any_tool_used"] == false
            && row["has_post_tool_text"] == false;
        !placeholder
            && ((!key.is_empty() && row["session_key"].as_str() == Some(key))
                || tmux
                    .filter(|name| !name.is_empty())
                    .is_some_and(|name| row["tmux_session_name"].as_str() == Some(name)))
    })
}

fn repair(
    io: &mut impl PolicyMaintenanceIo,
    key: &str,
    host: &Value,
    row: &Value,
    working: bool,
    fail_dispatch: bool,
) -> Result<Value, String> {
    let dispatch = row["active_dispatch_id"]
        .as_str()
        .filter(|id| !id.is_empty());
    let result = io.repair(
        key,
        json!({
            "session_id": host["session_id"],
            "active_dispatch_id": dispatch,
            "active_turn_nonce": row["active_turn_nonce"],
            "observed": host["state"],
            "fail_dispatch": fail_dispatch,
            "fail_reason": if working { FAIL_REASON } else { "" },
            "clear_active_dispatch_id": working,
        }),
    )?;
    if result["repaired"] != true {
        io.log(
            "warn",
            format!(
                "[deadlock] Repair deferred ({}) for {key}",
                result["deferred"].as_str().unwrap_or("undefined")
            ),
        )?;
    }
    Ok(result)
}

pub(crate) fn maintain_policy_sessions(io: &mut impl PolicyMaintenanceIo) -> Result<(), String> {
    let mut hosts = HashMap::new();
    io.clear_fresh(DEADLOCK_SCAN_MINUTES)?;
    for row in io.working(WORKING_GRACE_MINUTES)? {
        let key = row["session_key"].as_str();
        let observed = host(io, &mut hosts, key)?;
        if observed["state"] == "unknown" {
            continue;
        }
        let key = key.unwrap_or_default();
        let inflight = match io.inflights() {
            Ok(rows) => has_inflight(&rows, key, observed["tmux_name"].as_str()),
            Err(error) => {
                io.log(
                    "warn",
                    format!("[deadlock] Transient error looking up inflight for {key}: {error}"),
                )?;
                continue;
            }
        };
        if observed["state"] == "live" && inflight {
            continue;
        }
        let dispatch = row["active_dispatch_id"]
            .as_str()
            .filter(|id| !id.is_empty());
        let fail = dispatch.is_some()
            && matches!(
                row["active_dispatch_status"].as_str(),
                Some("pending" | "dispatched")
            );
        let result = repair(io, key, &observed, &row, true, fail)?;
        if result["repaired"] != true {
            continue;
        }
        if let Some(error) = result["dispatch_error"]
            .as_str()
            .filter(|error| !error.is_empty())
        {
            io.log(
                "warn",
                format!("[deadlock] Failed to mark dispatch for {key}: {error}"),
            )?;
        } else if let Some(dispatch) = dispatch.filter(|_| fail) {
            if result["dispatch_rows_affected"] == 0 {
                io.log("warn", format!("[dispatch.markFailed] no rows affected for {dispatch} — already terminal or missing"))?;
            }
            io.log(
                "warn",
                format!("[deadlock] Failed stale dispatch {dispatch} for session {key}"),
            )?;
        }
        io.log(
            "info",
            format!("[deadlock] Fixed stale working session → idle: {key}"),
        )?;
    }
    for row in io.deadlocks(DEADLOCK_SCAN_MINUTES, DEADLOCK_SCAN_LIMIT)? {
        let key = row["session_key"].as_str();
        let observed = host(io, &mut hosts, key)?;
        if observed["state"] == "unknown" {
            continue;
        }
        let key = key.unwrap_or_default();
        let inflight = match io.inflights() {
            Ok(rows) => has_inflight(&rows, key, observed["tmux_name"].as_str()),
            Err(_) => continue,
        };
        io.delete_counter(&format!("deadlock_check:{key}"))?;
        if observed["state"] == "live" && inflight {
            continue;
        }
        if repair(io, key, &observed, &row, false, false)?["repaired"] == true {
            io.log(
                "info",
                format!("[deadlock] Stale working session → idle (no active turn): {key}"),
            )?;
        }
    }
    io.cleanup_inactive()?;
    io.cleanup_history(chrono::Utc::now().timestamp_millis() - HISTORY_RETENTION_MS)?;
    Ok(())
}
