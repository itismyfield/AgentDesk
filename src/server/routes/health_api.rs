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

fn discord_send_caller_class(
    peer_addr: SocketAddr,
    headers: &HeaderMap,
) -> health::SendCallerClass {
    let source_header = headers.get(X_AGENTDESK_SOURCE);
    let header_class = source_header
        .and_then(|value| value.to_str().ok())
        .and_then(health::SendCallerClass::from_header);
    match header_class {
        Some(health::SendCallerClass::LoopbackInternal) if peer_addr.ip().is_loopback() => {
            health::SendCallerClass::LoopbackInternal
        }
        Some(health::SendCallerClass::LoopbackInternal) => health::SendCallerClass::Unknown,
        Some(class) => class,
        None if source_header.is_some() => health::SendCallerClass::Unknown,
        None if peer_addr.ip().is_loopback() => health::SendCallerClass::LoopbackInternal,
        None => health::SendCallerClass::Unknown,
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
    let (status_str, response_body) = if caller_class == health::SendCallerClass::LoopbackInternal {
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
mod tests {
    use super::{
        RegistryPurgeDecision, discord_control_endpoints_allowed, discord_send_caller_class,
        public_health_json, registry_purge_decision, stale_mailbox_repair_applied,
    };
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, Request, StatusCode, header::AUTHORIZATION},
    };
    use serde_json::json;
    use std::{path::Path, sync::Arc, sync::MutexGuard};
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

    fn source_headers(source: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-agentdesk-source", source.parse().expect("valid source"));
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

    #[test]
    fn discord_send_caller_class_uses_header_when_present() {
        use crate::services::discord::health::SendCallerClass;

        let loopback = "127.0.0.1:8791".parse().unwrap();
        let remote = "10.0.0.5:8791".parse().unwrap();

        assert_eq!(
            discord_send_caller_class(loopback, &source_headers("cli")),
            SendCallerClass::Cli
        );
        assert_eq!(
            discord_send_caller_class(loopback, &source_headers("dashboard")),
            SendCallerClass::Dashboard
        );
        assert_eq!(
            discord_send_caller_class(loopback, &source_headers("not-a-caller-class")),
            SendCallerClass::Unknown
        );
        assert_eq!(
            discord_send_caller_class(remote, &source_headers("internal")),
            SendCallerClass::Unknown
        );
    }

    #[test]
    fn discord_send_caller_class_keeps_headerless_loopback_compatibility() {
        use crate::services::discord::health::SendCallerClass;

        assert_eq!(
            discord_send_caller_class("127.0.0.1:8791".parse().unwrap(), &empty_headers()),
            SendCallerClass::LoopbackInternal
        );
        assert_eq!(
            discord_send_caller_class("10.0.0.5:8791".parse().unwrap(), &empty_headers()),
            SendCallerClass::Unknown
        );
    }

    fn test_api_router_with_config(config: crate::config::Config) -> axum::Router {
        test_api_router_with_config_and_registry(config, None)
    }

    pub(super) fn test_api_router_with_config_and_registry(
        config: crate::config::Config,
        health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    ) -> axum::Router {
        let mut engine_config = crate::config::Config::default();
        engine_config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        crate::server::routes::api_router_with_pg(engine, config, tx, buf, health_registry, None)
    }

    fn control_request(peer: &str) -> Request<Body> {
        control_request_for("/discord/send", peer, r#"{"content":"hello"}"#)
    }

    fn reload_bot_tokens_request(peer: &str) -> Request<Body> {
        control_request_for("/discord/bot-tokens/reload", peer, "")
    }

    fn control_request_for(uri: &str, peer: &str, body: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::from(body.to_string()))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        request
    }

    pub(super) struct EnvVarGuard {
        key: String,
        previous_value: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        pub(super) fn set_path(key: &str, path: &Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous_value = std::env::var_os(key);
            unsafe { std::env::set_var(key, path) };
            Self {
                key: key.to_string(),
                previous_value,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous_value {
                Some(value) => unsafe { std::env::set_var(&self.key, value) },
                None => unsafe { std::env::remove_var(&self.key) },
            }
        }
    }

    fn write_test_bot_token(root: &Path, bot_name: &str, token: &str) {
        crate::runtime_layout::ensure_credential_layout(root).unwrap();
        let path = crate::runtime_layout::credential_token_path(root, bot_name);
        crate::utils::secret_file::write_secret_file(&path, format!("{token}\n"))
            .expect("write test bot token");
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
        // The auth middleware honours `Origin: localhost` only from a loopback peer, so a
        // forged same-origin header from the LAN is rejected with 401.
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
    async fn discord_send_router_applies_explicit_cli_source_gate_on_loopback() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        let app = test_api_router_with_config_and_registry(config, Some(registry));

        let mut request = control_request_for(
            "/discord/send",
            "127.0.0.1:8791",
            r#"{"target":"channel:999999999999999999","content":"hello","source":"system"}"#,
        );
        request
            .headers_mut()
            .insert("x-agentdesk-source", "cli".parse().unwrap());

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "source not allowed for this caller");
    }

    #[tokio::test]
    async fn discord_control_router_rejects_non_loopback_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let app = test_api_router_with_config(config);

        let response = app.oneshot(control_request("10.0.0.5:8791")).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn discord_bot_token_reload_router_rejects_non_loopback_auth_token_without_bearer() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.auth_token = Some("secret".to_string());
        let app = test_api_router_with_config(config);

        let response = app
            .oneshot(reload_bot_tokens_request("10.0.0.5:8791"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn discord_bot_token_reload_router_rejects_non_loopback_without_auth_token() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let app = test_api_router_with_config(config);

        let response = app
            .oneshot(reload_bot_tokens_request("10.0.0.5:8791"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn discord_bot_token_reload_router_reports_success_without_exposing_tokens() {
        let runtime_root = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        write_test_bot_token(runtime_root.path(), "announce", "announce-token");
        write_test_bot_token(runtime_root.path(), "notify", "notify-token");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let mut config = crate::config::Config::default();
            config.server.host = "0.0.0.0".to_string();
            let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
            let app = test_api_router_with_config_and_registry(config, Some(registry));

            let response = app
                .oneshot(reload_bot_tokens_request("127.0.0.1:8791"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(body["ok"], true);
            assert_eq!(body["status"], "reloaded");
            assert_eq!(body["report"]["announce"]["status"], "reloaded");
            assert_eq!(body["report"]["notify"]["status"], "reloaded");
            assert_eq!(
                body["report"]["scopes"]["utility_rest_clients"]["status"],
                "reload_supported"
            );
            assert_eq!(
                body["report"]["scopes"]["utility_rest_clients"]["restart_required"],
                false
            );
            assert_eq!(
                body["report"]["scopes"]["provider_runtime_cached_token"]["status"],
                "restart_required"
            );
            assert_eq!(
                body["report"]["scopes"]["provider_runtime_cached_token"]["restart_required"],
                true
            );
            assert_eq!(
                body["report"]["scopes"]["provider_gateway_session"]["status"],
                "restart_required"
            );
            assert_eq!(
                body["report"]["scopes"]["provider_gateway_session"]["restart_required"],
                true
            );
            assert_eq!(
                body["report"]["provider_cached_bot_token_scope"],
                "announce/notify HealthRegistry clients are reloaded; provider runtime SharedData.cached_bot_token is restart-only"
            );
            assert!(!String::from_utf8_lossy(&bytes).contains("announce-token"));
            assert!(!String::from_utf8_lossy(&bytes).contains("notify-token"));
        });
    }

    #[test]
    fn discord_bot_token_reload_router_reports_missing_or_invalid_credentials() {
        let runtime_root = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        crate::runtime_layout::ensure_credential_layout(runtime_root.path()).unwrap();
        let notify_path =
            crate::runtime_layout::credential_token_path(runtime_root.path(), "notify");
        crate::utils::secret_file::write_secret_file(&notify_path, "   \n")
            .expect("write invalid notify token");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let mut config = crate::config::Config::default();
            config.server.host = "0.0.0.0".to_string();
            let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
            let app = test_api_router_with_config_and_registry(config, Some(registry));

            let response = app
                .oneshot(reload_bot_tokens_request("127.0.0.1:8791"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(body["ok"], false);
            assert_eq!(body["status"], "no_valid_credentials_loaded");
            assert_eq!(body["report"]["announce"]["status"], "missing_or_invalid");
            assert_eq!(body["report"]["announce"]["previous_client_kept"], false);
            assert_eq!(body["report"]["notify"]["status"], "missing_or_invalid");
            assert_eq!(body["report"]["notify"]["previous_client_kept"], false);
            assert_eq!(
                body["report"]["scopes"]["provider_runtime_cached_token"]["restart_required"],
                true
            );
            let response_text = String::from_utf8_lossy(&bytes);
            assert!(!response_text.contains("announce-token"));
            assert!(!response_text.contains("notify-token"));
        });
    }

    #[tokio::test]
    async fn health_detail_reports_provider_runtime_bot_token_restart_required() {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        let app = test_api_router_with_config_and_registry(config, Some(registry));

        let mut request = Request::builder()
            .method("GET")
            .uri("/health/detail")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            body["bot_token_reload_scopes"]["provider_runtime_cached_token"]["status"],
            "restart_required"
        );
        assert_eq!(
            body["bot_token_reload_scopes"]["provider_runtime_cached_token"]["restart_required"],
            true
        );
        assert_eq!(
            body["bot_token_reload_scopes"]["provider_gateway_session"]["restart_required"],
            true
        );
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
            "degraded_reasons": ["provider:codex:pending_queue_depth:2"],
        }));

        assert_eq!(public["status"], "degraded");
        assert_eq!(public["version"], "0.1.2");
        assert_eq!(public["db"], true);
        assert_eq!(public["dashboard"], false);
        assert_eq!(public["degraded"], true);
        assert!(public.get("providers").is_none());
        assert!(public.get("mailboxes").is_none());
        assert!(public.get("config_audit").is_none());
        // Known-provider reasons are carried verbatim.
        assert_eq!(
            public["degraded_reasons"],
            json!(["provider:codex:pending_queue_depth:2"])
        );
    }

    #[test]
    fn public_health_json_degraded_iff_reasons_nonempty() {
        // degraded => reasons present AND non-empty.
        let degraded = public_health_json(json!({
            "status": "degraded",
            "version": "0.1.2",
            "db": false,
            "dashboard": true,
            "server_up": true,
            "degraded_reasons": ["db_unavailable", "provider:claude:pending_queue_depth:1"],
        }));
        assert_eq!(degraded["degraded"], json!(true));
        let degraded_reasons = degraded["degraded_reasons"]
            .as_array()
            .expect("degraded_reasons is an array on public health");
        assert!(
            !degraded_reasons.is_empty(),
            "degraded health must carry non-empty degraded_reasons"
        );
        assert_eq!(
            degraded["degraded_reasons"],
            json!(["db_unavailable", "provider:claude:pending_queue_depth:1"])
        );

        // healthy => reasons present but EMPTY, and degraded is false.
        let healthy = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "degraded_reasons": [],
        }));
        assert_eq!(healthy["degraded"], json!(false));
        assert_eq!(healthy["degraded_reasons"], json!([]));

        // absent upstream array => still present as [].
        let absent = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
        }));
        assert_eq!(absent["degraded"], json!(false));
        assert_eq!(absent["degraded_reasons"], json!([]));
    }

    /// `degraded_reasons` decides `degraded`; `startup_degraded_reasons` does not, so the
    /// two must stay distinct public fields.
    #[test]
    fn public_health_json_keeps_degraded_and_startup_reasons_distinct() {
        let public = public_health_json(json!({
            "status": "degraded",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "degraded_reasons": ["provider:codex:disconnected"],
            "startup_degraded": true,
            "startup_degraded_reasons": ["startup_doctor:disk_check:warned"],
        }));
        assert_eq!(public["degraded"], json!(true));
        assert_eq!(
            public["degraded_reasons"],
            json!(["provider:codex:disconnected"])
        );
        assert_eq!(
            public["startup_degraded_reasons"],
            json!(["startup_doctor:disk_check:warned"])
        );
        assert_ne!(
            public["degraded_reasons"],
            public["startup_degraded_reasons"]
        );
    }

    /// No part of an operator-chosen provider id, colons included, may reach the public
    /// body; known providers pass through verbatim.
    #[test]
    fn public_health_json_sanitizes_provider_ids_including_colons() {
        // (raw provider name, reason suffix, sensitive tokens that must NOT
        //  appear anywhere in public, expected sanitized public element).
        let cases: &[(&str, &str, &[&str], &str)] = &[
            // simple unknown id
            (
                "prod-mini-01",
                "disconnected",
                &["prod-mini-01"],
                "provider:unsupported:disconnected",
            ),
            // 1. colon IN the name — the tail (`customerA`) must not survive
            (
                "prod-mini-01:customerA",
                "disconnected",
                &["prod-mini-01", "customerA"],
                "provider:unsupported:disconnected",
            ),
            // 2. many colons
            (
                "a:b:c",
                "restart_pending",
                &["a:b:c"],
                "provider:unsupported:restart_pending",
            ),
            // 3. empty name
            (
                "",
                "reconcile_in_progress",
                &[],
                "provider:unsupported:reconcile_in_progress",
            ),
            // 4. unicode + control chars in the name
            (
                "기밀-🤖\u{7}\u{202e}HOSTSECRET",
                "disconnected",
                &["기밀", "🤖", "HOSTSECRET"],
                "provider:unsupported:disconnected",
            ),
            // colon-in-name with a COUNTED reason — only the digit count survives
            (
                "evil:host",
                "pending_queue_depth:7",
                &["evil", "host"],
                "provider:unsupported:pending_queue_depth:7",
            ),
            // left-anchor bypass attempt: a real id used as a name prefix
            (
                "codex:leak",
                "disconnected",
                &["leak"],
                "provider:unsupported:disconnected",
            ),
            // unknown reason keyword -> fail closed, name AND reason dropped
            (
                "secret-host",
                "totally_new_reason",
                &["secret-host", "totally_new_reason"],
                "provider:unsupported",
            ),
        ];

        for (name, reason, sensitive, expected) in cases {
            let raw = format!("provider:{name}:{reason}");
            let full = json!({
                "status": "unhealthy", "version": "0.1.2", "db": true,
                "dashboard": true, "server_up": true,
                "degraded_reasons": [raw.clone()],
            });
            // detail/raw snapshot keeps the value verbatim (authenticated path).
            assert_eq!(
                full["degraded_reasons"],
                json!([raw]),
                "detail/raw must retain verbatim reason for {raw:?}"
            );

            let public = public_health_json(full);
            let text = public.to_string();
            for tok in *sensitive {
                assert!(
                    !text.contains(tok),
                    "sensitive token {tok:?} leaked for raw {raw:?}: {text}"
                );
            }
            assert_eq!(
                public["degraded_reasons"],
                json!([expected]),
                "wrong sanitized value for raw {raw:?}"
            );
            assert_eq!(public["degraded"], json!(true));
        }

        // 5. supported providers are FULLY preserved (including numeric tails)
        // even when mixed with a sanitized unknown id and a non-provider reason.
        let mixed = public_health_json(json!({
            "status": "unhealthy", "version": "0.1.2", "db": false,
            "dashboard": true, "server_up": true,
            "degraded_reasons": [
                "provider:codex:pending_queue_depth:2",
                "provider:claude:disconnected",
                "provider:gemini:recovering_channels:5",
                "provider:prod-mini-01:customerA:disconnected",
                "db_unavailable"
            ],
        }));
        assert_eq!(
            mixed["degraded_reasons"],
            json!([
                "provider:codex:pending_queue_depth:2",
                "provider:claude:disconnected",
                "provider:gemini:recovering_channels:5",
                "provider:unsupported:disconnected",
                "db_unavailable"
            ])
        );
        let mixed_text = mixed.to_string();
        assert!(!mixed_text.contains("prod-mini-01"));
        assert!(!mixed_text.contains("customerA"));
    }

    /// Operators need these keywords publicly to tell a standby node from a wedged reconcile.
    #[test]
    fn public_health_json_preserves_standby_and_stalled_reconcile_reasons() {
        let public = public_health_json(json!({
            "status": "degraded", "version": "0.1.2", "db": true,
            "dashboard": true, "server_up": true,
            "degraded_reasons": [
                "provider:codex:gateway_standby",
                "provider:codex:reconcile_stalled",
                "provider:prod-mini-01:reconcile_stalled",
                "gateway_standby",
            ],
        }));

        assert_eq!(
            public["degraded_reasons"],
            json!([
                "provider:codex:gateway_standby",
                "provider:codex:reconcile_stalled",
                // Unknown provider id is still replaced wholesale.
                "provider:unsupported:reconcile_stalled",
                // Cluster-level reason: no `provider:` prefix, passed through.
                "gateway_standby",
            ])
        );
        assert!(!public.to_string().contains("prod-mini-01"));
    }

    /// The right-anchored sanitizer trusts registry ids only because they never contain `:`.
    #[test]
    fn supported_provider_ids_contain_no_colon() {
        for id in crate::services::provider::supported_provider_ids() {
            assert!(
                !id.contains(':'),
                "supported provider id {id:?} contains ':'"
            );
        }
    }

    #[test]
    fn public_health_json_omits_opencode_warm_server_array() {
        let public = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "opencode": {
                "warm_server_count": 2,
                "warm_server_active_sessions": 3,
                "warm_server_suspicious_count": 1,
                "warm_servers": [
                    {"pid": 12345, "port": 54321, "key_hash": "abcdef0123456789",
                     "startup_output_tail": "secret leak", "base_url": "http://127.0.0.1:54321"}
                ]
            }
        }));

        let opencode = public.get("opencode").expect("opencode summary present");
        assert_eq!(opencode["warm_server_count"], 2);
        assert_eq!(opencode["warm_server_active_sessions"], 3);
        assert_eq!(opencode["warm_server_suspicious_count"], 1);
        assert!(opencode.get("warm_servers").is_none());
        let text = public.to_string();
        assert!(!text.contains("12345"));
        assert!(!text.contains("54321"));
        assert!(!text.contains("startup_output_tail"));
        assert!(!text.contains("base_url"));
        assert!(!text.contains("abcdef0123456789"));
    }

    /// A warm-pool reason degrades status, and the projection must turn that into `ok: false`.
    #[test]
    fn public_health_json_degraded_status_reports_not_ok() {
        let public = public_health_json(json!({
            "status": "degraded",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "degraded_reasons": ["opencode_warm_server:stopped_resident:1"],
        }));
        assert_eq!(public["status"], json!("degraded"));
        assert_eq!(public["ok"], json!(false));
        assert_eq!(public["degraded"], json!(true));
    }

    /// Pins the `public_health_json` hop, which snapshot-level tests cannot see; the
    /// reasons are the exact shape the relay polarity pass emits.
    #[test]
    fn public_health_json_carries_the_relay_verdict_axis_onto_the_summary() {
        let detail = json!({
            "status": "degraded",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "fully_recovered": true,
            "degraded_reasons": [
                "relay_verdict_unknown_codex_1479671301387059200",
                "relay_verdict_degraded_claude_1479671298497183835",
            ],
        });
        let public = public_health_json(detail.clone());
        assert_eq!(
            public["status"], detail["status"],
            "the summary must report the status the detail build computed"
        );
        assert_eq!(
            public["degraded_reasons"], detail["degraded_reasons"],
            "the relay-verdict reasons must reach the unauthenticated body intact"
        );
        assert_eq!(public["ok"], json!(false));
        assert_eq!(public["degraded"], json!(true));
    }

    /// Pins the allowlist hop for `expired_relay_ledgers`, which snapshot-level tests
    /// cannot see.
    #[test]
    fn public_health_json_carries_the_expired_relay_ledgers_onto_the_summary() {
        let detail = json!({
            "status": "degraded",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "fully_recovered": true,
            "degraded_reasons": ["relay_verdict_unknown_codex_1479671301387059200"],
            "expired_relay_ledgers": [
                "relay_verdict_expired_claude_1479671298497183835",
                "relay_verdict_expired_codex_1479671299000000000",
            ],
        });
        let public = public_health_json(detail.clone());
        assert_eq!(
            public["expired_relay_ledgers"], detail["expired_relay_ledgers"],
            "the expired-ledger vector must reach the unauthenticated body intact"
        );
        assert_eq!(
            public["degraded_reasons"], detail["degraded_reasons"],
            "an expiry must not be laundered into the degraded axis on the way out"
        );
    }

    #[test]
    fn public_expired_relay_ledgers_never_move_ok_and_default_to_an_empty_vector() {
        let expired_only = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "fully_recovered": true,
            "degraded_reasons": [],
            "expired_relay_ledgers": ["relay_verdict_expired_codex_1479671301387059200"],
        }));
        assert_eq!(expired_only["ok"], json!(true));
        assert_eq!(expired_only["degraded"], json!(false));
        assert_eq!(
            expired_only["expired_relay_ledgers"],
            json!(["relay_verdict_expired_codex_1479671301387059200"])
        );

        let absent = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
        }));
        assert_eq!(
            absent["expired_relay_ledgers"],
            json!([]),
            "the key is always present so a counting reader never has to branch on absence"
        );
    }

    /// Both URLs off one registry. The fixture has no expired ledgers, so this pins that
    /// the summary keeps the key, not that the contents agree.
    #[test]
    fn summary_and_detail_routes_agree_on_expired_relay_ledgers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = Some(Arc::new(
            crate::services::discord::health::HealthRegistry::new(),
        ));

        let public = runtime.block_on(health_body("/health", registry.clone()));
        let detail = runtime.block_on(health_body("/health/detail", registry));

        assert!(
            detail["expired_relay_ledgers"].is_array(),
            "the detail route must publish the vector, got {}",
            detail["expired_relay_ledgers"]
        );
        assert!(
            public["expired_relay_ledgers"].is_array(),
            "the summary route must publish the vector, got {}",
            public["expired_relay_ledgers"]
        );
        assert_eq!(
            public["expired_relay_ledgers"], detail["expired_relay_ledgers"],
            "the summary route must carry the detail route's expired ledgers"
        );
    }

    /// Both URLs off one registry; the relay axis itself is covered in `health::snapshot`,
    /// which can reach registry internals.
    #[test]
    fn summary_and_detail_routes_agree_on_status_and_degraded_reasons() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let registry = Some(Arc::new(
            crate::services::discord::health::HealthRegistry::new(),
        ));

        let public = runtime.block_on(health_body("/health", registry.clone()));
        let detail = runtime.block_on(health_body("/health/detail", registry));

        assert_eq!(
            public["status"], detail["status"],
            "the summary route must not report a healthier status than the detail route"
        );
        assert_eq!(
            public["degraded_reasons"], detail["degraded_reasons"],
            "the summary route must carry the detail route's degraded reasons"
        );
        assert!(
            !detail["degraded_reasons"]
                .as_array()
                .expect("detail degraded_reasons is an array")
                .is_empty(),
            "the fixture must produce at least one reason, or the equality above is vacuous"
        );
        assert_eq!(public["ok"], json!(public["status"] == json!("healthy")));
    }

    #[test]
    fn public_health_json_preserves_delivery_record_rollout_state() {
        let public = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "delivery_record_rollout": {
                "shadow_enabled": false,
                "authority_enabled": false,
                "mode": "off",
                "dedup_authority": "in_memory_committed_offset",
                "same_turn_backward_write_enforcement": "observe_only",
                "flag_source": {
                    "shadow": "compiled_default",
                    "authority": "env_override"
                },
                "warning_count": 1,
                "configuration_warnings": [
                    "delivery_record_authority_disabled: durable frontiers are not the default committed-offset authority"
                ]
            }
        }));
        assert_eq!(public["delivery_record_rollout"]["mode"], json!("off"));
        assert_eq!(public["delivery_record_rollout"]["warning_count"], json!(1));
        assert_eq!(
            public["delivery_record_rollout"]["flag_source"],
            json!({"shadow": "compiled_default", "authority": "env_override"})
        );
        assert_eq!(public["ok"], json!(true));
    }

    #[test]
    fn public_health_json_preserves_intake_routing_state() {
        let public = public_health_json(json!({
            "status": "healthy",
            "version": "0.1.2",
            "db": true,
            "dashboard": true,
            "server_up": true,
            "intake_routing": {
                "enabled": true,
                "mode": "observe",
                "source": "yaml",
                "owner_authority_allowlist_size": 2,
                "recent_decision_count": 17,
                "yaml": {
                    "enabled": true,
                    "mode": "observe",
                    "owner_authority_allowlist_size": 2,
                    "forward_pre_claim_timeout_secs": 12,
                    "stale_claim_recovery_secs": 60
                },
                "env_override": null,
                "warning_count": 0,
                "configuration_warnings": []
            }
        }));
        assert_eq!(public["intake_routing"]["enabled"], json!(true));
        assert_eq!(public["intake_routing"]["mode"], json!("observe"));
        assert_eq!(public["intake_routing"]["source"], json!("yaml"));
        assert_eq!(
            public["intake_routing"]["owner_authority_allowlist_size"],
            json!(2)
        );
        assert_eq!(public["intake_routing"]["recent_decision_count"], json!(17));
        assert_eq!(public["intake_routing"]["warning_count"], json!(0));
        assert_eq!(public["ok"], json!(true));
    }

    /// Build a `/health` or `/health/detail` request from a loopback peer.
    fn health_get(uri: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("valid health request");
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:8791"
                .parse::<std::net::SocketAddr>()
                .expect("loopback peer"),
        ));
        request
    }

    /// Serve one health request on a fresh router and return the decoded body.
    /// `Some(registry)` runs the registry arm of `health_response`, `None` the standalone arm.
    pub(super) async fn health_body(
        uri: &str,
        registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    ) -> serde_json::Value {
        let mut config = crate::config::Config::default();
        config.server.host = "0.0.0.0".to_string();
        let app = test_api_router_with_config_and_registry(config, registry);
        let response = app.oneshot(health_get(uri)).await.expect("health response");
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("health body");
        serde_json::from_slice(&bytes).expect("health body is JSON")
    }

    #[test]
    fn public_health_reads_release_source_manifest_from_runtime_root() {
        let runtime_root = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        let runtime_dir = runtime_root.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
        std::fs::write(
            runtime_dir.join("release-source.json"),
            r#"{"generated_at":"2026-08-12T00:00:00Z","repo_head":"0123456789abcdef0123456789abcdef01234567","latest_postgres_migration":"0104_example.sql","repo_dirty":"false"}"#,
        )
        .expect("write release source manifest");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        for registry in [
            None,
            Some(Arc::new(
                crate::services::discord::health::HealthRegistry::new(),
            )),
        ] {
            let body = runtime.block_on(health_body("/health", registry));
            assert_eq!(body["release_source"]["observation_status"], "observed");
            assert_eq!(
                body["release_source"]["generated_at"],
                "2026-08-12T00:00:00Z"
            );
            assert_eq!(
                body["release_source"]["deployed_repo_head"],
                "0123456789abcdef0123456789abcdef01234567"
            );
            assert_eq!(
                body["release_source"]["deployed_latest_postgres_migration"],
                "0104_example.sql"
            );
            // The checkout-cleanliness verdict rides the same public entry as its head.
            assert_eq!(body["release_source"]["deployed_repo_dirty"], "false");
            assert!(body["release_source"].get("node_hostname").is_none());
        }

        let detail = runtime.block_on(health_body("/health/detail", None));
        assert!(detail["release_source"]["node_hostname"].is_string());
    }

    #[test]
    fn public_health_exposes_rollout_flag_source_on_both_assembly_points() {
        // Peer-rollout comparison reads the PUBLIC endpoint, so provenance must survive the
        // allowlist; registry and standalone assembly are separate branches, so check both.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let expected = crate::services::discord::outbound::delivery_record_rollout_health_json()
            ["flag_source"]
            .clone();
        assert!(
            expected["shadow"].is_string() && expected["authority"].is_string(),
            "both rollout axes must report a provenance"
        );

        for registry in [
            None,
            Some(Arc::new(
                crate::services::discord::health::HealthRegistry::new(),
            )),
        ] {
            let assembly = if registry.is_some() {
                "registry"
            } else {
                "standalone"
            };
            for path in ["/health", "/health/detail"] {
                let body = runtime.block_on(health_body(path, registry.clone()));
                assert_eq!(
                    body["delivery_record_rollout"]["flag_source"], expected,
                    "{path} must carry rollout provenance on the {assembly} assembly branch"
                );
                if path == "/health" {
                    assert!(
                        body["release_source"].get("node_hostname").is_none(),
                        "{assembly} public health must keep node_hostname detail-only"
                    );
                }
            }
        }
    }

    #[test]
    fn public_health_keeps_each_release_source_fact_independent() {
        let runtime_root = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        let runtime_dir = runtime_root.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
        let manifest = runtime_dir.join("release-source.json");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        std::fs::write(
            &manifest,
            r#"{"generated_at":"2026-08-12T00:00:00Z","repo_head":"unknown","latest_postgres_migration":"0104_example.sql","repo_dirty":"true"}"#,
        )
        .expect("write release source manifest");
        let body = runtime.block_on(health_body("/health", None));
        assert_eq!(body["release_source"]["observation_status"], "partial");
        assert_eq!(
            body["release_source"]["observation_failures"],
            serde_json::json!(["repo_head_invalid"])
        );
        // A dirty checkout is still an OBSERVED fact; only the head failed here.
        assert_eq!(body["release_source"]["deployed_repo_dirty"], "true");
        assert_eq!(
            body["release_source"]["generated_at"],
            "2026-08-12T00:00:00Z"
        );
        assert!(body["release_source"].get("deployed_repo_head").is_none());
        assert_eq!(
            body["release_source"]["deployed_latest_postgres_migration"],
            "0104_example.sql"
        );

        std::fs::write(
            &manifest,
            r#"{"repo_head":"0123456789abcdef0123456789abcdef01234567"}"#,
        )
        .expect("write release source manifest");
        let body = runtime.block_on(health_body("/health/detail", None));
        assert_eq!(body["release_source"]["observation_status"], "partial");
        assert_eq!(
            body["release_source"]["observation_failures"],
            serde_json::json!(["latest_postgres_migration_missing", "repo_dirty_missing"])
        );
        assert_eq!(
            body["release_source"]["deployed_repo_head"],
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert!(
            body["release_source"]
                .get("deployed_latest_postgres_migration")
                .is_none()
        );

        std::fs::write(manifest, r#"{}"#).expect("write release source manifest");
        let body = runtime.block_on(health_body("/health", None));
        assert_eq!(body["release_source"]["observation_status"], "unobserved");
        assert_eq!(
            body["release_source"]["observation_failures"],
            serde_json::json!([
                "repo_head_missing",
                "latest_postgres_migration_missing",
                "repo_dirty_missing"
            ])
        );
    }

    #[test]
    fn public_health_exposes_release_source_read_failure() {
        let runtime_root = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let body = runtime.block_on(health_body("/health", None));

        assert_eq!(body["release_source"]["observation_status"], "unobserved");
        assert_eq!(
            body["release_source"]["observation_failures"],
            serde_json::json!(["manifest_missing"])
        );
        assert!(body["release_source"].get("deployed_repo_head").is_none());
    }

    fn injected_backlog(
        pending: i64,
        dead_lettered: i64,
    ) -> crate::services::health_diagnostics::backlog_probe::InjectedBacklog {
        crate::services::health_diagnostics::backlog_probe::inject(
            crate::services::health_diagnostics::AutoQueueCleanupBacklog {
                pending,
                dead_lettered,
            },
        )
    }

    /// Injects at the loader and reads the served body, so an unreached loader or a
    /// reshape after the projection is caught.
    #[tokio::test]
    async fn public_auto_queue_cleanup_backlog_is_served_on_the_unauthenticated_endpoint() {
        let _injected = injected_backlog(4, 5);
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());

        let public = health_body("/health", Some(registry)).await;

        assert_eq!(
            public["auto_queue_cleanup"]["dead_lettered"],
            json!(5),
            "the permanently parked cleanup count must be readable without a \
             credential; it is the only standing signal that a slot token and a \
             provider session are stranded: {public}"
        );
        assert_eq!(public["auto_queue_cleanup"]["pending"], json!(4));
        assert_eq!(
            public["outbox_age"],
            serde_json::Value::Null,
            "precondition: the public shape carries no outbox age, so nothing \
             above may gate the cleanup block on one"
        );
        assert!(
            public.get("dispatch_outbox").is_none(),
            "and this is not a licence to publish the unrelated dispatch outbox \
             block, which does have a doctor check and an ack endpoint behind \
             it: {public}"
        );
    }

    /// The standalone arm builds its JSON from scratch, so it is a second attachment site.
    #[tokio::test]
    async fn auto_queue_cleanup_backlog_is_served_from_both_health_response_arms() {
        let _injected = injected_backlog(1, 2);

        for registry in [
            Some(Arc::new(
                crate::services::discord::health::HealthRegistry::new(),
            )),
            None,
        ] {
            let detail = health_body("/health/detail", registry.clone()).await;
            assert_eq!(
                detail["auto_queue_cleanup"]["pending"],
                json!(1),
                "detail payload lost the cleanup backlog (registry present: {}): {detail}",
                registry.is_some()
            );
            assert_eq!(detail["auto_queue_cleanup"]["dead_lettered"], json!(2));

            let public = health_body("/health", registry.clone()).await;
            assert_eq!(
                public["auto_queue_cleanup"]["dead_lettered"],
                json!(2),
                "public payload lost the cleanup backlog (registry present: {}): {public}",
                registry.is_some()
            );
        }
    }

    /// `health::snapshot`'s retirement test cannot see the standalone arm, which builds
    /// its JSON from scratch.
    #[tokio::test]
    async fn retired_observation_blocks_are_absent_from_both_health_response_arms() {
        for registry in [
            Some(Arc::new(
                crate::services::discord::health::HealthRegistry::new(),
            )),
            None,
        ] {
            let detail = health_body("/health/detail", registry.clone()).await;
            for key in [
                "relay_authority_observation",
                "relay_authority_rollout",
                "axis_b_observation",
            ] {
                assert!(
                    detail.get(key).is_none(),
                    "/health/detail published retired key {key} (registry present: {}): {detail}",
                    registry.is_some()
                );
            }
        }
    }

    #[tokio::test]
    async fn public_auto_queue_cleanup_backlog_does_not_move_ok_or_degraded_reasons() {
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());

        let quiet = {
            let _injected = injected_backlog(0, 0);
            health_body("/health", Some(registry.clone())).await
        };
        let parked = {
            let _injected = injected_backlog(9, 9);
            health_body("/health", Some(registry.clone())).await
        };

        assert_eq!(parked["auto_queue_cleanup"]["dead_lettered"], json!(9));
        assert_eq!(quiet["auto_queue_cleanup"]["dead_lettered"], json!(0));
        for field in ["ok", "status", "degraded", "degraded_reasons"] {
            assert_eq!(
                parked[field], quiet[field],
                "a parked cleanup backlog must not move `{field}`: a gauge that \
                 fails a deploy gate is a different change with different risk"
            );
        }
        assert!(
            parked["degraded_reasons"]
                .as_array()
                .expect("degraded_reasons is always an array on the public shape")
                .iter()
                .all(|reason| reason.as_str() != Some("auto_queue_cleanup_dead_lettered")),
            "no cleanup reason may appear on the axis that decides `ok`: {parked}"
        );
    }

    #[test]
    fn public_auto_queue_cleanup_projection_is_count_only() {
        let public = public_health_json(json!({
            "status": "healthy",
            "auto_queue_cleanup": {
                "pending": 4,
                "dead_lettered": 5,
                "oldest_dead_lettered_run_id": "run-secret",
                "last_error": "connection string leaked here",
            }
        }));

        assert_eq!(public["auto_queue_cleanup"]["pending"], json!(4));
        assert_eq!(public["auto_queue_cleanup"]["dead_lettered"], json!(5));
        let block = public["auto_queue_cleanup"]
            .as_object()
            .expect("the public cleanup block is an object");
        assert_eq!(
            block.len(),
            2,
            "count-only: exactly `pending` and `dead_lettered` cross to the \
             unauthenticated shape, and a future detail field does not tag \
             along: {public}"
        );
    }

    #[test]
    fn stale_mailbox_repair_applied_includes_session_only_disconnect() {
        assert!(stale_mailbox_repair_applied(false, false, 1));
        assert!(stale_mailbox_repair_applied(true, false, 0));
        assert!(stale_mailbox_repair_applied(false, true, 0));
        assert!(!stale_mailbox_repair_applied(false, false, 0));
    }

    /// A partial repair's skip reason surfaces as `registry_purge_skipped_reason`.
    #[test]
    fn registry_purge_decision_gates_on_request_and_fully_applied_repair() {
        assert_eq!(
            registry_purge_decision(false, "applied"),
            RegistryPurgeDecision::NotRequested
        );
        assert_eq!(
            registry_purge_decision(false, "partial_repair"),
            RegistryPurgeDecision::NotRequested
        );
        assert_eq!(
            registry_purge_decision(true, "applied"),
            RegistryPurgeDecision::Run
        );
        assert_eq!(
            registry_purge_decision(true, "partial_repair"),
            RegistryPurgeDecision::Skip("repair_not_fully_applied")
        );
    }

    #[test]
    fn compute_fully_recovered_preserves_recovery_axis_when_runtime_degrades() {
        use super::compute_fully_recovered;
        use crate::services::discord::health;

        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Healthy,
            &[]
        ));

        let reasons_db = vec![json!("db_unavailable")];
        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Healthy,
            &reasons_db
        ));

        let reasons_outbox_disk = vec![
            json!("dispatch_outbox_oldest_pending_age:120"),
            json!("disk_low_free_bytes:104857600"),
        ];
        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Degraded,
            &reasons_outbox_disk
        ));

        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Unhealthy,
            &[]
        ));

        assert!(!compute_fully_recovered(
            false,
            health::HealthStatus::Healthy,
            &[]
        ));
    }
    #[tokio::test]
    async fn legacy_body_census_detail_exposes_cached_file_evidence() {
        use crate::services::legacy_body_census::{
            Collector, ProjectionCache, evaluate, with_projection_for_test,
        };
        use serde_json::json;
        const START: i64 = 60_000;
        const CUT: i64 = 240_000;
        const BUILD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const CENSUS: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        fn fixture(root: &std::path::Path, host: &str, case: &str) -> String {
            let directory = root.join("legacy_body_census");
            let doctor = root.join("runtime/doctor/startup");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::create_dir_all(&doctor).unwrap();
            std::fs::File::create(directory.join("writer.lock")).unwrap();
            let ids: Vec<_> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();
            let offset = i64::from(case == "late_epoch");
            let anchor = json!({"boot_id":ids[1],"started_at_ms":START + offset});
            let last = if case == "no_restart" { 1 } else { 2 };
            for i in 0..=last {
                let started = if case == "same_time" && i == 2 {
                    START
                } else {
                    i as i64 * START + if i > 0 { offset } else { 0 }
                };
                let doc = format!("{}-{}", i + 1, i * 60);
                let mut header = json!({"kind":"header","schema_version":1,"observation_policy_version":1,
                    "host":host,"process_role":"server","boot_id":ids[i],"doctor_boot_id":doc,
                    "predecessor_boot_id":i.checked_sub(1).map(|n| &ids[n]),"started_at_ms":started,
                    "coverage_start":if i==0 {json!({"boot_id":ids[0],"started_at_ms":0})} else {anchor.clone()},
                    "build_sha":BUILD,"census_hash":CENSUS,"probe_coverage":true,"pending_probes":0,
                    "static_instrumentation":{"schema_version":1,"complete":true,"manifest_error":null,"pending":0,"census_hash":CENSUS}});
                if case == "artifact_absent" {
                    header["probe_coverage"] = json!(false);
                }
                if case == "historic_static_missing" && i == 1 {
                    header
                        .as_object_mut()
                        .unwrap()
                        .remove("static_instrumentation");
                }
                if i == last {
                    match case {
                        "static_missing" => {
                            header
                                .as_object_mut()
                                .unwrap()
                                .remove("static_instrumentation");
                        }
                        "static_false" => {
                            header["static_instrumentation"]["complete"] = json!(false)
                        }
                        "static_malformed" => header["static_instrumentation"] = json!(true),
                        "static_error" => {
                            header["static_instrumentation"]["manifest_error"] =
                                json!("manifest_invalid")
                        }
                        "static_hash" => {
                            header["static_instrumentation"]["census_hash"] = json!("c".repeat(64))
                        }
                        "static_pending" => header["static_instrumentation"]["pending"] = json!(1),
                        "static_error_missing" => {
                            header["static_instrumentation"]
                                .as_object_mut()
                                .unwrap()
                                .remove("manifest_error");
                        }
                        "static_schema" => {
                            header["static_instrumentation"]["schema_version"] = json!(2)
                        }
                        _ => {}
                    }
                }
                if case == "old_uncovered" && i == 0 {
                    header["probe_coverage"] = json!(false);
                    header["pending_probes"] = json!(7);
                }
                if case == "old_schema" && i == 0 {
                    header["schema_version"] = json!(2);
                }
                if case == "wrong_census" && i == 2 {
                    header["census_hash"] = json!("d".repeat(64));
                }
                if case == "wrong_build" && i == 2 {
                    header["build_sha"] = json!("d".repeat(40));
                }
                let selection = json!({"kind":"selection","installed":true,"tui_bindings":[1],"effective":[1],"dormant":[],"selection_source":"explicit_all_tui"});
                let mut records = vec![json!({"kind":"boot_ready"}), selection];
                if case == "selection_off" && i == 2 {
                    records[1]["effective"] = json!([]);
                }
                let no_bindings = case == "no_bindings" || case.starts_with("na_");
                if case == "selection_disabled" || no_bindings {
                    records[1]["installed"] = json!(false);
                    records[1]["effective"] = serde_json::Value::Null;
                    records[1]["dormant"] = serde_json::Value::Null;
                    if no_bindings && !(case == "na_history_binding" && i == 1) {
                        records[1]["tui_bindings"] = json!([]);
                    }
                }
                if case == "na_history_binding" && i == 1 {
                    records[1]["installed"] = json!(true);
                    records[1]["effective"] = json!([1]);
                    records[1]["dormant"] = json!([]);
                }
                if case == "na_unknown" && i == last {
                    records[1]["installed"] = serde_json::Value::Null;
                }
                if case == "selection_unknown" && i == 2 {
                    records[1]["installed"] = serde_json::Value::Null;
                }
                if i == last
                    && matches!(
                        case,
                        "na_changed_selection" | "na_earlier_unknown_selection"
                    )
                {
                    records.insert(1, if case == "na_changed_selection" {
                        json!({"kind":"selection","installed":true,"tui_bindings":[1],"effective":[1],"dormant":[],"selection_source":"explicit_all_tui"})
                    } else {
                        json!({"kind":"selection","installed":null,"tui_bindings":null})
                    });
                }
                if i == last && case != "denominator_missing" {
                    let turns = if no_bindings && case != "na_positive_counters" {
                        0
                    } else if case == "no_traffic" {
                        9
                    } else {
                        10
                    };
                    let ack = if case == "no_ack" || (no_bindings && case != "na_positive_counters")
                    {
                        0
                    } else {
                        1
                    };
                    let opportunities = if case == "no_site_traffic" { 0 } else { 1 };
                    let begin = if case == "late_epoch" { 180000 } else { 120000 };
                    records.push(json!({"kind":"denominator","bucket_start_ms":begin,"bucket_end_ms":begin+60000,
                        "complete":true,"tui_turns":turns,"o_ack":ack,"opportunities":{"W11":opportunities,"W01":1}}));
                    if case == "bucket_duplicate" {
                        records.push(records.last().unwrap().clone());
                    }
                    if case == "partial_bucket" {
                        records.last_mut().unwrap()["bucket_end_ms"] = json!(CUT + 1);
                    }
                }
                let target_at = match case {
                    "old_unknown" if i == 0 => Some(START),
                    "after_end" if i == 0 => Some(START - 1),
                    "clock_reverse" if i == 2 => Some(START - 1),
                    "clock_forward" if i == 2 => Some(CUT + 5001),
                    "start_minus_one" if i == 0 => Some(START - 1),
                    "start_at" if i == 0 => Some(START),
                    "body" | "mixed_invalid" | "end_at" | "end_plus_one" | "unknown_body"
                    | "nonbody" | "refresh" | "runtime_unknown" | "runtime_empty"
                    | "owner_unknown" | "na_body" | "na_refresh"
                        if i == 2 =>
                    {
                        Some(CUT)
                    }
                    _ => None,
                };
                if let Some(at) = target_at {
                    records.push(json!({"kind":"target","phase":"begin","op":"POST","at_ms":at,
                        "census":{"site":"W11","binding_mode":"TUI","body_owner":"Legacy","actual_runtime":"codex_tui",
                        "source_incarnation":{"file":1,"generation":1},"output_class":match case {"unknown_body"|"old_unknown"|"after_end"=>"UnknownBody","nonbody"=>"NonBody","refresh"|"na_refresh"=>"LegacyBodyRefresh",_=>"LegacyBody"}}}));
                }
                if case == "runtime_unknown" && i == 2 {
                    records.last_mut().unwrap()["census"]["actual_runtime"] = json!("unknown");
                    records.last_mut().unwrap()["census"]["source_incarnation"] = json!(null);
                }
                if case == "runtime_empty" && i == 2 {
                    records.last_mut().unwrap()["census"]["actual_runtime"] = json!("");
                }
                if case == "owner_unknown" && i == 2 {
                    records.last_mut().unwrap()["census"]["body_owner"] = serde_json::Value::Null;
                }
                if i < last && !(case == "na_end_missing" && i == 1) {
                    let trailing = if case == "after_end" && i == 0 {
                        records.pop()
                    } else {
                        None
                    };
                    records.push(json!({"kind":"end","outputs_drained":true,"ended_at_ms":if case=="start_at" && i==0 {CUT} else {(i as i64+1)*START-1}}));
                    if let Some(target) = trailing {
                        records.push(target);
                    }
                }
                let mut text = header.to_string() + "\n";
                for record in &records {
                    text += &(record.to_string() + "\n");
                }
                if case != "old_drain_missing" || i != 0 {
                    text+=&(json!({"kind":"drain","boot_id":ids[i],"cut":records.len(),"closed":i<last,"invalid":false}).to_string()+"\n");
                }
                if case == "mixed_invalid" && i == 2 {
                    text += "{";
                }
                if case == "truncated" && i == 2 {
                    text.pop();
                }
                let path = directory.join(format!("{}.jsonl", ids[i]));
                std::fs::write(path, text).unwrap();
                let timestamp = chrono::DateTime::from_timestamp_millis(started + 1)
                    .unwrap()
                    .to_rfc3339();
                std::fs::write(doctor.join(format!("{doc}.json")),json!({"schema_version":1,"run_context":"startup_once","boot_id":doc,"started_at":timestamp}).to_string()).unwrap();
            }
            if case == "middle_boot_missing" {
                let at = chrono::DateTime::from_timestamp_millis(90000)
                    .unwrap()
                    .to_rfc3339();
                std::fs::write(doctor.join("999-90.json"),json!({"schema_version":1,"run_context":"startup_once","boot_id":"999-90","started_at":at}).to_string()).unwrap();
            }
            if case == "read_error" {
                std::fs::remove_file(directory.join("writer.lock")).unwrap();
            }
            ids[last].clone()
        }
        let mut contract_failures = Vec::new();
        for (case, expected) in [
            ("valid", "PASS"),
            ("artifact_absent", "PASS"),
            ("static_missing", "INCONCLUSIVE"),
            ("static_false", "INCONCLUSIVE"),
            ("static_malformed", "INCONCLUSIVE"),
            ("static_error", "INCONCLUSIVE"),
            ("static_hash", "INCONCLUSIVE"),
            ("static_pending", "INCONCLUSIVE"),
            ("static_error_missing", "INCONCLUSIVE"),
            ("static_schema", "INCONCLUSIVE"),
            ("historic_static_missing", "INCONCLUSIVE"),
            ("same_time", "PASS"),
            ("asynchronous_cuts", "PASS"),
            ("staggered_epochs", "FAIL"),
            ("bucket_duplicate", "PASS"),
            ("no_traffic", "NO_TRAFFIC"),
            ("no_ack", "NO_TRAFFIC"),
            ("no_site_traffic", "PASS"),
            ("denominator_missing", "INCONCLUSIVE"),
            ("no_restart", "INCONCLUSIVE"),
            ("partial_bucket", "INCONCLUSIVE"),
            ("live_missing", "INCONCLUSIVE"),
            ("live_pending", "INCONCLUSIVE"),
            ("live_invalid", "INCONCLUSIVE"),
            ("host_missing", "INCONCLUSIVE"),
            ("empty", "INCONCLUSIVE"),
            ("duplicate_host", "INCONCLUSIVE"),
            ("middle_boot_missing", "INCONCLUSIVE"),
            ("read_error", "INCONCLUSIVE"),
            ("truncated", "INCONCLUSIVE"),
            ("stale", "INCONCLUSIVE"),
            ("skew", "INCONCLUSIVE"),
            ("wrong_build", "INCONCLUSIVE"),
            ("wrong_census", "FAIL"),
            ("runtime_unknown", "FAIL"),
            ("runtime_empty", "FAIL"),
            ("owner_unknown", "INCONCLUSIVE"),
            ("unknown_body", "INCONCLUSIVE"),
            ("old_unknown", "INCONCLUSIVE"),
            ("after_end", "INCONCLUSIVE"),
            ("old_schema", "INCONCLUSIVE"),
            ("old_uncovered", "PASS"),
            ("old_drain_missing", "INCONCLUSIVE"),
            ("selection_disabled", "FAIL"),
            ("selection_unknown", "INCONCLUSIVE"),
            ("no_bindings", "PASS"),
            ("na_unknown", "INCONCLUSIVE"),
            ("na_history_binding", "NO_TRAFFIC"),
            ("na_body", "FAIL"),
            ("na_refresh", "INCONCLUSIVE"),
            ("na_end_missing", "INCONCLUSIVE"),
            ("na_positive_counters", "INCONCLUSIVE"),
            ("na_changed_selection", "INCONCLUSIVE"),
            ("na_earlier_unknown_selection", "INCONCLUSIVE"),
            ("selection_off", "FAIL"),
            ("body", "FAIL"),
            ("mixed_invalid", "FAIL"),
            ("clock_reverse", "INCONCLUSIVE"),
            ("clock_forward", "INCONCLUSIVE"),
            ("start_minus_one", "PASS"),
            ("start_at", "FAIL"),
            ("end_at", "FAIL"),
            ("end_plus_one", "PASS"),
            ("nonbody", "PASS"),
            ("refresh", "PASS"),
        ] {
            let mut observations = Vec::new();
            for name in ["mini", "book"] {
                let root = tempfile::tempdir().unwrap();
                let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
                let active = if case == "staggered_epochs" {
                    if name == "mini" {
                        "start_at"
                    } else {
                        "late_epoch"
                    }
                } else if name == "mini" || case == "no_site_traffic" {
                    case
                } else {
                    "valid"
                };
                let current = fixture(root.path(), name, active);
                let cut = if name == "book" && case == "asynchronous_cuts" {
                    CUT - 60000
                } else if name == "book" && case == "end_plus_one" {
                    CUT - 1
                } else {
                    CUT
                };
                let cache = if name == "book" && case == "asynchronous_cuts" {
                    ProjectionCache::fixture_at_age(root.path(), &current, cut, 60000)
                } else {
                    ProjectionCache::fixture(root.path(), &current, cut)
                };
                let now = if active == "stale" {
                    CUT + 120001
                } else if name == "book" && case == "skew" {
                    CUT + 5001
                } else {
                    CUT
                };
                let detail = with_projection_for_test(
                    cache.clone(),
                    now,
                    health_body("/health/detail", None),
                )
                .await;
                if case == "valid" {
                    assert_eq!(detail["legacy_body_census"]["probe_coverage"], true);
                    assert_eq!(
                        detail["legacy_body_census"]["static_instrumentation"]["complete"],
                        true
                    );
                    assert_eq!(detail["legacy_body_census"]["pending_probes"], 0);
                    assert_eq!(detail["legacy_body_census"]["read_error"], false);
                }
                if active == "stale" {
                    let local = &detail["legacy_body_census"]["local_evaluation"];
                    assert_eq!(local["status"], "INCONCLUSIVE");
                    assert_eq!(local["sites"]["W11"]["status"], "DELETE_NOT_READY");
                }
                observations.push(detail["legacy_body_census"].clone());
                let public =
                    with_projection_for_test(cache, now, health_body("/health", None)).await;
                assert!(public["legacy_body_census"]["boot_files"].is_null());
            }
            match case {
                "live_missing" => observations[0]["live_journal"] = serde_json::Value::Null,
                "live_pending" => {
                    observations[0]["live_journal"] = json!({"boot_id":observations[0]["current_boot_id"],"enqueued_cut":999,"invalid":false});
                }
                "live_invalid" => {
                    observations[0]["live_journal"] = json!({"boot_id":observations[0]["current_boot_id"],"enqueued_cut":3,"invalid":true});
                }
                "host_missing" => {
                    observations.pop();
                }
                "empty" => observations.clear(),
                "duplicate_host" => observations[1]["host"] = json!("mini"),
                _ => {}
            }
            let report = evaluate(["mini", "book"], &observations, CUT, BUILD, CENSUS);
            if report["stage1"] != expected {
                contract_failures.push(format!("{case}: expected {expected}: {report}"));
            }
            if case == "no_bindings" {
                assert_eq!(report["sites"]["W11"]["status"], "DELETE_NOT_READY");
                assert_eq!(report["hosts"]["mini"]["status"], "NO_TUI_BINDING");
            }
            if case == "artifact_absent"
                && report["hosts"]["mini"]["reasons"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("BOOT_EVIDENCE_UNKNOWN"))
            {
                contract_failures.push(format!("artifact absence still gates routine: {report}"));
            }
            if case == "valid" {
                assert_eq!(report["sites"]["W11"]["status"], "DELETE_READY");
                assert_eq!(report["sites"]["W01"]["status"], "DELETE_NOT_READY");
            }
            if case == "no_site_traffic" {
                assert_eq!(report["sites"]["W11"]["status"], "DELETE_NOT_READY");
            }
            if matches!(case, "refresh" | "runtime_unknown") {
                assert_eq!(report["sites"]["W11"]["status"], "DELETE_NOT_READY");
                assert_eq!(report["sites"]["W11"]["body_events"], 1);
            }
            if case == "mixed_invalid" {
                assert_eq!(report["complete"], false);
            }
        }
        let cases = [
            "valid",
            "body",
            "selection_unknown",
            "no_traffic",
            "no_bindings",
        ];
        let verdicts = [
            ["PASS", "FAIL", "INCONCLUSIVE", "NO_TRAFFIC", "PASS"],
            ["FAIL", "FAIL", "FAIL", "FAIL", "FAIL"],
            [
                "INCONCLUSIVE",
                "FAIL",
                "INCONCLUSIVE",
                "INCONCLUSIVE",
                "INCONCLUSIVE",
            ],
            [
                "NO_TRAFFIC",
                "FAIL",
                "INCONCLUSIVE",
                "NO_TRAFFIC",
                "NO_TRAFFIC",
            ],
            [
                "PASS",
                "FAIL",
                "INCONCLUSIVE",
                "NO_TRAFFIC",
                "NO_TUI_BINDING",
            ],
        ];
        for (a, left) in cases.iter().enumerate() {
            for (b, right) in cases.iter().enumerate() {
                let mut observations = Vec::new();
                for (host, case) in [("mini", *left), ("book", *right)] {
                    let root = tempfile::tempdir().unwrap();
                    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
                    let current = fixture(root.path(), host, case);
                    let cache = ProjectionCache::fixture(root.path(), &current, CUT);
                    let detail =
                        with_projection_for_test(cache, CUT, health_body("/health/detail", None))
                            .await;
                    observations.push(detail["legacy_body_census"].clone());
                }
                let report = evaluate(["mini", "book"], &observations, CUT, BUILD, CENSUS);
                if report["stage1"] != verdicts[a][b] {
                    contract_failures.push(format!(
                        "pair {left}/{right}: expected {}: {report}",
                        verdicts[a][b]
                    ));
                }
                if a == 4 || b == 4 {
                    assert_eq!(
                        report["sites"]["W11"]["status"], "DELETE_NOT_READY",
                        "{left}/{right}: {report}"
                    );
                }
            }
        }
        assert!(
            contract_failures.is_empty(),
            "{}",
            contract_failures.join("\n")
        );
        // A blocked cache publisher must not stall an actual detail request.
        let root = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let current = fixture(root.path(), "mini", "valid");
        let cache = ProjectionCache::fixture(root.path(), &current, CUT);
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let blocked = cache.clone();
        let a = entered.clone();
        let b = release.clone();
        let writer = std::thread::spawn(move || {
            blocked.block_for_test(|| {
                a.wait();
                b.wait();
            })
        });
        entered.wait();
        let (sent, received) = std::sync::mpsc::channel();
        let request = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let response = runtime.block_on(with_projection_for_test(
                cache,
                CUT,
                health_body("/health/detail", None),
            ));
            sent.send(response).unwrap();
        });
        let response = received.recv_timeout(std::time::Duration::from_secs(3));
        release.wait();
        writer.join().unwrap();
        request.join().unwrap();
        assert!(response.unwrap()["legacy_body_census"]["inventory_source"]["available"] == false);
        let mut collector = Collector::default();
        assert_eq!(
            collector.fixture(root.path(), &current, CUT)["local_evaluation"]["status"],
            "PASS"
        );
        let old = std::fs::read_dir(root.path().join("legacy_body_census"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "jsonl")
                    && std::fs::read_to_string(entry.path())
                        .unwrap()
                        .lines()
                        .next()
                        .unwrap()
                        .contains("\"started_at_ms\":0")
            })
            .unwrap()
            .path();
        let bytes = std::fs::read_to_string(&old).unwrap();
        std::fs::write(
            old,
            bytes.replace("\"host\":\"mini\"", "\"host\":\"changed\""),
        )
        .unwrap();
        for _ in 0..2 {
            let observed = collector.fixture(root.path(), &current, CUT);
            assert!(
                observed["errors"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("CLOSED_JOURNAL_CHANGED"))
            );
            assert_eq!(observed["local_evaluation"]["status"], "INCONCLUSIVE");
        }
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
