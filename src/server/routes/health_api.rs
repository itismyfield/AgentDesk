use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::net::SocketAddr;

use crate::services::discord::health;
use crate::services::disk_monitor;
use crate::services::provider::ProviderKind;

use super::AppState;

/// TODO(#1238 / 843g): see agents::agent_quality_legacy_db.
fn health_legacy_db(state: &AppState) -> &crate::db::Db {
    use std::sync::OnceLock;
    static PLACEHOLDER: OnceLock<crate::db::Db> = OnceLock::new();
    state
        .engine
        .legacy_db()
        .or_else(|| state.legacy_db())
        .unwrap_or_else(|| PLACEHOLDER.get_or_init(super::pending_migration_shim_for_callers))
}

const OUTBOX_AGE_DEGRADED_SECS: i64 = 60;

struct DispatchOutboxStats {
    pending: i64,
    retrying: i64,
    permanent_failures: i64,
    oldest_pending_age: i64,
}

#[derive(Clone, Debug, Serialize)]
struct ChannelSessionState {
    agent_id: Option<String>,
    provider: Option<String>,
    status: Option<String>,
    active_dispatch_id: Option<String>,
    thread_channel_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StaleMailboxRepairRequest {
    channel_id: u64,
    expected_has_cancel_token: Option<bool>,
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

/// Build combined DB + Discord provider health.
/// Public callers receive only a redacted safe summary; authenticated/local
/// detail callers receive provider/config/outbox diagnostics.
async fn health_response(state: &AppState, detailed: bool) -> Response {
    let server_up = probe_server_up(state.pg_pool_ref()).await;

    // Check if dashboard dist is available
    let dashboard_ok = {
        let dashboard_dir = crate::cli::agentdesk_runtime_root()
            .map(|r| r.join("dashboard/dist"))
            .unwrap_or_else(|| std::path::PathBuf::from("dashboard/dist"));
        dashboard_dir.join("index.html").exists()
    };

    // #1203: surface free disk on the runtime partition. ENOSPC silently
    // breaks inflight state writes and tool buffers; a numeric signal lets
    // the dashboard / `agentdesk doctor` warn before we hit the cliff.
    let disk_probe_path =
        crate::cli::agentdesk_runtime_root().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let disk_snapshot = disk_monitor::probe(&disk_probe_path);

    let outbox_stats = load_dispatch_outbox_stats(state.pg_pool_ref()).await;
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
    let config_audit_report = load_config_audit_report_pg(state.pg_pool_ref()).await;
    let pipeline_override_report = load_pipeline_override_report_pg(state.pg_pool_ref()).await;

    if let Some(ref registry) = state.health_registry {
        let discord_snapshot = if detailed {
            health::build_health_snapshot(registry).await
        } else {
            health::build_public_health_snapshot(registry).await
        };
        let mut status = discord_snapshot.status();
        let mut json =
            serde_json::to_value(discord_snapshot).unwrap_or_else(|_| serde_json::json!({}));
        if detailed {
            enrich_mailbox_session_state(&mut json, state).await;
        }
        let mut degraded_reasons = json["degraded_reasons"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        if !server_up {
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
        if let Some(snapshot) = disk_snapshot
            && snapshot.is_low()
        {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!(format!(
                "disk_low_free_bytes:{}",
                snapshot.free_bytes
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
        json["db"] = serde_json::json!(server_up);
        json["dashboard"] = serde_json::json!(dashboard_ok);
        json["server_up"] = serde_json::json!(server_up);
        json["outbox_age"] = serde_json::json!(outbox_age);
        if let Some(snapshot) = disk_snapshot {
            json["disk_free_bytes"] = serde_json::json!(snapshot.free_bytes);
            json["disk_total_bytes"] = serde_json::json!(snapshot.total_bytes);
            json["disk_used_pct"] = serde_json::json!(snapshot.used_pct());
            json["disk_low"] = serde_json::json!(snapshot.is_low());
        }
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
        let json = if detailed {
            with_latest_startup_doctor(json, true)
        } else {
            with_latest_startup_doctor(public_health_json(json), false)
        };
        (http_status, Json(json)).into_response()
    } else {
        // Standalone mode — no Discord providers
        let status = if server_up {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        let health_status = if server_up { "healthy" } else { "unhealthy" };
        let mut json = serde_json::json!({
            "status": health_status,
            "ok": server_up,
            "version": env!("CARGO_PKG_VERSION"),
            "db": server_up,
            "dashboard": dashboard_ok,
            "server_up": server_up,
            "fully_recovered": server_up,
            "deferred_hooks": 0,
            "outbox_age": outbox_age,
            "queue_depth": 0,
            "watcher_count": 0,
            "recovery_duration": 0.0
        });
        if let Some(snapshot) = disk_snapshot {
            json["disk_free_bytes"] = serde_json::json!(snapshot.free_bytes);
            json["disk_total_bytes"] = serde_json::json!(snapshot.total_bytes);
            json["disk_used_pct"] = serde_json::json!(snapshot.used_pct());
            json["disk_low"] = serde_json::json!(snapshot.is_low());
        }
        if let Some(stats) = outbox_json {
            json["dispatch_outbox"] = stats;
        }
        if let Some(report) = config_audit_report {
            json["config_audit"] = report;
        }
        if let Some(report) = pipeline_override_report {
            json["pipeline_overrides"] = report;
        }
        let json = if detailed {
            with_latest_startup_doctor(json, true)
        } else {
            with_latest_startup_doctor(public_health_json(json), false)
        };
        (status, Json(json)).into_response()
    }
}

fn with_latest_startup_doctor(mut json: serde_json::Value, detailed: bool) -> serde_json::Value {
    json["latest_startup_doctor"] =
        crate::cli::doctor::startup::latest_startup_doctor_health_json(detailed);
    json
}

fn public_health_json(json: serde_json::Value) -> serde_json::Value {
    let status = json
        .get("status")
        .cloned()
        .unwrap_or_else(|| serde_json::json!("unknown"));
    let version = json
        .get("version")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(env!("CARGO_PKG_VERSION")));
    let db = json
        .get("db")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(false));
    let dashboard = json
        .get("dashboard")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(false));
    let server_up = json.get("server_up").cloned().unwrap_or_else(|| db.clone());
    let fully_recovered = json
        .get("fully_recovered")
        .cloned()
        .unwrap_or_else(|| server_up.clone());
    let degraded = status.as_str().is_some_and(|status| status != "healthy");
    serde_json::json!({
        "ok": !degraded,
        "status": status,
        "version": version,
        "db": db,
        "dashboard": dashboard,
        "server_up": server_up,
        "fully_recovered": fully_recovered,
        "degraded": degraded,
    })
}

fn stale_mailbox_repair_applied(
    removed_token: bool,
    inflight_cleared: bool,
    session_disconnected_count: usize,
) -> bool {
    removed_token || inflight_cleared || session_disconnected_count > 0
}

async fn load_config_audit_report_pg(pg_pool: Option<&PgPool>) -> Option<serde_json::Value> {
    let pool = pg_pool?;
    let raw = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind("config_audit_report")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

async fn load_pipeline_override_report_pg(pg_pool: Option<&PgPool>) -> Option<serde_json::Value> {
    let pool = pg_pool?;
    let raw = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind("pipeline_override_health_report")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

async fn load_channel_session_state(
    pg_pool: Option<&PgPool>,
    channel_id: u64,
) -> Option<ChannelSessionState> {
    let channel_id = channel_id.to_string();
    if let Some(pool) = pg_pool {
        let row = sqlx::query(
            "SELECT agent_id, provider, status, active_dispatch_id, thread_channel_id
               FROM sessions
              WHERE thread_channel_id = $1
              ORDER BY last_heartbeat DESC NULLS LAST, id DESC
              LIMIT 1",
        )
        .bind(&channel_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
        return Some(ChannelSessionState {
            agent_id: row.try_get("agent_id").ok(),
            provider: row.try_get("provider").ok(),
            status: row.try_get("status").ok(),
            active_dispatch_id: row.try_get("active_dispatch_id").ok(),
            thread_channel_id: row.try_get("thread_channel_id").ok(),
        });
    }
    None
}

async fn mark_channel_sessions_disconnected(
    pg_pool: Option<&PgPool>,
    channel_id: u64,
) -> Result<usize, String> {
    let channel_id = channel_id.to_string();
    if let Some(pool) = pg_pool {
        return sqlx::query(
            "UPDATE sessions
                SET status = 'disconnected',
                    active_dispatch_id = NULL
              WHERE thread_channel_id = $1
                AND status = 'working'
                AND (active_dispatch_id IS NULL OR active_dispatch_id = '')",
        )
        .bind(&channel_id)
        .execute(pool)
        .await
        .map(|result| result.rows_affected() as usize)
        .map_err(|error| format!("mark postgres sessions disconnected: {error}"));
    }
    Err("postgres pool unavailable".to_string())
}

async fn enrich_mailbox_session_state(json: &mut serde_json::Value, state: &AppState) {
    let Some(mailboxes) = json
        .get_mut("mailboxes")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for mailbox in mailboxes {
        let Some(channel_id) = mailbox
            .get("channel_id")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        if let Some(session) = load_channel_session_state(state.pg_pool_ref(), channel_id).await {
            let active_dispatch_present = session
                .active_dispatch_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty());
            mailbox["session_record_present"] = serde_json::json!(true);
            mailbox["session_agent_id"] = serde_json::json!(session.agent_id);
            mailbox["session_provider"] = serde_json::json!(session.provider);
            mailbox["session_status"] = serde_json::json!(session.status);
            mailbox["session_active_dispatch_id"] = serde_json::json!(session.active_dispatch_id);
            mailbox["session_thread_channel_id"] = serde_json::json!(session.thread_channel_id);
            if active_dispatch_present {
                mailbox["active_dispatch_present"] = serde_json::json!(true);
            }
        } else {
            mailbox["session_record_present"] = serde_json::json!(false);
            mailbox["session_status"] = serde_json::Value::Null;
            mailbox["session_active_dispatch_id"] = serde_json::Value::Null;
        }
    }
}

/// GET /api/health — public safe health summary.
pub async fn health_handler(State(state): State<AppState>) -> Response {
    health_response(&state, false).await
}

/// GET /api/health/detail — authenticated or local detailed health.
pub async fn health_detail_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }
    health_response(&state, true).await
}

/// GET /api/doctor/startup/latest — protected/local latest startup doctor artifact.
pub async fn startup_doctor_latest_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }

    Json(crate::cli::doctor::startup::latest_startup_doctor_response_json()).into_response()
}

/// POST /api/doctor/stale-mailbox/repair — protected/local stale mailbox cleanup.
pub async fn stale_mailbox_repair_handler(
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

    let body_str = String::from_utf8_lossy(&body);
    let request = match serde_json::from_str::<StaleMailboxRepairRequest>(&body_str) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"ok": false, "error": format!("invalid request: {error}")}),
                ),
            )
                .into_response();
        }
    };

    let channel_id = ChannelId::new(request.channel_id);
    let Some(handle) =
        crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "safe_local_repair",
                "safety_gate": "mailbox_not_found",
                "skipped_reason": "no mailbox handle exists for channel",
                "post_repair_mailbox": null,
                "post_repair_watcher_inflight": null
            })),
        )
            .into_response();
    };

    let before = handle.snapshot().await;
    if request.expected_has_cancel_token.is_some()
        && request.expected_has_cancel_token != Some(before.cancel_token.is_some())
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "safe_local_repair",
                "safety_gate": "expected_evidence_mismatch",
                "skipped_reason": "mailbox evidence changed before repair",
                "post_repair_mailbox": {
                    "channel_id": request.channel_id,
                    "has_cancel_token": before.cancel_token.is_some(),
                    "queue_depth": before.intervention_queue.len(),
                    "recovery_started": before.recovery_started_at.is_some()
                },
                "post_repair_watcher_inflight": null
            })),
        )
            .into_response();
    }
    if !before.intervention_queue.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "safe_local_repair",
                "safety_gate": "queue_not_empty",
                "skipped_reason": "live queue evidence exists",
                "post_repair_mailbox": {
                    "channel_id": request.channel_id,
                    "has_cancel_token": before.cancel_token.is_some(),
                    "queue_depth": before.intervention_queue.len(),
                    "recovery_started": before.recovery_started_at.is_some()
                },
                "post_repair_watcher_inflight": null
            })),
        )
            .into_response();
    }

    let before_watcher_inflight = if let Some(registry) = state.health_registry.as_ref() {
        registry.snapshot_watcher_state(request.channel_id).await
    } else {
        None
    };
    let before_session_state =
        load_channel_session_state(state.pg_pool_ref(), request.channel_id).await;
    if before_session_state
        .as_ref()
        .and_then(|session| session.active_dispatch_id.as_deref())
        .is_some_and(|dispatch_id| !dispatch_id.trim().is_empty())
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "explicit_restart_required",
                "safety_gate": "active_dispatch_present",
                "skipped_reason": "session record still has active dispatch evidence",
                "pre_repair_session": before_session_state,
                "post_repair_mailbox": {
                    "channel_id": request.channel_id,
                    "has_cancel_token": before.cancel_token.is_some(),
                    "queue_depth": before.intervention_queue.len(),
                    "recovery_started": before.recovery_started_at.is_some()
                },
                "post_repair_watcher_inflight": before_watcher_inflight
            })),
        )
            .into_response();
    }
    let tmux_present = before_watcher_inflight
        .as_ref()
        .and_then(|snapshot| snapshot.tmux_session.as_deref())
        .is_some_and(crate::services::platform::tmux::has_session);
    if tmux_present {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "explicit_restart_required",
                "safety_gate": "tmux_present",
                "skipped_reason": "live tmux evidence exists",
                "post_repair_mailbox": {
                    "channel_id": request.channel_id,
                    "has_cancel_token": before.cancel_token.is_some(),
                    "queue_depth": before.intervention_queue.len(),
                    "recovery_started": before.recovery_started_at.is_some()
                },
                "post_repair_watcher_inflight": before_watcher_inflight
            })),
        )
            .into_response();
    }

    let repair = handle.hard_stop().await;
    let session_disconnect_result =
        mark_channel_sessions_disconnected(state.pg_pool_ref(), request.channel_id).await;
    let (session_disconnected_count, session_disconnect_error) = match session_disconnect_result {
        Ok(count) => (count, None),
        Err(error) => (0, Some(error)),
    };
    let mut inflight_cleared = false;
    if let Some(snapshot) = before_watcher_inflight.as_ref()
        && snapshot.inflight_state_present
        && !snapshot.attached
        && let Some(provider) = ProviderKind::from_str(&snapshot.provider)
    {
        crate::services::discord::clear_inflight_state_for_channel(&provider, request.channel_id);
        inflight_cleared = true;
    }
    let after = handle.snapshot().await;
    let after_watcher_inflight = if let Some(registry) = state.health_registry.as_ref() {
        registry.snapshot_watcher_state(request.channel_id).await
    } else {
        None
    };
    let after_session_state =
        load_channel_session_state(state.pg_pool_ref(), request.channel_id).await;
    let residual_inflight = after_watcher_inflight
        .as_ref()
        .is_some_and(|snapshot| snapshot.inflight_state_present || snapshot.attached);
    let residual_working_session = after_session_state
        .as_ref()
        .and_then(|session| session.status.as_deref())
        == Some("working");
    let status =
        if residual_inflight || residual_working_session || session_disconnect_error.is_some() {
            "partial_repair"
        } else {
            "applied"
        };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": status == "applied",
            "status": status,
            "applied": stale_mailbox_repair_applied(
                repair.removed_token.is_some(),
                inflight_cleared,
                session_disconnected_count
            ),
            "skipped": false,
            "fix_safety": "safe_local_repair",
            "safety_gate": "no_live_work_evidence",
            "inflight_cleared": inflight_cleared,
            "session_disconnected_count": session_disconnected_count,
            "session_disconnect_error": session_disconnect_error,
            "pre_repair_session": before_session_state,
            "post_repair_session": after_session_state,
            "delivery_completed": false,
            "post_repair_mailbox": {
                "channel_id": request.channel_id,
                "has_cancel_token": after.cancel_token.is_some(),
                "queue_depth": after.intervention_queue.len(),
                "recovery_started": after.recovery_started_at.is_some()
            },
            "post_repair_watcher_inflight": after_watcher_inflight
        })),
    )
        .into_response()
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
    let (status_str, response_body) = health::handle_send(
        registry,
        health_legacy_db(&state),
        state.pg_pool_ref(),
        &body_str,
    )
    .await;
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
    let (status_str, response_body) = health::handle_send_to_agent(
        registry,
        health_legacy_db(&state),
        state.pg_pool_ref(),
        &body_str,
    )
    .await;
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
    use super::{
        discord_control_endpoints_allowed, public_health_json, stale_mailbox_repair_applied,
    };
    use serde_json::json;

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

    #[test]
    fn public_health_json_redacts_provider_and_mailbox_details() {
        let public = public_health_json(json!({
            "status": "degraded",
            "version": "0.1.2",
            "db": true,
            "dashboard": false,
            "providers": [{"name": "codex", "connected": true}],
            "mailboxes": [{"channel_id": 123, "has_cancel_token": true}],
            "config_audit": {"warnings": ["secret-ish path"]},
            "degraded_reasons": ["provider:codex:pending_queue_depth:2"]
        }));

        assert_eq!(public["status"], "degraded");
        assert_eq!(public["version"], "0.1.2");
        assert_eq!(public["db"], true);
        assert_eq!(public["dashboard"], false);
        assert_eq!(public["degraded"], true);
        assert!(public.get("providers").is_none());
        assert!(public.get("mailboxes").is_none());
        assert!(public.get("config_audit").is_none());
        assert!(public.get("degraded_reasons").is_none());
    }

    #[test]
    fn stale_mailbox_repair_applied_includes_session_only_disconnect() {
        assert!(stale_mailbox_repair_applied(false, false, 1));
        assert!(stale_mailbox_repair_applied(true, false, 0));
        assert!(stale_mailbox_repair_applied(false, true, 0));
        assert!(!stale_mailbox_repair_applied(false, false, 0));
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

async fn load_dispatch_outbox_stats(pg_pool: Option<&PgPool>) -> Option<DispatchOutboxStats> {
    if let Some(pool) = pg_pool {
        if let Some(stats) = load_dispatch_outbox_stats_pg(pool).await {
            return Some(stats);
        }
        tracing::warn!("[health] failed to load dispatch_outbox stats from PostgreSQL");
    }
    None
}

async fn probe_server_up(pg_pool: Option<&PgPool>) -> bool {
    if let Some(pool) = pg_pool {
        return sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(pool)
            .await
            .is_ok();
    }
    false
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
    // Use COALESCE(next_attempt_at, created_at) so rows that were re-queued
    // by boot reconcile (processing→pending) reflect their re-queue time,
    // not the original creation time. This keeps the promote health gate
    // accurate after restarts without inflating age with rows that the
    // outbox worker is about to pick up.
    let oldest_pending_age = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(
             CAST(
                 EXTRACT(
                     EPOCH FROM (NOW() - MIN(COALESCE(next_attempt_at, created_at)))
                 ) AS BIGINT
             ),
             0
         )
         FROM dispatch_outbox
         WHERE status = 'pending'
           AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())",
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
