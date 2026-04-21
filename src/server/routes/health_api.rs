use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sqlx::PgPool;
use std::net::SocketAddr;

use crate::services::discord::health;

use super::AppState;

const OUTBOX_AGE_DEGRADED_SECS: i64 = 60;

struct DispatchOutboxStats {
    pending: i64,
    retrying: i64,
    permanent_failures: i64,
    oldest_pending_age: i64,
}

fn discord_control_endpoints_allowed(
    config: &crate::config::Config,
    peer_addr: Option<SocketAddr>,
) -> bool {
    if config
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|token| !token.is_empty())
    {
        return true;
    }

    if peer_addr.is_some_and(|addr| addr.ip().is_loopback()) {
        return true;
    }

    matches!(config.server.host.trim(), "127.0.0.1" | "localhost" | "::1")
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

    let outbox_stats = load_dispatch_outbox_stats(&state.db, state.pg_pool.as_ref()).await;
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
    let config_audit_report =
        crate::services::discord::config_audit::load_persisted_report(&state.db)
            .and_then(|report| serde_json::to_value(report).ok());
    let pipeline_override_report =
        crate::pipeline::load_persisted_override_health_report(&state.db)
            .and_then(|report| serde_json::to_value(report).ok());

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
        let pipeline_override_warnings = pipeline_override_report
            .as_ref()
            .and_then(|value| value["warnings_count"].as_u64())
            .unwrap_or(0);
        if pipeline_override_warnings > 0 {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!(format!(
                "pipeline_override_warnings:{}",
                pipeline_override_warnings
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
        if let Some(report) = config_audit_report.clone() {
            json["config_audit"] = report;
        }
        if let Some(report) = pipeline_override_report.clone() {
            json["pipeline_overrides"] = report;
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
        if let Some(report) = config_audit_report {
            json["config_audit"] = report;
        }
        if let Some(report) = pipeline_override_report {
            json["pipeline_overrides"] = report;
        }
        (status, Json(json)).into_response()
    }
}

/// POST /api/send — agent-to-agent native routing.
///
/// Requires `ConnectInfo<SocketAddr>` injected by the server bootstrap
/// (see `boot.rs::run_with_state` and `mod.rs::launch_*` which both call
/// `into_make_service_with_connect_info::<SocketAddr>`). The
/// `discord_control_endpoints_allowed` helper supports `peer_addr: None`
/// for internal callers / unit tests where the connection info isn't
/// available; in production HTTP traffic the extractor is always present.
pub async fn send_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }

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

/// POST /api/inflight/rebind — #896 orphan recovery endpoint.
///
/// Rebinds a live tmux session to a freshly-created inflight state and
/// respawns the output watcher. Intended for operators recovering from
/// situations where the tmux session is alive (agent is actively working)
/// but the inflight JSON was cleared by a prior turn's cleanup, leaving
/// subsequent output with no Discord relay path.
///
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
pub async fn rebind_inflight_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "Discord not available (standalone mode)"})),
        )
            .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let (status_str, response_body) = health::handle_rebind_inflight(registry, &body_str).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/send_to_agent — role_id-based agent routing.
///
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
pub async fn send_to_agent_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "Discord not available (standalone mode)"})),
        )
            .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let (status_str, response_body) =
        health::handle_send_to_agent(registry, &state.db, &body_str).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/senddm — send a DM to a Discord user.
///
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
pub async fn senddm_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }

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

#[cfg(test)]
mod tests {
    use super::discord_control_endpoints_allowed;

    #[test]
    fn discord_control_endpoints_allow_loopback_peer_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some(String::new());

        assert!(discord_control_endpoints_allowed(
            &config,
            Some("127.0.0.1:8791".parse().unwrap())
        ));
        assert!(discord_control_endpoints_allowed(
            &config,
            Some("[::1]:8791".parse().unwrap())
        ));
    }

    #[test]
    fn discord_control_endpoints_reject_non_loopback_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some(String::new());

        assert!(!discord_control_endpoints_allowed(
            &config,
            Some("10.0.0.5:8791".parse().unwrap())
        ));
        assert!(!discord_control_endpoints_allowed(&config, None));
    }
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

async fn load_dispatch_outbox_stats(
    db: &crate::db::Db,
    pg_pool: Option<&PgPool>,
) -> Option<DispatchOutboxStats> {
    if let Some(pool) = pg_pool {
        if let Some(stats) = load_dispatch_outbox_stats_pg(pool).await {
            return Some(stats);
        }
        tracing::warn!(
            "[health] failed to load dispatch_outbox stats from PostgreSQL; falling back to SQLite"
        );
    }

    load_dispatch_outbox_stats_sqlite(db)
}

async fn load_dispatch_outbox_stats_pg(pool: &PgPool) -> Option<DispatchOutboxStats> {
    let pending = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending'",
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let retrying = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending' AND retry_count > 0",
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let failed = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'failed'",
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let oldest_pending_age = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(CAST(EXTRACT(EPOCH FROM (NOW() - MIN(created_at))) AS BIGINT), 0)
         FROM dispatch_outbox
         WHERE status = 'pending'",
    )
    .fetch_one(pool)
    .await
    .ok()?;

    Some(DispatchOutboxStats {
        pending,
        retrying,
        permanent_failures: failed,
        oldest_pending_age,
    })
}

fn load_dispatch_outbox_stats_sqlite(db: &crate::db::Db) -> Option<DispatchOutboxStats> {
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
