use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::services::discord::health;

use super::AppState;

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

    // #209: Collect dispatch_outbox stats for observability
    let outbox_stats = state.db.lock().ok().map(|conn| {
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
        serde_json::json!({
            "pending": pending,
            "retrying": retrying,
            "permanent_failures": failed
        })
    });

    if let Some(ref registry) = state.health_registry {
        let healthy = health::is_healthy(registry).await;
        let discord_json = health::build_health_json(registry).await;
        // Parse the discord JSON and merge with DB status
        let mut json: serde_json::Value =
            serde_json::from_str(&discord_json).unwrap_or(serde_json::json!({}));
        json["db"] = serde_json::json!(db_ok);
        json["dashboard"] = serde_json::json!(dashboard_ok);
        if let Some(stats) = outbox_stats {
            json["dispatch_outbox"] = stats;
        }

        let status = if healthy && db_ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        (status, Json(json)).into_response()
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
            "dashboard": dashboard_ok
        });
        if let Some(stats) = outbox_stats {
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
