use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::AppState;

/// Read a kv_meta value as i64.
fn read_kv_i64(state: &AppState, key: &str) -> i64 {
    state
        .db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .ok()
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Read a kv_meta value as String.
fn read_kv_str(state: &AppState, key: &str) -> String {
    state
        .db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .ok()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build cron job list — 3-tier tick jobs (#127) + legacy per-policy entries.
fn build_cron_jobs(state: &AppState, _agent_filter: Option<&str>) -> Vec<serde_json::Value> {
    let mut jobs = Vec::new();

    // 3-tier tick jobs
    let tiers: &[(&str, &str, i64, &str)] = &[
        (
            "tick:30s",
            "onTick30s — [J] retry, [I-0] notification recovery, [I] deadlock, [K] orphan",
            30_000,
            "30s",
        ),
        (
            "tick:1min",
            "onTick1min — [A][C][D][E][L] non-critical timeouts",
            60_000,
            "1min",
        ),
        (
            "tick:5min",
            "onTick5min — [R][B][F][G][H][M][O] non-critical reconciliation + idle session cleanup",
            300_000,
            "5min",
        ),
    ];

    for &(id, desc, every_ms, label) in tiers {
        let last_ms = read_kv_i64(state, &format!("last_tick_{}_ms", label));
        let status = read_kv_str(state, &format!("last_tick_{}_status", label));
        let next_ms = if last_ms == 0 { 0 } else { last_ms + every_ms };

        jobs.push(json!({
            "id": id,
            "name": desc,
            "enabled": true,
            "schedule": {
                "kind": "every",
                "everyMs": every_ms,
            },
            "state": {
                "status": "active",
                "lastStatus": status,
                "lastRunAtMs": if last_ms == 0 { serde_json::Value::Null } else { json!(last_ms) },
                "nextRunAtMs": if next_ms == 0 { serde_json::Value::Null } else { json!(next_ms) },
            },
        }));
    }

    // Legacy per-policy entries for non-tiered onTick handlers (auto-queue, triage-rules)
    let policies = state.engine.list_policies();
    let legacy_ms = read_kv_i64(state, "last_tick_legacy_ms");
    let legacy_status = read_kv_str(state, "last_tick_legacy_status");

    for p in policies
        .iter()
        .filter(|p| p.hooks.iter().any(|h| h == "onTick") && p.name != "timeouts")
    {
        let next = if legacy_ms == 0 {
            0
        } else {
            legacy_ms + 300_000
        };
        jobs.push(json!({
            "id": format!("policy:{}", p.name),
            "name": format!("policy/{} → onTick (5min legacy)", p.name),
            "enabled": true,
            "schedule": {
                "kind": "every",
                "everyMs": 300_000,
            },
            "state": {
                "status": "active",
                "lastStatus": legacy_status,
                "lastRunAtMs": if legacy_ms == 0 { serde_json::Value::Null } else { json!(legacy_ms) },
                "nextRunAtMs": if next == 0 { serde_json::Value::Null } else { json!(next) },
            },
        }));
    }

    jobs
}

/// GET /api/cron-jobs
pub async fn list_cron_jobs(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let jobs = build_cron_jobs(&state, None);
    (StatusCode::OK, Json(json!({ "jobs": jobs })))
}

/// GET /api/agents/{id}/cron — agent-specific cron jobs
pub async fn agent_cron_jobs(
    State(state): State<AppState>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let jobs = build_cron_jobs(&state, Some(&agent_id));
    (StatusCode::OK, Json(json!({ "jobs": jobs })))
}
