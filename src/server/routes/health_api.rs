use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::services::discord::health;

use super::AppState;

const OUTBOX_AGE_DEGRADED_SECS: i64 = 60;

struct DispatchOutboxStats {
    pending: i64,
    retrying: i64,
    permanent_failures: i64,
    oldest_pending_age: i64,
}

/// GET /api/health — combined DB + Discord provider health.
/// When HealthRegistry is present, returns Discord provider status.
/// Always includes DB status and dashboard availability.
pub async fn health_handler(State(state): State<AppState>) -> Response {
    let db_ok = state
        .db
        .lock()
        .map(|conn| conn.execute_batch("SELECT 1").is_ok())
        .unwrap_or(false);

    // Check if dashboard dist is available
    let dashboard_ok = {
        let dashboard_dir = crate::cli::agentdesk_runtime_root()
            .map(|r| r.join("dashboard/dist"))
            .unwrap_or_else(|| std::path::PathBuf::from("dashboard/dist"));
        dashboard_dir.join("index.html").exists()
    };

    let outbox_stats = load_dispatch_outbox_stats(&state.db);
    let outbox_json = outbox_stats.as_ref().map(|stats| {
        serde_json::json!({
            "pending": stats.pending,
            "retrying": stats.retrying,
            "permanent_failures": stats.permanent_failures,
            "oldest_pending_age": stats.oldest_pending_age,
        })
    });
    let outbox_age = outbox_stats
        .as_ref()
        .map(|stats| stats.oldest_pending_age)
        .unwrap_or(0);

    if let Some(ref registry) = state.health_registry {
        let discord_snapshot = health::build_health_snapshot(registry).await;
        let mut status = discord_snapshot.status();
        let mut json =
            serde_json::to_value(discord_snapshot).unwrap_or_else(|_| serde_json::json!({}));
        let mut degraded_reasons = json["degraded_reasons"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        if !db_ok {
            status = status.worsen(health::HealthStatus::Unhealthy);
            degraded_reasons.push(serde_json::json!("db_unavailable"));
        }
        if outbox_age >= OUTBOX_AGE_DEGRADED_SECS {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!(format!(
                "dispatch_outbox_oldest_pending_age:{}",
                outbox_age
            )));
        }

        json["status"] =
            serde_json::to_value(status).unwrap_or_else(|_| serde_json::json!("unhealthy"));
        json["degraded_reasons"] = serde_json::Value::Array(degraded_reasons);
        json["db"] = serde_json::json!(db_ok);
        json["dashboard"] = serde_json::json!(dashboard_ok);
        json["outbox_age"] = serde_json::json!(outbox_age);
        if let Some(stats) = outbox_json {
            json["dispatch_outbox"] = stats;
        }

        let http_status = if status.is_http_ready() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        (http_status, Json(json)).into_response()
    } else {
        // Standalone mode — no Discord providers
        let status = if db_ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        let mut json = serde_json::json!({
            "ok": db_ok,
            "version": env!("CARGO_PKG_VERSION"),
            "db": db_ok,
            "dashboard": dashboard_ok,
            "deferred_hooks": 0,
            "outbox_age": outbox_age,
            "queue_depth": 0,
            "watcher_count": 0,
            "recovery_duration": 0.0
        });
        if let Some(stats) = outbox_json {
            json["dispatch_outbox"] = stats;
        }
        (status, Json(json)).into_response()
    }
}

/// POST /api/send — agent-to-agent native routing.
pub async fn send_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "Discord not available (standalone mode)"})),
        )
            .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let (status_str, response_body) = health::handle_send(registry, &state.db, &body_str).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/senddm — send a DM to a Discord user.
pub async fn senddm_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "Discord not available (standalone mode)"})),
        )
            .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let (status_str, response_body) = health::handle_senddm(registry, &body_str).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/session/start — start a session via API.
pub async fn session_start_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "Discord not available (standalone mode)"})),
        )
            .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let (status_str, response_body) = health::handle_session_start(registry, &body_str).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

fn parse_status_code(s: &str) -> StatusCode {
    match s {
        "200 OK" => StatusCode::OK,
        "400 Bad Request" => StatusCode::BAD_REQUEST,
        "403 Forbidden" => StatusCode::FORBIDDEN,
        "404 Not Found" => StatusCode::NOT_FOUND,
        "500 Internal Server Error" => StatusCode::INTERNAL_SERVER_ERROR,
        "503 Service Unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn load_dispatch_outbox_stats(db: &crate::db::Db) -> Option<DispatchOutboxStats> {
    db.lock().ok().map(|conn| {
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let retrying: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'pending' AND retry_count > 0",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'failed'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let oldest_pending_age: i64 = conn
            .query_row(
                "SELECT COALESCE(CAST(MAX((julianday('now') - julianday(created_at)) * 86400.0) AS INTEGER), 0) \
                 FROM dispatch_outbox WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        DispatchOutboxStats {
            pending,
            retrying,
            permanent_failures: failed,
            oldest_pending_age,
        }
    })
}
