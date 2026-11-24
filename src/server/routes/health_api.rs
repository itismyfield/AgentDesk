use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use poise::serenity_prelude::ChannelId;
use serde::Deserialize;
use std::net::SocketAddr;

use crate::db::session_status::is_active_status;
use crate::error::{AppError, ErrorCode};
use crate::services::discord::{health, outbound};
use crate::services::message_outbox::source_registry::SendCallerClass;
use crate::services::provider::ProviderKind;
use crate::services::{disk_monitor, health_diagnostics};

use super::AppState;

mod db_collection;
#[cfg(test)]
mod db_deadline_tests;
#[cfg(test)]
mod host_guard_tests;
#[cfg(all(test, unix))]
mod presence_unknown_tests;
/// Disclosure rules for the unauthenticated `/api/health` body.
mod public_projection;
mod response;
mod runtime_profile;
mod session_repair;
#[cfg(all(test, unix))]
mod unread_tail_attribution_tests;
mod writer_readiness;
use public_projection::public_health_json;
use runtime_profile::attach_runtime_profile;

const X_AGENTDESK_SOURCE: &str = "x-agentdesk-source";

/// Preserve the long-standing health-control API envelope while centralizing
/// status, category, and message construction in `AppError`.
fn legacy_health_error(error: AppError) -> (StatusCode, Json<serde_json::Value>) {
    let status = error.status();
    let mut body = serde_json::json!({"ok": false, "error": error.message()});
    body.as_object_mut()
        .expect("health error envelope is an object")
        .extend(error.context().clone());
    (status, Json(body))
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

#[derive(Debug, Deserialize)]
struct StaleMailboxRepairRequest {
    channel_id: u64,
    #[serde(default)]
    provider: Option<String>,
    expected_has_cancel_token: Option<bool>,
    /// When true and the repair fully applied, also unlink the channel's idle
    /// in-memory mailbox registry entry (no disk/DB mutation).
    #[serde(default)]
    purge: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RelayRecoveryRequest {
    provider: Option<String>,
    #[serde(default)]
    apply: bool,
}

pub(super) fn local_or_configured_control_endpoint_allowed(
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

/// Read handlers also accept direct tailnet socket peers, independently of request headers.
pub(super) fn read_rejection(
    config: &crate::config::Config,
    peer_addr: SocketAddr,
) -> Option<Response> {
    if local_or_configured_control_endpoint_allowed(config, Some(peer_addr))
        || super::auth::is_tailnet_peer(Some(peer_addr))
    {
        return None;
    }
    Some(
        legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response(),
    )
}

/// `fully_recovered` tracks startup/recovery completion only; live degradation is
/// reported by `status` and `degraded_reasons`, so operators can tell the two apart.
fn compute_fully_recovered(
    snapshot_fully_recovered: bool,
    status: health::HealthStatus,
    degraded_reasons: &[serde_json::Value],
) -> bool {
    let _ = (status, degraded_reasons);
    snapshot_fully_recovered
}

fn provider_deferred_hooks_backlog_recovered(
    live_deferred_hooks: u64,
    degraded_reasons: &[serde_json::Value],
) -> bool {
    live_deferred_hooks == 0
        && !degraded_reasons.iter().any(|reason| {
            reason
                .as_str()
                .is_some_and(crate::cli::doctor::startup::is_provider_deferred_hooks_backlog_reason)
        })
}

fn bearer_token_matches(config: &crate::config::Config, headers: &HeaderMap) -> bool {
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

    crate::utils::auth::constant_time_token_eq(expected_token, supplied)
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

fn discord_send_caller_class(peer_addr: SocketAddr, headers: &HeaderMap) -> SendCallerClass {
    let source_header = headers.get(X_AGENTDESK_SOURCE);
    let header_class = source_header
        .and_then(|value| value.to_str().ok())
        .and_then(SendCallerClass::from_header);
    match header_class {
        Some(SendCallerClass::LoopbackInternal) if peer_addr.ip().is_loopback() => {
            SendCallerClass::LoopbackInternal
        }
        Some(SendCallerClass::LoopbackInternal) => SendCallerClass::Unknown,
        Some(class) => class,
        None if source_header.is_some() => SendCallerClass::Unknown,
        None if peer_addr.ip().is_loopback() => SendCallerClass::LoopbackInternal,
        None => SendCallerClass::Unknown,
    }
}

fn with_latest_startup_doctor(mut json: serde_json::Value, detailed: bool) -> serde_json::Value {
    let doctor = crate::cli::doctor::startup::latest_startup_doctor_health_json(detailed);
    let startup_status = startup_status_from_doctor(&doctor);
    let mut startup_degraded_reasons = json
        .get("startup_degraded_reasons")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_else(|| startup_doctor_reasons_from_summary(&doctor));
    ensure_startup_doctor_state_reason(&doctor, &mut startup_degraded_reasons);
    let startup_degraded =
        startup_status_is_degraded(startup_status) || !startup_degraded_reasons.is_empty();

    json["startup_status"] = serde_json::json!(startup_status);
    json["startup_degraded"] = serde_json::json!(startup_degraded);
    json["startup_degraded_reasons"] = serde_json::Value::Array(startup_degraded_reasons);
    json["latest_startup_doctor"] = doctor;
    json
}

fn startup_status_from_doctor(doctor: &serde_json::Value) -> &'static str {
    match doctor
        .get("doctor_status")
        .and_then(serde_json::Value::as_str)
    {
        Some("pending") => "doctor_pending",
        Some("missing") => "doctor_missing",
        Some("error") => "doctor_error",
        Some("failed") => "doctor_failed",
        Some("warned") => "doctor_warned",
        Some("passed") => "doctor_passed",
        Some("skipped") => "doctor_skipped",
        _ => "doctor_unknown",
    }
}

fn startup_status_is_degraded(status: &str) -> bool {
    matches!(
        status,
        "doctor_pending" | "doctor_error" | "doctor_failed" | "doctor_warned"
    )
}

fn startup_doctor_count_reasons(failed_count: u64, warned_count: u64) -> serde_json::Value {
    serde_json::Value::Array(startup_doctor_count_reason_vec(failed_count, warned_count))
}

fn startup_doctor_count_reason_vec(failed_count: u64, warned_count: u64) -> Vec<serde_json::Value> {
    let mut reasons = Vec::new();
    if failed_count > 0 {
        reasons.push(serde_json::json!(format!(
            "startup_doctor_failed:{}",
            failed_count
        )));
    }
    if warned_count > 0 {
        reasons.push(serde_json::json!(format!(
            "startup_doctor_warned:{}",
            warned_count
        )));
    }
    reasons
}

fn startup_doctor_reasons_from_summary(doctor: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut reasons = startup_doctor_count_reason_vec(
        doctor["failed_count"].as_u64().unwrap_or(0),
        doctor["warned_count"].as_u64().unwrap_or(0),
    );
    ensure_startup_doctor_state_reason(doctor, &mut reasons);
    reasons
}

fn ensure_startup_doctor_state_reason(
    doctor: &serde_json::Value,
    reasons: &mut Vec<serde_json::Value>,
) {
    let reason = match doctor
        .get("doctor_status")
        .and_then(serde_json::Value::as_str)
    {
        Some("pending") => "startup_doctor_pending",
        Some("error") => "startup_doctor_error",
        _ => return,
    };
    if !reasons
        .iter()
        .any(|existing| existing.as_str() == Some(reason))
    {
        reasons.push(serde_json::json!(reason));
    }
}

fn delivery_record_rollout_health_json() -> serde_json::Value {
    outbound::delivery_record_rollout_health_json()
}

fn stale_mailbox_repair_applied(
    removed_token: bool,
    inflight_cleared: bool,
    session_disconnected_count: usize,
) -> bool {
    removed_token || inflight_cleared || session_disconnected_count > 0
}

/// Whether the optional mailbox-registry purge may run after a stale-mailbox
/// repair, and the skip reason to report when it may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryPurgeDecision {
    /// `purge` was not requested — report nothing.
    NotRequested,
    /// Repair fully applied (no residual inflight/session/token evidence) —
    /// the idle-entry purge may run.
    Run,
    /// Purge requested but the repair did not fully apply — refuse.
    Skip(&'static str),
}

fn registry_purge_decision(purge_requested: bool, repair_status: &str) -> RegistryPurgeDecision {
    if !purge_requested {
        return RegistryPurgeDecision::NotRequested;
    }
    if repair_status == "applied" {
        RegistryPurgeDecision::Run
    } else {
        RegistryPurgeDecision::Skip("repair_not_fully_applied")
    }
}

/// Build the `opencode` warm-pool diagnostics block: counts only, plus the redacted
/// per-server array when `detailed`. `None` (field omitted) when no resident server exists.
fn opencode_warm_pool_json(detailed: bool) -> Option<serde_json::Value> {
    let snapshots = crate::services::opencode::warm_server_snapshots();
    if snapshots.is_empty() {
        return None;
    }
    let count = snapshots.len() as u64;
    let active_sessions: u64 = snapshots.iter().map(|s| s.active_sessions as u64).sum();
    let suspicious_count = snapshots
        .iter()
        .filter(|s| s.suspicious_active_leak)
        .count() as u64;
    let mut block = serde_json::json!({
        "warm_server_count": count,
        "warm_server_active_sessions": active_sessions,
        "warm_server_suspicious_count": suspicious_count,
    });
    if detailed {
        block["warm_servers"] =
            serde_json::to_value(&snapshots).unwrap_or_else(|_| serde_json::json!([]));
    }
    Some(block)
}

/// `degraded_reasons` for the resident warm pool, distinct from the fresh-serve / MCP
/// doctor checks.
fn opencode_warm_pool_degraded_reasons() -> Vec<serde_json::Value> {
    let snapshots = crate::services::opencode::warm_server_snapshots();
    let mut reasons = Vec::new();
    let suspicious = snapshots
        .iter()
        .filter(|s| s.suspicious_active_leak)
        .count();
    if suspicious > 0 {
        reasons.push(serde_json::json!(format!(
            "opencode_warm_server:suspicious_active_leak:{suspicious}"
        )));
    }
    let stopped = snapshots.iter().filter(|s| !s.running).count();
    if stopped > 0 {
        reasons.push(serde_json::json!(format!(
            "opencode_warm_server:stopped_resident:{stopped}"
        )));
    }
    reasons
}

/// Fold worker recovery reasons into health for both `health_response` branches. Flapping
/// stays out (reported as `worker_restart_flapping`) so it cannot trip the deploy gate.
fn apply_worker_recovery_reasons(
    status: &mut health::HealthStatus,
    degraded_reasons: &mut Vec<serde_json::Value>,
) {
    use crate::server::worker_recovery::RecoveryReasonSeverity;
    for reason in crate::server::worker_recovery::recovery_health_reasons() {
        let worsened = match reason.severity {
            RecoveryReasonSeverity::Unhealthy => health::HealthStatus::Unhealthy,
            RecoveryReasonSeverity::Degraded => health::HealthStatus::Degraded,
        };
        *status = status.worsen(worsened);
        degraded_reasons.push(serde_json::json!(reason.reason));
    }
}

/// GET /api/health — public safe health summary.
pub async fn health_handler(State(state): State<AppState>) -> Response {
    response::health_response(&state, false).await
}

/// GET /api/health/detail — authenticated, local or direct-tailnet detailed health.
pub async fn health_detail_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(response) = read_rejection(&state.config, peer_addr) {
        return response;
    }
    response::health_response(&state, true).await
}

pub async fn list_dispatch_outbox_failures_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "pg pool unavailable",
        ));
    };
    match health_diagnostics::load_failed_dispatch_outbox_rows(pool, None).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "count": rows.len(),
                "rows": rows,
            })),
        ),
        Err(error) => legacy_health_error(AppError::internal(error.to_string())),
    }
}

pub async fn ack_dispatch_outbox_failures_handler(
    State(state): State<AppState>,
    Json(request): Json<AckDispatchOutboxFailuresRequest>,
) -> impl IntoResponse {
    let Some(pool) = state.pg_pool_ref() else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "pg pool unavailable",
        ));
    };
    let ids = request.ids.as_deref();
    if ids.is_none() && !request.dry_run.unwrap_or(false) {
        return legacy_health_error(AppError::bad_request("ids required unless dry_run is true"));
    }
    let rows = match health_diagnostics::load_failed_dispatch_outbox_rows(pool, ids).await {
        Ok(rows) => rows,
        Err(error) => {
            return legacy_health_error(AppError::internal(error.to_string()));
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
    match health_diagnostics::acknowledge_failed_dispatch_outbox_rows(pool, &row_ids, reason).await
    {
        Ok(acknowledged_ids) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "acknowledged": acknowledged_ids.len(),
                "dry_run": false,
                "acknowledged_ids": acknowledged_ids,
            })),
        ),
        Err(error) => legacy_health_error(AppError::internal(error.to_string())),
    }
}

/// GET /api/doctor/startup/latest — authenticated, local or direct-tailnet doctor artifact.
pub async fn startup_doctor_latest_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(response) = read_rejection(&state.config, peer_addr) {
        return response;
    }

    Json(crate::cli::doctor::startup::doctor_json(&state.config)).into_response()
}

/// POST /api/doctor/stale-mailbox/repair — protected/local stale mailbox cleanup.
pub async fn stale_mailbox_repair_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let body_str = String::from_utf8_lossy(&body);
    let request = match serde_json::from_str::<StaleMailboxRepairRequest>(&body_str) {
        Ok(request) => request,
        Err(error) => {
            return legacy_health_error(AppError::bad_request(format!("invalid request: {error}")))
                .into_response();
        }
    };

    let provider_filter = match request
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
    {
        Some(provider) => match ProviderKind::from_str(provider) {
            Some(provider) => Some(provider),
            None => {
                return legacy_health_error(
                    AppError::bad_request("invalid provider").with_context("provider", provider),
                )
                .into_response();
            }
        },
        None => None,
    };

    let channel_id = ChannelId::new(request.channel_id);
    let global_handle = if provider_filter.is_none() {
        crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel_id)
    } else {
        None
    };
    let before = if let Some(provider) = provider_filter.as_ref() {
        let Some(registry) = state.health_registry.as_ref() else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "ok": false,
                    "applied": false,
                    "skipped": true,
                    "fix_safety": crate::cli::doctor::contract::FixSafety::SafeLocalRepair,
                    "safety_gate": "mailbox_not_found",
                    "skipped_reason": "provider-scoped mailbox registry unavailable",
                    "post_repair_mailbox": null,
                    "post_repair_watcher_inflight": null
                })),
            )
                .into_response();
        };
        let Some(state) =
            health::provider_channel_mailbox_state(registry, provider.as_str(), request.channel_id)
                .await
        else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "ok": false,
                    "applied": false,
                    "skipped": true,
                    "fix_safety": crate::cli::doctor::contract::FixSafety::SafeLocalRepair,
                    "safety_gate": "mailbox_not_found",
                    "skipped_reason": "no provider-scoped mailbox exists for channel",
                    "provider": provider.as_str(),
                    "post_repair_mailbox": null,
                    "post_repair_watcher_inflight": null
                })),
            )
                .into_response();
        };
        state
    } else {
        let Some(handle) = global_handle.as_ref() else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "ok": false,
                    "applied": false,
                    "skipped": true,
                    "fix_safety": crate::cli::doctor::contract::FixSafety::SafeLocalRepair,
                    "safety_gate": "mailbox_not_found",
                    "skipped_reason": "no mailbox handle exists for channel",
                    "post_repair_mailbox": null,
                    "post_repair_watcher_inflight": null
                })),
            )
                .into_response();
        };
        health::ProviderMailboxState::from_snapshot(request.channel_id, &handle.snapshot().await)
    };
    if request.expected_has_cancel_token.is_some()
        && request.expected_has_cancel_token != Some(before.has_cancel_token)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": crate::cli::doctor::contract::FixSafety::SafeLocalRepair,
                "safety_gate": "expected_evidence_mismatch",
                "skipped_reason": "mailbox evidence changed before repair",
                "post_repair_mailbox": before,
                "post_repair_watcher_inflight": null
            })),
        )
            .into_response();
    }
    if before.holds_input {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": crate::cli::doctor::contract::FixSafety::SafeLocalRepair,
                "safety_gate": "queue_not_empty",
                "skipped_reason": "live queue evidence exists",
                "post_repair_mailbox": before,
                "post_repair_watcher_inflight": null
            })),
        )
            .into_response();
    }

    let before_watcher_inflight = if let Some(registry) = state.health_registry.as_ref() {
        if let Some(provider) = provider_filter.as_ref() {
            registry
                .snapshot_watcher_state_for_provider(provider, request.channel_id)
                .await
        } else {
            registry.snapshot_watcher_state(request.channel_id).await
        }
    } else {
        None
    };
    let registry = state.health_registry.as_deref();
    if let Some(refusal) = session_repair::host_refusal(
        registry,
        request.channel_id,
        &before,
        &before_watcher_inflight,
    )
    .await
    {
        return refusal;
    }
    session_repair::with_session(
        health_diagnostics::load_channel_session_state(state.pg_pool_ref(), request.channel_id),
        &before,
        &before_watcher_inflight,
        || {
            let tmux_present = session_repair::tmux_present(&before, &before_watcher_inflight)?;
            if tmux_present && !session_repair::idle_tmux_admits(
                state.health_registry.is_some(), &before_watcher_inflight, request.channel_id,
            ) {
                return Err(session_repair::tmux_refusal(&before, &before_watcher_inflight));
            }
            Ok(tmux_present)
        },
        |session, tmux_present| {
            let (state, provider_filter, before, before_watcher_inflight, global_handle) =
                (&state, &provider_filter, &before, &before_watcher_inflight, &global_handle);
            async move {
                let before_session_state = session;
                if tmux_present {
                    let idle_tmux_repair = match (
                        state.health_registry.as_ref(),
                        before_watcher_inflight.as_ref(),
                    ) {
                        (Some(registry), Some(snapshot)) => {
                            let snapshot_provider = ProviderKind::from_str(&snapshot.provider);
                            let tmux_session = snapshot.tmux_session.as_deref();
                            if let (Some(provider), Some(tmux_session)) = (snapshot_provider, tmux_session) {
                                health::clear_idle_tmux_stale_turn(
                                    registry,
                                    provider.as_str(),
                                    request.channel_id,
                                    tmux_session,
                                    "stale_mailbox_idle_tmux_repair",
                                )
                                .await
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some(idle_repair) = idle_tmux_repair {
                        let after = if let Some(provider) = provider_filter.as_ref() {
                            match state.health_registry.as_ref() {
                                Some(registry) => health::provider_channel_mailbox_state(
                                    registry,
                                    provider.as_str(),
                                    request.channel_id,
                                )
                                .await
                                .unwrap_or(health::ProviderMailboxState::empty(request.channel_id)),
                                None => health::ProviderMailboxState::empty(request.channel_id),
                            }
                        } else if let Some(handle) = global_handle.as_ref() {
                            health::ProviderMailboxState::from_snapshot(
                                request.channel_id,
                                &handle.snapshot().await,
                            )
                        } else {
                            health::ProviderMailboxState::empty(request.channel_id)
                        };
                        let after_watcher_inflight = if let Some(registry) = state.health_registry.as_ref() {
                            if let Some(provider) = provider_filter.as_ref() {
                                registry
                                    .snapshot_watcher_state_for_provider(provider, request.channel_id)
                                    .await
                            } else {
                                registry.snapshot_watcher_state(request.channel_id).await
                            }
                        } else {
                            None
                        };
                        let residual_inflight = after_watcher_inflight
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.inflight_state_present || snapshot.attached);
                        let status =
                            if after.has_cancel_token || residual_inflight || idle_repair.has_pending_queue {
                                "partial_repair"
                            } else {
                                "applied"
                            };
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "ok": status == "applied",
                                "status": status,
                                "applied": idle_repair.had_active_turn
                                    || idle_repair.persistent_inflight_cleared
                                    || idle_repair.runtime_session_cleared,
                                "skipped": false,
                                "fix_safety": crate::cli::doctor::contract::FixSafety::SafeIdleTmuxRepair,
                                "safety_gate": "tmux_ready_for_input_no_unsent_output",
                                "inflight_cleared": idle_repair.persistent_inflight_cleared,
                                "runtime_session_cleared": idle_repair.runtime_session_cleared,
                                "pre_repair_session": before_session_state,
                                "delivery_completed": false,
                                "post_repair_mailbox": after,
                                "post_repair_watcher_inflight": after_watcher_inflight
                            })),
                        )
                            .into_response();
                    }
                    return session_repair::tmux_refusal(before, before_watcher_inflight);
                }

                let repair_had_active_turn = if let Some(provider) = provider_filter.as_ref() {
                    health::stop_runtime_turn_preserving_watcher(
                        state.health_registry.as_deref(),
                        Some(provider.as_str()),
                        Some(request.channel_id),
                        None,
                        "stale_mailbox_repair",
                    )
                    .await
                    .had_active_turn
                } else if global_handle.is_some() {
                    health::stop_providerless_runtime_turn_preserving_watcher_strict_ownership(
                        state.health_registry.as_deref(),
                        request.channel_id,
                        "stale_mailbox_repair",
                    )
                    .await
                    .had_active_turn
                } else {
                    false
                };
                let session_disconnect_result = health_diagnostics::mark_channel_sessions_disconnected(
                    state.pg_pool_ref(),
                    request.channel_id,
                )
                .await;
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
                    if crate::services::discord::clear_inflight_state_for_channel(&provider, request.channel_id) { health::repair_cleared_row(state.health_registry.as_deref(), &provider, request.channel_id).await; }
                    inflight_cleared = true;
                }
                let after_watcher_inflight = if let Some(registry) = state.health_registry.as_ref() {
                    if let Some(provider) = provider_filter.as_ref() {
                        registry
                            .snapshot_watcher_state_for_provider(provider, request.channel_id)
                            .await
                    } else {
                        registry.snapshot_watcher_state(request.channel_id).await
                    }
                } else {
                    None
                };
                let after = if let Some(provider) = provider_filter.as_ref() {
                    match state.health_registry.as_ref() {
                        Some(registry) => health::provider_channel_mailbox_state(
                            registry,
                            provider.as_str(),
                            request.channel_id,
                        )
                        .await
                        .unwrap_or(health::ProviderMailboxState::empty(request.channel_id)),
                        None => health::ProviderMailboxState::empty(request.channel_id),
                    }
                } else if let Some(handle) = global_handle.as_ref() {
                    health::ProviderMailboxState::from_snapshot(
                        request.channel_id,
                        &handle.snapshot().await,
                    )
                } else {
                    health::ProviderMailboxState::empty(request.channel_id)
                };
                let (after_session_state, session_lookup_error) = session_repair::post_session(
                    health_diagnostics::load_channel_session_state(state.pg_pool_ref(), request.channel_id).await,
                );
                let residual_inflight = after_watcher_inflight
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.inflight_state_present || snapshot.attached);
                let residual_working_session = after_session_state
                    .as_ref()
                    .and_then(|session| session.status.as_deref())
                    .is_some_and(is_active_status);
                let status = session_repair::post_status(
                    residual_inflight,
                    residual_working_session,
                    session_disconnect_error.as_deref(),
                    session_lookup_error.as_deref(),
                );
                // Purge only after a full repair; removal rechecks actor idleness.
                let (registry_entry_removed, registry_purge_skipped_reason) =
                    match registry_purge_decision(request.purge, status) {
                        RegistryPurgeDecision::NotRequested => (false, None),
                        RegistryPurgeDecision::Skip(reason) => (false, Some(reason)),
                        RegistryPurgeDecision::Run => match state.health_registry.as_deref() {
                            Some(registry) => {
                                let purge = health::purge_idle_channel_mailbox_registry_entry(
                                    registry,
                                    provider_filter.as_ref().map(ProviderKind::as_str),
                                    request.channel_id,
                                )
                                .await;
                                (purge.removed, purge.skipped_reason)
                            }
                            None => (false, Some("registry_unavailable")),
                        },
                    };
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "ok": status == "applied",
                        "status": status,
                        "applied": stale_mailbox_repair_applied(
                            repair_had_active_turn,
                            inflight_cleared,
                            session_disconnected_count
                        ),
                        "skipped": false,
                        "fix_safety": crate::cli::doctor::contract::FixSafety::SafeLocalRepair,
                        "safety_gate": "no_live_work_evidence",
                        "inflight_cleared": inflight_cleared,
                        "session_disconnected_count": session_disconnected_count,
                        "session_disconnect_error": session_disconnect_error,
                        "pre_repair_session": before_session_state,
                        "post_repair_session": after_session_state,
                        "post_repair_session_error": session_lookup_error,
                        "delivery_completed": false,
                        "registry_entry_removed": registry_entry_removed,
                        "registry_purge_skipped_reason": registry_purge_skipped_reason,
                        "post_repair_mailbox": after,
                        "post_repair_watcher_inflight": after_watcher_inflight
                    })),
                )
                    .into_response()
            }
        },
    )
    .await
}

/// POST /api/channels/{id}/relay-recovery — protected/local relay recovery dry-run.
pub async fn relay_recovery_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Path(channel_id): Path<String>,
    body: Bytes,
) -> Response {
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let channel_id = match channel_id.parse::<u64>() {
        Ok(channel_id) if channel_id > 0 => channel_id,
        _ => {
            return legacy_health_error(AppError::bad_request(
                "channel_id must be a numeric Discord channel ID",
            ))
            .into_response();
        }
    };

    let Some(ref registry) = state.health_registry else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        ))
        .into_response();
    };

    let request = if body.is_empty() {
        RelayRecoveryRequest::default()
    } else {
        let body_str = String::from_utf8_lossy(&body);
        match serde_json::from_str::<RelayRecoveryRequest>(&body_str) {
            Ok(request) => request,
            Err(error) => {
                return legacy_health_error(AppError::bad_request(format!(
                    "invalid request: {error}"
                )))
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
/// Requires `ConnectInfo<SocketAddr>`, which `server::run` provides via
/// `into_make_service_with_connect_info`. Non-loopback callers must send a bearer token
/// even inside the protected API domain, so the same-origin dashboard bypass never applies.
pub async fn send_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        ))
        .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let caller_class = discord_send_caller_class(peer_addr, &headers);
    let (status_str, response_body) = if caller_class == SendCallerClass::LoopbackInternal {
        health::handle_send(registry, state.pg_pool_ref(), &body_str).await
    } else {
        crate::services::discord::outbound::send_api::handle_send_with_caller(
            registry,
            state.pg_pool_ref(),
            &body_str,
            caller_class,
        )
        .await
    };
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/discord/bot-tokens/reload — reload announce/notify REST clients.
///
/// Only the `HealthRegistry` utility bots rotate; provider runtime token caches are
/// `OnceCell`s that need a dcserver restart, and the response reports each scope.
/// Auth and `ConnectInfo` requirements as for `send_handler`.
pub async fn reload_discord_bot_tokens_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        ))
        .into_response();
    };

    let report = registry.reload_bot_tokens().await;
    let status = if !report.runtime_root_available {
        "runtime_root_unavailable"
    } else if report.any_reloaded {
        "reloaded"
    } else if report.announce.previous_client_kept || report.notify.previous_client_kept {
        "kept_previous_no_valid_credentials"
    } else {
        "no_valid_credentials_loaded"
    };
    tracing::info!(
        status,
        announce = ?report.announce.status,
        notify = ?report.notify.status,
        utility_bot_user_ids_invalidated = report.utility_bot_user_ids_invalidated,
        "operator-triggered Discord utility bot token reload completed"
    );

    let http_status = if report.runtime_root_available {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        http_status,
        Json(serde_json::json!({
            "ok": report.any_reloaded,
            "status": status,
            "report": report,
        })),
    )
        .into_response()
}

/// POST /api/inflight/rebind — orphan recovery endpoint.
///
/// Rebinds a live tmux session to fresh inflight state and respawns the output watcher,
/// for when a prior turn's cleanup cleared the inflight JSON and left no relay path.
/// Auth and `ConnectInfo` requirements as for `send_handler`.
pub async fn rebind_inflight_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        ))
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
/// Auth and `ConnectInfo` requirements as for `send_handler`.
pub async fn send_to_agent_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        ))
        .into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let (status_str, response_body) =
        crate::services::discord::outbound::send_to_agent::handle_send_to_agent(
            registry,
            state.pg_pool_ref(),
            &body_str,
        )
        .await;
    let status = parse_status_code(status_str);
    let json: serde_json::Value =
        serde_json::from_str(&response_body).unwrap_or(serde_json::json!({"error": "internal"}));
    (status, Json(json)).into_response()
}

/// POST /api/discord/send-dm — send a DM to a Discord user.
///
/// Auth and `ConnectInfo` requirements as for `send_handler`.
pub async fn senddm_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !discord_control_endpoints_allowed(&state.config, Some(peer_addr), &headers) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }

    let Some(ref registry) = state.health_registry else {
        return legacy_health_error(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        ))
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
mod tests;

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
