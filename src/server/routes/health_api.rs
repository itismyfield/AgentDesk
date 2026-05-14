use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row, postgres::PgRow};
use std::net::SocketAddr;

use crate::db::session_status::is_active_status;
use crate::services::discord::health;
use crate::services::disk_monitor;
use crate::services::provider::ProviderKind;

use super::AppState;

const OUTBOX_AGE_DEGRADED_SECS: i64 = 60;

struct DispatchOutboxStats {
    pending: i64,
    retrying: i64,
    permanent_failures: i64,
    oldest_pending_age: i64,
}

#[derive(Debug, Deserialize)]
pub struct AckDispatchOutboxFailuresRequest {
    #[serde(default)]
    pub ids: Option<Vec<i64>>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
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

#[derive(Debug, Default, Deserialize)]
struct RelayRecoveryRequest {
    provider: Option<String>,
    #[serde(default)]
    apply: bool,
}

fn local_or_configured_control_endpoint_allowed(
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

/// #2049 Finding 2: ground truth `fully_recovered` predicate. The field must
/// be `true` if and only if (a) no `degraded_reasons` were collected by the
/// composed health pipeline AND (b) the final `HealthStatus` is HTTP-ready
/// (Healthy/Degraded — *not* Unhealthy). Centralised here so any future
/// degradation check appended to `health_response` automatically participates
/// in the recovery signal.
fn compute_fully_recovered(
    status: health::HealthStatus,
    degraded_reasons: &[serde_json::Value],
) -> bool {
    degraded_reasons.is_empty() && status.is_http_ready()
}

/// #2049 Finding 11 — constant-time bearer comparison.
/// `str` PartialEq short-circuits on the first byte mismatch, exposing the
/// token length and prefix to remote timing observation when the server is
/// reachable from non-loopback hosts. Use `subtle::ConstantTimeEq` so every
/// matched-length comparison takes the same wall-clock regardless of where
/// the bytes differ. Length itself is not secret in our threat model so the
/// length check happens outside the constant-time path.
fn bearer_token_matches(config: &crate::config::Config, headers: &HeaderMap) -> bool {
    use subtle::ConstantTimeEq;

    let Some(expected_token) = config.server.auth_token.as_deref() else {
        return false;
    };
    if expected_token.is_empty() {
        return false;
    }

    let Some(supplied) = headers
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };

    if supplied.len() != expected_token.len() {
        return false;
    }
    supplied.as_bytes().ct_eq(expected_token.as_bytes()).into()
}

fn discord_control_endpoints_allowed(
    config: &crate::config::Config,
    peer_addr: Option<SocketAddr>,
    headers: &HeaderMap,
) -> bool {
    if peer_addr.is_some_and(|addr| addr.ip().is_loopback()) {
        return true;
    }

    bearer_token_matches(config, headers)
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
        // #2049 Finding 3: detect cluster-standby up-front (so we know to
        // suppress the `no_providers_registered` noise), but defer the
        // `status = Healthy` rewrite until *after* every worsen check below
        // — otherwise standby would mask the worsen signal it just unmasked.
        let cluster_standby_without_gateway =
            cluster_standby_without_gateway(state, server_up, &degraded_reasons).await;
        if cluster_standby_without_gateway {
            degraded_reasons.retain(|reason| reason.as_str() != Some("no_providers_registered"));
            json["cluster_standby"] = serde_json::json!(true);
        }

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

        // #2049 Finding 4: surface startup_doctor failed/warned counts in the
        // top-level status + degraded_reasons so external health watchers see
        // the boot-time damage. The doctor summary itself is still attached
        // by `with_latest_startup_doctor` below.
        let (doctor_failed, doctor_warned) =
            crate::cli::doctor::startup::latest_startup_doctor_counts();
        if doctor_failed > 0 {
            status = status.worsen(health::HealthStatus::Unhealthy);
            degraded_reasons.push(serde_json::json!(format!(
                "startup_doctor_failed:{}",
                doctor_failed
            )));
        }
        if doctor_warned > 0 {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!(format!(
                "startup_doctor_warned:{}",
                doctor_warned
            )));
        }

        // #2049 Finding 3: now that every worsen check has run, lift status
        // to Healthy only when standby has *no other* degraded reasons.
        if cluster_standby_without_gateway && degraded_reasons.is_empty() {
            status = health::HealthStatus::Healthy;
        }

        // #2049 Finding 2 (+ Finding 3): recompute `fully_recovered`
        // from the final set of degraded reasons + final status so that
        // DB/disk/outbox/pipeline/doctor regressions cannot leave the field
        // stale-true. This also overrides the cluster_standby short-circuit
        // above when later checks discover real degradation.
        let fully_recovered = compute_fully_recovered(status, &degraded_reasons);
        json["fully_recovered"] = serde_json::json!(fully_recovered);

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
    let cluster_standby = json
        .get("cluster_standby")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(false));
    let degraded = status.as_str().is_some_and(|status| status != "healthy");
    serde_json::json!({
        "ok": !degraded,
        "status": status,
        "version": version,
        "db": db,
        "dashboard": dashboard,
        "server_up": server_up,
        "fully_recovered": fully_recovered,
        "cluster_standby": cluster_standby,
        "degraded": degraded,
    })
}

async fn cluster_standby_without_gateway(
    state: &AppState,
    server_up: bool,
    degraded_reasons: &[serde_json::Value],
) -> bool {
    if !server_up || !state.config.cluster.enabled {
        return false;
    }
    if !degraded_reasons
        .iter()
        .any(|reason| reason.as_str() == Some("no_providers_registered"))
    {
        return false;
    }
    let instance_id = state
        .config
        .cluster
        .instance_id
        .as_deref()
        .unwrap_or("")
        .trim();
    if instance_id.is_empty() {
        return false;
    }
    let Some(pool) = state.pg_pool_ref() else {
        return false;
    };
    let ttl_secs = state.config.cluster.lease_ttl_secs.max(1) as f64;
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT effective_role
          FROM worker_nodes
         WHERE instance_id = $1
           AND last_heartbeat_at >= NOW() - ($2::double precision * INTERVAL '1 second')
        "#,
    )
    .bind(instance_id)
    .bind(ttl_secs)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .as_deref()
        == Some("worker")
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

/// #2049 Finding 16: the handler-layer pre-check uses
/// `dispatch_id.trim().is_empty()` but the UPDATE WHERE used
/// `active_dispatch_id = ''`. A whitespace-only dispatch id (e.g. `' '`)
/// would pass the UPDATE but be rejected by the pre-check, so the two
/// definitions of "no live work" disagreed. `COALESCE(btrim(...), '') = ''`
/// makes them match.
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
                AND status IN ('turn_active', 'working')
                AND COALESCE(btrim(active_dispatch_id), '') = ''",
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
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }
    health_response(&state, true).await
}

pub async fn list_dispatch_outbox_failures_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "pg pool unavailable"})),
        );
    };
    match load_failed_dispatch_outbox_rows(pool, None).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "count": rows.len(),
                "rows": rows,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": error.to_string()})),
        ),
    }
}

pub async fn ack_dispatch_outbox_failures_handler(
    State(state): State<AppState>,
    Json(request): Json<AckDispatchOutboxFailuresRequest>,
) -> impl IntoResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "pg pool unavailable"})),
        );
    };
    let ids = request.ids.as_deref();
    if ids.is_none() && !request.dry_run.unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "ids required unless dry_run is true",
            })),
        );
    }
    let rows = match load_failed_dispatch_outbox_rows(pool, ids).await {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": error.to_string()})),
            );
        }
    };
    if rows.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "acknowledged": 0,
                "dry_run": request.dry_run.unwrap_or(false),
                "rows": [],
            })),
        );
    }
    if request.dry_run.unwrap_or(false) {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "acknowledged": 0,
                "dry_run": true,
                "would_acknowledge": rows.len(),
                "rows": rows,
            })),
        );
    }

    let row_ids = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(serde_json::Value::as_i64))
        .collect::<Vec<_>>();
    let reason = request
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("operator acknowledged failed dispatch_outbox rows");
    match acknowledge_failed_dispatch_outbox_rows(pool, &row_ids, reason).await {
        Ok(acknowledged_ids) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "acknowledged": acknowledged_ids.len(),
                "dry_run": false,
                "acknowledged_ids": acknowledged_ids,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": error.to_string()})),
        ),
    }
}

/// GET /api/doctor/startup/latest — protected/local latest startup doctor artifact.
pub async fn startup_doctor_latest_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
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
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
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
        .is_some_and(is_active_status);
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

/// POST /api/channels/{id}/relay-recovery — protected/local relay recovery dry-run.
pub async fn relay_recovery_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Path(channel_id): Path<String>,
    body: Bytes,
) -> Response {
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response();
    }

    let channel_id = match channel_id.parse::<u64>() {
        Ok(channel_id) if channel_id > 0 => channel_id,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": "channel_id must be a numeric Discord channel ID"
                })),
            )
                .into_response();
        }
    };

    let Some(ref registry) = state.health_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "Discord not available (standalone mode)"})),
        )
            .into_response();
    };

    let request = if body.is_empty() {
        RelayRecoveryRequest::default()
    } else {
        let body_str = String::from_utf8_lossy(&body);
        match serde_json::from_str::<RelayRecoveryRequest>(&body_str) {
            Ok(request) => request,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!("invalid request: {error}")
                    })),
                )
                    .into_response();
            }
        }
    };

    let provider = request.provider.as_deref();
    let (status_str, response_body) =
        health::handle_relay_recovery(registry, provider, channel_id, request.apply).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/discord/send — agent-to-agent native routing.
///
/// Requires `ConnectInfo<SocketAddr>` injected by the server bootstrap
/// (see `boot.rs::run_with_state` and `mod.rs::launch_*` which both call
/// `into_make_service_with_connect_info::<SocketAddr>`). The
/// Non-loopback callers must present an explicit bearer token even though the
/// route is also in the protected API domain; that keeps control traffic out
/// of the same-origin dashboard bypass used by ordinary dashboard routes.
pub async fn send_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
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
        health::handle_send(registry, None, state.pg_pool_ref(), &body_str).await;
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
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
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

/// POST /api/discord/send-to-agent — role_id-based agent routing.
///
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
pub async fn send_to_agent_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
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
        health::handle_send_to_agent(registry, None, state.pg_pool_ref(), &body_str).await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/discord/send-dm — send a DM to a Discord user.
///
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
pub async fn senddm_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
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
    use axum::{
        body::Body,
        http::{HeaderMap, Request, StatusCode, header::AUTHORIZATION},
    };
    use serde_json::json;
    use tower::ServiceExt;

    fn empty_headers() -> HeaderMap {
        HeaderMap::new()
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .expect("valid bearer header"),
        );
        headers
    }

    #[test]
    fn discord_control_endpoints_allow_loopback_peer_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some(String::new());

        assert!(discord_control_endpoints_allowed(
            &config,
            Some("127.0.0.1:8791".parse().unwrap()),
            &empty_headers()
        ));
        assert!(discord_control_endpoints_allowed(
            &config,
            Some("[::1]:8791".parse().unwrap()),
            &empty_headers()
        ));
    }

    #[test]
    fn discord_control_endpoints_reject_non_loopback_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some(String::new());

        assert!(!discord_control_endpoints_allowed(
            &config,
            Some("10.0.0.5:8791".parse().unwrap()),
            &empty_headers()
        ));
        assert!(!discord_control_endpoints_allowed(
            &config,
            None,
            &empty_headers()
        ));
    }

    #[test]
    fn discord_control_endpoints_require_bearer_for_non_loopback_when_auth_token_is_set() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some("secret".to_string());

        assert!(!discord_control_endpoints_allowed(
            &config,
            Some("10.0.0.5:8791".parse().unwrap()),
            &empty_headers()
        ));
        assert!(discord_control_endpoints_allowed(
            &config,
            Some("10.0.0.5:8791".parse().unwrap()),
            &bearer_headers("secret")
        ));
        assert!(!discord_control_endpoints_allowed(
            &config,
            Some("10.0.0.5:8791".parse().unwrap()),
            &bearer_headers("wrong")
        ));
    }

    fn test_api_router_with_config(config: crate::config::Config) -> axum::Router {
        let mut engine_config = crate::config::Config::default();
        engine_config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        crate::server::routes::api_router_with_pg(engine, config, tx, buf, None, None)
    }

    fn control_request(peer: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/discord/send")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        request
    }

    #[tokio::test]
    async fn discord_control_router_rejects_non_loopback_auth_token_without_bearer() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some("secret".to_string());
        let app = test_api_router_with_config(config);

        let response = app.oneshot(control_request("10.0.0.5:8791")).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn discord_control_router_rejects_same_origin_bypass_without_bearer() {
        // #2047 Finding 3 — the auth middleware now also requires the peer
        // address itself to be loopback before honouring an `Origin: localhost`
        // header. A LAN attacker (10.0.0.5) who forges the same-origin header
        // is rejected by the middleware at 401 (Unauthorized), strictly tighter
        // than the previous handler-layer 403.
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some("secret".to_string());
        let app = test_api_router_with_config(config);

        let mut request = control_request("10.0.0.5:8791");
        request.headers_mut().insert(
            "origin",
            "http://localhost:8791".parse().expect("valid origin"),
        );
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn discord_control_router_allows_non_loopback_with_bearer() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some("secret".to_string());
        let app = test_api_router_with_config(config);

        let mut request = control_request("10.0.0.5:8791");
        request
            .headers_mut()
            .insert(AUTHORIZATION, "Bearer secret".parse().unwrap());
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn discord_control_router_keeps_loopback_dev_flow_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let app = test_api_router_with_config(config);

        let response = app
            .oneshot(control_request("127.0.0.1:8791"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn discord_control_router_rejects_non_loopback_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let app = test_api_router_with_config(config);

        let response = app.oneshot(control_request("10.0.0.5:8791")).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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

    /// #2049 Finding 2 regression guard: any time the composed health
    /// pipeline pushes a `degraded_reason`, `fully_recovered` must flip
    /// to false even if Discord's per-provider snapshot reported true.
    #[test]
    fn compute_fully_recovered_flips_to_false_when_degraded_reasons_present() {
        use super::compute_fully_recovered;
        use crate::services::discord::health;

        // Clean state — healthy + no reasons → fully_recovered=true.
        assert!(compute_fully_recovered(health::HealthStatus::Healthy, &[]));

        // Any reason flips the recovery signal off, even when the snapshot
        // reported Healthy upstream.
        let reasons_db = vec![json!("db_unavailable")];
        assert!(!compute_fully_recovered(
            health::HealthStatus::Healthy,
            &reasons_db
        ));

        // Multiple reasons stay false.
        let reasons_outbox_disk = vec![
            json!("dispatch_outbox_oldest_pending_age:120"),
            json!("disk_low_free_bytes:104857600"),
        ];
        assert!(!compute_fully_recovered(
            health::HealthStatus::Degraded,
            &reasons_outbox_disk
        ));

        // Unhealthy status → false even when (theoretically) reasons list
        // is empty. Guards against future code paths that worsen status
        // without pushing a reason string.
        assert!(!compute_fully_recovered(
            health::HealthStatus::Unhealthy,
            &[]
        ));

        // Doctor failure reason → false.
        let reasons_doctor = vec![json!("startup_doctor_failed:6")];
        assert!(!compute_fully_recovered(
            health::HealthStatus::Unhealthy,
            &reasons_doctor
        ));
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

async fn load_failed_dispatch_outbox_rows(
    pool: &PgPool,
    ids: Option<&[i64]>,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = if let Some(ids) = ids {
        if ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(
                "SELECT o.id,
                        o.dispatch_id,
                        o.action,
                        o.agent_id,
                        o.card_id,
                        o.title,
                        o.retry_count,
                        o.error,
                        o.delivery_status,
                        o.delivery_result,
                        o.created_at,
                        o.processed_at,
                        td.status AS dispatch_status
                   FROM dispatch_outbox o
              LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
                  WHERE o.status = 'failed'
                    AND o.id = ANY($1)
               ORDER BY o.processed_at DESC NULLS LAST, o.id DESC",
            )
            .bind(ids)
            .fetch_all(pool)
            .await?
        }
    } else {
        sqlx::query(
            "SELECT o.id,
                    o.dispatch_id,
                    o.action,
                    o.agent_id,
                    o.card_id,
                    o.title,
                    o.retry_count,
                    o.error,
                    o.delivery_status,
                    o.delivery_result,
                    o.created_at,
                    o.processed_at,
                    td.status AS dispatch_status
               FROM dispatch_outbox o
          LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
              WHERE o.status = 'failed'
           ORDER BY o.processed_at DESC NULLS LAST, o.id DESC
              LIMIT 100",
        )
        .fetch_all(pool)
        .await?
    };

    rows.into_iter()
        .map(dispatch_outbox_failure_row_json)
        .collect()
}

fn dispatch_outbox_failure_row_json(row: PgRow) -> Result<serde_json::Value, sqlx::Error> {
    Ok(serde_json::json!({
        "id": row.try_get::<i64, _>("id")?,
        "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id")?,
        "action": row.try_get::<String, _>("action")?,
        "agent_id": row.try_get::<Option<String>, _>("agent_id")?,
        "card_id": row.try_get::<Option<String>, _>("card_id")?,
        "title": row.try_get::<Option<String>, _>("title")?,
        "retry_count": row.try_get::<i64, _>("retry_count")?,
        "error": row.try_get::<Option<String>, _>("error")?,
        "delivery_status": row.try_get::<Option<String>, _>("delivery_status")?,
        "delivery_result": row.try_get::<Option<serde_json::Value>, _>("delivery_result")?,
        "created_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("created_at")?,
        "processed_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("processed_at")?,
        "dispatch_status": row.try_get::<Option<String>, _>("dispatch_status")?,
    }))
}

async fn acknowledge_failed_dispatch_outbox_rows(
    pool: &PgPool,
    ids: &[i64],
    reason: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "UPDATE dispatch_outbox
            SET status = 'acknowledged',
                delivery_status = 'acknowledged',
                delivery_result = jsonb_build_object(
                    'acknowledged_at', NOW(),
                    'reason', $2::TEXT,
                    'previous_delivery_status', delivery_status,
                    'previous_delivery_result', delivery_result
                ),
                claimed_at = NULL,
                claim_owner = NULL
          WHERE status = 'failed'
            AND id = ANY($1)
      RETURNING id",
    )
    .bind(ids)
    .bind(reason)
    .fetch_all(pool)
    .await
}
