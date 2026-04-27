use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;
use sqlx::Row;

use super::AppState;

/// Read a kv_meta value as i64.
async fn read_kv_i64(state: &AppState, key: &str) -> i64 {
    read_kv_str(state, key).await.parse().ok().unwrap_or(0)
}

/// Read a kv_meta value as String.
async fn read_kv_str(state: &AppState, key: &str) -> String {
    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query("SELECT value FROM kv_meta WHERE key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(row)) => {
                if let Ok(value) = row.try_get::<Option<String>, _>("value") {
                    return value.unwrap_or_else(|| "unknown".to_string());
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    "[cron_api] read kv_meta {} from postgres failed: {}",
                    key,
                    error
                );
            }
        }
    }

    // TODO(#1238 / 843g): the runtime is PG-only and the kv_meta SQLite
    // fallback is never reached. Tests still hit this branch through
    // `AppState::test_state` which sets `db: Some(test_db)`.
    state
        .db
        .as_ref()
        .and_then(|db| db.lock().ok())
        .and_then(|conn| {
            conn.query_row::<String, _, _>(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build cron job list — 3-tier tick jobs (#127) + legacy per-policy entries.
async fn build_cron_jobs(state: &AppState, _agent_filter: Option<&str>) -> Vec<serde_json::Value> {
    let mut jobs = Vec::new();

    for descriptor in crate::server::cron_catalog::tier_descriptors() {
        let last_ms = read_kv_i64(state, &format!("last_tick_{}_ms", descriptor.kv_label)).await;
        let status = read_kv_str(state, &format!("last_tick_{}_status", descriptor.kv_label)).await;
        let next_ms = if last_ms == 0 {
            0
        } else {
            last_ms + descriptor.every_ms
        };

        jobs.push(json!({
            "id": descriptor.job_id,
            "name": descriptor.name,
            "source": "cron",
            "enabled": true,
            "schedule": {
                "kind": "every",
                "everyMs": descriptor.every_ms,
            },
            "state": {
                "status": "active",
                "lastStatus": status,
                "lastRunAtMs": if last_ms == 0 { serde_json::Value::Null } else { json!(last_ms) },
                "nextRunAtMs": if next_ms == 0 { serde_json::Value::Null } else { json!(next_ms) },
            },
        }));
    }

    if state.pg_pool.is_some() {
        if let Some(descriptor) = crate::server::cron_catalog::github_issue_sync_descriptor(
            state.config.github.sync_interval_minutes,
        ) {
            let last_ms =
                read_kv_i64(state, &format!("last_tick_{}_ms", descriptor.kv_label)).await;
            let status =
                read_kv_str(state, &format!("last_tick_{}_status", descriptor.kv_label)).await;
            let next_ms = if last_ms == 0 {
                0
            } else {
                last_ms + descriptor.every_ms
            };

            jobs.push(json!({
                "id": descriptor.job_id,
                "name": descriptor.name,
                "source": "cron",
                "enabled": true,
                "schedule": {
                    "kind": "every",
                    "everyMs": descriptor.every_ms,
                },
                "state": {
                    "status": "active",
                    "lastStatus": status,
                    "lastRunAtMs": if last_ms == 0 { serde_json::Value::Null } else { json!(last_ms) },
                    "nextRunAtMs": if next_ms == 0 { serde_json::Value::Null } else { json!(next_ms) },
                },
            }));
        }
    }

    for descriptor in crate::server::cron_catalog::legacy_policy_descriptors(&state.engine) {
        let legacy_ms = read_kv_i64(state, "last_tick_legacy_ms").await;
        let legacy_status = read_kv_str(state, "last_tick_legacy_status").await;
        let next = if legacy_ms == 0 {
            0
        } else {
            legacy_ms + descriptor.every_ms
        };

        jobs.push(json!({
            "id": descriptor.job_id,
            "name": descriptor.name,
            "source": "cron",
            "enabled": true,
            "schedule": {
                "kind": "every",
                "everyMs": descriptor.every_ms,
            },
            "state": {
                "status": "active",
                "lastStatus": legacy_status,
                "lastRunAtMs": if legacy_ms == 0 { serde_json::Value::Null } else { json!(legacy_ms) },
                "nextRunAtMs": if next == 0 { serde_json::Value::Null } else { json!(next) },
            },
        }));
    }

    // #1091: surface dynamically-registered maintenance jobs alongside cron.
    for info in crate::services::maintenance::list_maintenance_jobs() {
        match serde_json::to_value(info) {
            Ok(value) => jobs.push(value),
            Err(error) => {
                tracing::warn!("[cron_api] failed to encode services::maintenance job: {error}");
            }
        }
    }

    // #1237 (843f): runtime is PG-only, so `state.db` is None at runtime and
    // the SQLite branch is reachable only from tests (which do supply a Db).
    // #1238 (843g) will drop the SQLite branch entirely and require PG.
    let maintenance_jobs = match state.pg_pool_ref() {
        Some(pool) => crate::server::maintenance::list_job_statuses_pg(pool.clone()).await,
        None => match state.db.clone() {
            Some(db) => crate::server::maintenance::list_job_statuses_sqlite(db).await,
            None => Vec::new(),
        },
    };
    for job in maintenance_jobs {
        match serde_json::to_value(job) {
            Ok(mut value) => {
                // #1091: mark legacy server::maintenance entries with
                // source = "maintenance" so /api/cron-jobs carries a
                // consistent tag for every non-cron job.
                if let Some(object) = value.as_object_mut() {
                    object.insert("source".to_string(), json!("maintenance"));
                }
                jobs.push(value);
            }
            Err(error) => {
                tracing::warn!("[cron_api] failed to encode maintenance job status: {error}");
            }
        }
    }

    jobs
}

/// GET /api/cron-jobs
pub async fn list_cron_jobs(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let jobs = build_cron_jobs(&state, None).await;
    (StatusCode::OK, Json(json!({ "jobs": jobs })))
}

/// GET /api/agents/{id}/cron — agent-specific cron jobs
pub async fn agent_cron_jobs(
    State(state): State<AppState>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let jobs = build_cron_jobs(&state, Some(&agent_id)).await;
    (StatusCode::OK, Json(json!({ "jobs": jobs })))
}
