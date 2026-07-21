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
    /// #3293 (c): when true AND the repair fully applied, also unlink the
    /// channel's idle in-memory mailbox registry entry (no disk/DB mutation).
    /// `#[serde(default)]` keeps existing clients compatible.
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

/// `fully_recovered` tracks startup/recovery completion, not whether the
/// runtime is currently degraded. Runtime readiness lives in `status` and
/// `degraded_reasons`; preserving this axis lets operators distinguish
/// recovery-in-progress from ordinary live runtime degradation.
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

/// Build combined DB + Discord provider health.
/// Public callers receive only a redacted safe summary; authenticated/local
/// detail callers receive provider/config/outbox diagnostics.
async fn health_response(state: &AppState, detailed: bool) -> Response {
    let server_up = health_diagnostics::probe_server_up(state.pg_pool_ref()).await;

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

    let outbox_stats = health_diagnostics::load_dispatch_outbox_stats(state.pg_pool_ref()).await;
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
        health_diagnostics::load_config_audit_report_pg(state.pg_pool_ref()).await;
    let pipeline_override_report =
        health_diagnostics::load_pipeline_override_report_pg(state.pg_pool_ref()).await;

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
            health_diagnostics::enrich_mailbox_session_state(&mut json, state.pg_pool_ref()).await;
            // #stale-running-session-reconciler-audit: read-only DB active-session
            // mismatch audit. Detail-only (never on public GET), additive block,
            // no DB/session/runtime mutation. Runs AFTER mailbox enrichment so it
            // is off the hot public health path (REQ-003/REQ-004).
            json["active_session_audit"] = health_diagnostics::build_active_session_audit(
                state.pg_pool_ref(),
                state.cluster_instance_id.as_deref(),
            )
            .await
            .to_json();
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
        let all_registered_providers_standby = registry.all_providers_are_standby().await;
        let cluster_standby = cluster_standby_without_gateway || all_registered_providers_standby;
        if cluster_standby {
            degraded_reasons.retain(|reason| reason.as_str() != Some("no_providers_registered"));
            json["cluster_standby"] = serde_json::json!(true);
        }

        if !server_up {
            status = status.worsen(health::HealthStatus::Unhealthy);
            degraded_reasons.push(serde_json::json!("db_unavailable"));
        }
        if outbox_age >= health_diagnostics::OUTBOX_AGE_DEGRADED_SECS {
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

        // Resident OpenCode warm-pool diagnostics (additive, read-only). The
        // reasons worsen status to Degraded only; the per-server array is
        // detailed-only and the count summary is public-safe.
        for reason in opencode_warm_pool_degraded_reasons() {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(reason);
        }

        // #4515 PR2: worker-local recovery circuit. Budget exhaustion of a
        // necessary worker (dispatch_outbox / session_discovery) worsens to
        // Unhealthy → readiness 503; an un-migrated LoopOwned worker's
        // unexpected death worsens to Degraded. Flapping is intentionally kept
        // OUT of degraded_reasons (§9.3 deploy-gate safety) and exposed as a
        // separate informational field below.
        apply_worker_recovery_reasons(&mut status, &mut degraded_reasons);
        let worker_restart_flapping = crate::server::worker_recovery::recovery_flapping_info();
        if !worker_restart_flapping.is_empty() {
            json["worker_restart_flapping"] = serde_json::Value::Array(worker_restart_flapping);
        }

        // Startup doctor warnings are boot/recovery diagnostics, not proof
        // that the current runtime is unhealthy. Keep them on a separate
        // startup axis so deploy/restart gates that read runtime health do
        // not block unrelated live-turn-safe operations.
        let live_deferred_hooks = json["deferred_hooks"].as_u64().unwrap_or(0);
        let suppress_recovered_provider_deferred_hooks_backlog =
            provider_deferred_hooks_backlog_recovered(live_deferred_hooks, &degraded_reasons);
        let (doctor_failed, doctor_warned) =
            crate::cli::doctor::startup::latest_startup_doctor_effective_counts(
                suppress_recovered_provider_deferred_hooks_backlog,
            );
        json["startup_degraded_reasons"] =
            startup_doctor_count_reasons(doctor_failed, doctor_warned);

        // #2049 Finding 3: now that every worsen check has run, lift status
        // only for the legacy empty-registry standby case. A registered standby
        // worker keeps its provider classification; restart safety reads the
        // explicit counters/mailboxes even when status remains degraded.
        if cluster_standby_without_gateway && degraded_reasons.is_empty() {
            status = health::HealthStatus::Healthy;
        }

        let snapshot_fully_recovered = json["fully_recovered"].as_bool().unwrap_or(false);
        let fully_recovered =
            compute_fully_recovered(snapshot_fully_recovered, status, &degraded_reasons);
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
        if let Some(opencode_block) = opencode_warm_pool_json(detailed) {
            json["opencode"] = opencode_block;
        }

        // feature: rate-limit-aware-dispatch-gate (REQ-004). Aggregate,
        // credential-free dispatch-gate counters (no API tokens, no provider
        // credentials, no raw cache rows — only counts, the active threshold,
        // and the last-defer timestamp). Added on the `detailed` axis only;
        // `public_health_json` is an explicit allowlist so this never leaks on
        // the public `/api/health` endpoint. Additive: a NEW diagnostic block,
        // not a new `degraded_reasons` category.
        let (gate_enabled_override, gate_danger_override) =
            health_diagnostics::load_dispatch_gate_runtime_overrides(state.pg_pool_ref()).await;
        json["rate_limit_dispatch_gate"] =
            serde_json::to_value(crate::services::dispatch_gate::diagnostics_with_overrides(
                gate_enabled_override,
                gate_danger_override,
            ))
            .unwrap_or_else(|_| serde_json::json!({}));
        json["delivery_record_rollout"] = delivery_record_rollout_health_json();
        json["intake_routing"] =
            crate::services::cluster::intake_router_hook::intake_routing_status_json();

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
        let mut health_state = if server_up {
            health::HealthStatus::Healthy
        } else {
            health::HealthStatus::Unhealthy
        };
        let mut degraded_reasons: Vec<serde_json::Value> = Vec::new();
        if !server_up {
            degraded_reasons.push(serde_json::json!("db_unavailable"));
        }

        // Resident OpenCode warm-pool diagnostics (additive, read-only). Mirror
        // the registry branch above so a stopped or suspicious resident server
        // degrades standalone health too — otherwise top-level `/api/health`
        // and `/api/health/detail` could keep reporting `status: healthy` /
        // `ok: true` while a bad warm server is surfaced under `opencode`.
        // Per spec C-8 the `stopped_resident` reason is intentional worsening
        // and is kept consistent with the registry branch.
        for reason in opencode_warm_pool_degraded_reasons() {
            health_state = health_state.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(reason);
        }

        // #4515 PR2: mirror the registry branch so a fatal worker recovery
        // circuit also drives standalone `/api/health` readiness — otherwise a
        // HealthRegistry-less node would report ready while a necessary worker
        // is permanently dead.
        apply_worker_recovery_reasons(&mut health_state, &mut degraded_reasons);
        let worker_restart_flapping = crate::server::worker_recovery::recovery_flapping_info();

        let status = if health_state.is_http_ready() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        let health_status =
            serde_json::to_value(health_state).unwrap_or_else(|_| serde_json::json!("unhealthy"));
        // `ok` tracks full health: a degraded warm pool keeps the server
        // HTTP-ready but is not "ok".
        let healthy = health_state == health::HealthStatus::Healthy;
        // `fully_recovered` tracks the startup/recovery (server_up) axis, NOT
        // live runtime degradation — mirroring `compute_fully_recovered` in the
        // registry branch, whose doc explicitly excludes warm-pool health. A
        // degraded warm pool must not flip `fully_recovered` to false here, or
        // the standalone branch would be asymmetric with the registry branch
        // (where warm-pool reasons never touch `fully_recovered`). In
        // standalone mode the only non-warm-pool degradation is
        // `db_unavailable`, so the recovery axis is exactly `server_up`.
        let fully_recovered = server_up;
        let mut json = serde_json::json!({
            "status": health_status,
            "ok": healthy,
            "version": env!("CARGO_PKG_VERSION"),
            "db": server_up,
            "dashboard": dashboard_ok,
            "server_up": server_up,
            "fully_recovered": fully_recovered,
            "deferred_hooks": 0,
            "outbox_age": outbox_age,
            "queue_depth": 0,
            "watcher_count": 0,
            "recovery_duration": 0.0,
            "degraded_reasons": serde_json::Value::Array(degraded_reasons),
        });
        if !worker_restart_flapping.is_empty() {
            json["worker_restart_flapping"] = serde_json::Value::Array(worker_restart_flapping);
        }
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
        if detailed {
            json["active_session_audit"] = health_diagnostics::build_active_session_audit(
                state.pg_pool_ref(),
                state.cluster_instance_id.as_deref(),
            )
            .await
            .to_json();
        }
        if let Some(opencode_block) = opencode_warm_pool_json(detailed) {
            json["opencode"] = opencode_block;
        }
        json["delivery_record_rollout"] = delivery_record_rollout_health_json();
        json["intake_routing"] =
            crate::services::cluster::intake_router_hook::intake_routing_status_json();
        let json = if detailed {
            with_latest_startup_doctor(json, true)
        } else {
            with_latest_startup_doctor(public_health_json(json), false)
        };
        (status, Json(json)).into_response()
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

/// Bare (argument-less) provider degraded-reason classifications emitted by
/// `provider_probe::classify_provider`. Keep in sync with that producer.
const PROVIDER_BARE_REASONS: &[&str] =
    &["disconnected", "restart_pending", "reconcile_in_progress"];
/// Counted (`<keyword>:<N>`) provider degraded-reason classifications emitted by
/// `provider_probe::classify_provider`. Keep in sync with that producer.
const PROVIDER_COUNTED_REASONS: &[&str] = &[
    "deferred_hooks_backlog",
    "pending_queue_depth",
    "recovering_channels",
];

/// Sanitize one `provider:<name>:<reason>` string for public exposure.
///
/// #4386 round-2 defect: `<name>` is operator-controlled and — because a legacy
/// `bot_settings.json` `provider` value is preserved verbatim as
/// `ProviderKind::Unsupported(_)` — may itself contain `:`. A first-colon split
/// (`split_once`) leaves everything after the first colon in the "reason" tail,
/// leaking the rest of the name (`provider:prod-mini-01:customerA:disconnected`
/// -> `customerA` survives). A left-anchored "is the first segment a known id"
/// test is also bypassable (`provider:codex:leak:disconnected`). We therefore
/// anchor on the FIXED reason vocabulary from the RIGHT: the trailing 1-2
/// segments must match a known classification; everything before them is the
/// name, which is replaced WHOLESALE with `unsupported` unless it is exactly a
/// supported provider id (registry ids never contain `:`). Only the fixed reason
/// keyword and an all-digits count can survive, so no arbitrary name byte leaks.
/// Any unrecognized shape fails CLOSED to `provider:unsupported`.
fn sanitize_provider_reason(rest: &str, supported: &[&str]) -> String {
    let segments: Vec<&str> = rest.split(':').collect();
    // Counted reason: `<name...> : <keyword> : <digits>`.
    if segments.len() >= 3 {
        let count = segments[segments.len() - 1];
        let keyword = segments[segments.len() - 2];
        if !count.is_empty()
            && count.bytes().all(|b| b.is_ascii_digit())
            && PROVIDER_COUNTED_REASONS.contains(&keyword)
        {
            let name = segments[..segments.len() - 2].join(":");
            let reason = format!("{keyword}:{count}");
            return sanitized_provider_reason(&name, &reason, supported);
        }
    }
    // Bare reason: `<name...> : <keyword>`.
    if segments.len() >= 2 {
        let keyword = segments[segments.len() - 1];
        if PROVIDER_BARE_REASONS.contains(&keyword) {
            let name = segments[..segments.len() - 1].join(":");
            return sanitized_provider_reason(&name, keyword, supported);
        }
    }
    // Unknown / malformed shape: drop everything after `provider:` (fail closed).
    "provider:unsupported".to_string()
}

fn sanitized_provider_reason(name: &str, reason: &str, supported: &[&str]) -> String {
    if supported.contains(&name) {
        format!("provider:{name}:{reason}")
    } else {
        format!("provider:unsupported:{reason}")
    }
}

/// #4382 / #4386-review defect 1: `degraded_reasons` embeds `provider:<name>:...`
/// where `<name>` can be an ARBITRARY, operator-chosen string — a legacy
/// `bot_settings.json` `provider` field is parsed via
/// `ProviderKind::from_str_or_unsupported`, which preserves the raw value as
/// `Unsupported(_)` and re-emits it verbatim. Copying reasons unredacted onto the
/// UNAUTHENTICATED public `/api/health` would leak internal identifiers/hostnames
/// (e.g. `provider:prod-mini-01:disconnected`), breaking the allowlist guarantee
/// the public projection is documented to uphold. Rewrite any `provider:<name>:`
/// whose `<name>` is not a known, supported provider id to `provider:unsupported:`
/// (see `sanitize_provider_reason` for the colon-safe, fail-closed parsing).
/// `/api/health/detail` (authenticated) keeps the verbatim reasons. The rewrite is
/// 1:1 so the `degraded <=> non-empty` invariant is preserved.
fn sanitize_public_degraded_reasons(reasons: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Array(items) = reasons else {
        return serde_json::json!([]);
    };
    let supported = crate::services::provider::supported_provider_ids();
    let sanitized: Vec<serde_json::Value> = items
        .into_iter()
        .map(|item| {
            let Some(reason) = item.as_str() else {
                return item;
            };
            match reason.strip_prefix("provider:") {
                Some(rest) => serde_json::Value::String(sanitize_provider_reason(rest, &supported)),
                None => serde_json::Value::String(reason.to_string()),
            }
        })
        .collect();
    serde_json::Value::Array(sanitized)
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
    let startup_status = json.get("startup_status").cloned();
    let startup_degraded = json.get("startup_degraded").cloned();
    let startup_degraded_reasons = json.get("startup_degraded_reasons").cloned();
    let delivery_record_rollout = json.get("delivery_record_rollout").cloned();
    let intake_routing = json.get("intake_routing").cloned();
    // Public OpenCode summary is count-only and never includes the per-server
    // `warm_servers` array, pids, ports, or startup tails (spec C-3). The
    // upstream `opencode_warm_pool_json(false)` already produced a count-only
    // object for the public path; defensively strip `warm_servers` if present.
    let opencode_public = json.get("opencode").map(|block| {
        serde_json::json!({
            "warm_server_count": block.get("warm_server_count").cloned().unwrap_or(serde_json::json!(0)),
            "warm_server_active_sessions": block.get("warm_server_active_sessions").cloned().unwrap_or(serde_json::json!(0)),
            "warm_server_suspicious_count": block.get("warm_server_suspicious_count").cloned().unwrap_or(serde_json::json!(0)),
        })
    });
    let degraded = status.as_str().is_some_and(|status| status != "healthy");
    // #4382: carry the live `degraded_reasons` (the axis that actually decides
    // `degraded`/`status`) into the public object instead of dropping it, so
    // public-only consumers stop misattributing the cause to the unrelated
    // `startup_degraded_reasons`. Always present (empty array when absent) so the
    // `degraded <=> degraded_reasons non-empty` invariant holds on the public shape.
    // Sanitized to strip operator-chosen provider ids before public exposure
    // (#4386-review defect 1); see `sanitize_public_degraded_reasons`.
    let degraded_reasons = sanitize_public_degraded_reasons(
        json.get("degraded_reasons")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    );
    let mut public = serde_json::json!({
        "ok": !degraded,
        "status": status,
        "version": version,
        "db": db,
        "dashboard": dashboard,
        "server_up": server_up,
        "fully_recovered": fully_recovered,
        "cluster_standby": cluster_standby,
        "degraded": degraded,
        "degraded_reasons": degraded_reasons,
    });
    if let Some(startup_status) = startup_status {
        public["startup_status"] = startup_status;
    }
    if let Some(startup_degraded) = startup_degraded {
        public["startup_degraded"] = startup_degraded;
    }
    if let Some(startup_degraded_reasons) = startup_degraded_reasons {
        public["startup_degraded_reasons"] = startup_degraded_reasons;
    }
    if let Some(delivery_record_rollout) = delivery_record_rollout {
        public["delivery_record_rollout"] = delivery_record_rollout;
    }
    if let Some(intake_routing) = intake_routing {
        public["intake_routing"] = intake_routing;
    }
    if let Some(opencode_public) = opencode_public {
        public["opencode"] = opencode_public;
    }
    public
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
    health_diagnostics::is_recent_cluster_worker(
        state.pg_pool_ref(),
        instance_id,
        state.config.cluster.lease_ttl_secs,
    )
    .await
}

fn stale_mailbox_repair_applied(
    removed_token: bool,
    inflight_cleared: bool,
    session_disconnected_count: usize,
) -> bool {
    removed_token || inflight_cleared || session_disconnected_count > 0
}

/// #3293 (c): whether the optional mailbox-registry purge may run after a
/// stale-mailbox repair, and the skip reason to report when it may not.
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

/// Build the additive `opencode` warm-pool diagnostics block.
///
/// `detailed=true` (authenticated/local) includes the full redacted per-server
/// snapshot array; public callers get count-only aggregates. Returns `None`
/// when no resident warm servers exist so the field is omitted entirely
/// (cold-start safe — spec C-6/C-7). Snapshot collection performs no network
/// probes and copies pool data under short locks (REQ-002).
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

/// Additive `degraded_reasons` for the resident warm pool (REQ-004). These are
/// classified by the existing `classify_degraded_reason` table and are distinct
/// from the fresh-serve / MCP doctor checks.
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

/// #4515 PR2: fold worker-local recovery reasons into a health snapshot. Shared
/// by the registry and standalone `/api/health` branches so a fatal worker
/// recovery circuit drives readiness identically in both. Flapping is handled
/// separately (informational field) and never appears here.
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
    health_response(&state, false).await
}

/// GET /api/health/detail — authenticated or local detailed health.
pub async fn health_detail_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
        .into_response();
    }
    health_response(&state, true).await
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

/// GET /api/doctor/startup/latest — protected/local latest startup doctor artifact.
pub async fn startup_doctor_latest_handler(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !local_or_configured_control_endpoint_allowed(&state.config, Some(peer_addr)) {
        return legacy_health_error(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            "auth_token required for non-loopback host",
        ))
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
                    "fix_safety": "safe_local_repair",
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
                    "fix_safety": "safe_local_repair",
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
                    "fix_safety": "safe_local_repair",
                    "safety_gate": "mailbox_not_found",
                    "skipped_reason": "no mailbox handle exists for channel",
                    "post_repair_mailbox": null,
                    "post_repair_watcher_inflight": null
                })),
            )
                .into_response();
        };
        let snapshot = handle.snapshot().await;
        health::ProviderMailboxState {
            channel_id: request.channel_id,
            has_cancel_token: snapshot.cancel_token.is_some(),
            queue_depth: snapshot.intervention_queue.len(),
            recovery_started: snapshot.recovery_started_at.is_some(),
        }
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
                "fix_safety": "safe_local_repair",
                "safety_gate": "expected_evidence_mismatch",
                "skipped_reason": "mailbox evidence changed before repair",
                "post_repair_mailbox": before,
                "post_repair_watcher_inflight": null
            })),
        )
            .into_response();
    }
    if before.queue_depth > 0 {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "safe_local_repair",
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
    let before_session_state =
        health_diagnostics::load_channel_session_state(state.pg_pool_ref(), request.channel_id)
            .await;
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
                "post_repair_mailbox": before,
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
        let idle_tmux_repair = match (
            state.health_registry.as_ref(),
            before_watcher_inflight.as_ref(),
        ) {
            (Some(registry), Some(snapshot)) => {
                let snapshot_provider = ProviderKind::from_str(&snapshot.provider);
                let tmux_session = snapshot.tmux_session.as_deref();
                if let (Some(provider), Some(tmux_session)) = (snapshot_provider, tmux_session) {
                    let inflight_safe = if snapshot.inflight_state_present {
                        crate::services::discord::inflight_state_allows_idle_tmux_repair_for_channel(
                            &provider,
                            request.channel_id,
                        )
                        .unwrap_or(false)
                    } else {
                        true
                    };
                    // #3668 F2: never destructively idle-clear when a final
                    // answer is still persisted in JSONL after `last_offset` —
                    // let normal recovery deliver it instead of dropping it.
                    let unrelayed_tail =
                        crate::services::discord::relay_recovery::channel_has_unrelayed_idle_tmux_tail_answer(
                            &provider,
                            request.channel_id,
                        );
                    let no_unread_bytes = snapshot.unread_bytes.unwrap_or(0) == 0;
                    // Keep the manual stale-mailbox repair's destructive idle
                    // clear gate aligned with ReattachWatcher: unread capture bytes
                    // are live relay evidence, so do not retire mailbox/inflight
                    // bookkeeping while the watcher still has bytes to consume.
                    if inflight_safe && no_unread_bytes && !unrelayed_tail {
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
                    .unwrap_or(health::ProviderMailboxState {
                        channel_id: request.channel_id,
                        has_cancel_token: false,
                        queue_depth: 0,
                        recovery_started: false,
                    }),
                    None => health::ProviderMailboxState {
                        channel_id: request.channel_id,
                        has_cancel_token: false,
                        queue_depth: 0,
                        recovery_started: false,
                    },
                }
            } else if let Some(handle) = global_handle.as_ref() {
                let snapshot = handle.snapshot().await;
                health::ProviderMailboxState {
                    channel_id: request.channel_id,
                    has_cancel_token: snapshot.cancel_token.is_some(),
                    queue_depth: snapshot.intervention_queue.len(),
                    recovery_started: snapshot.recovery_started_at.is_some(),
                }
            } else {
                health::ProviderMailboxState {
                    channel_id: request.channel_id,
                    has_cancel_token: false,
                    queue_depth: 0,
                    recovery_started: false,
                }
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
                    "fix_safety": "safe_idle_tmux_repair",
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
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "applied": false,
                "skipped": true,
                "fix_safety": "explicit_restart_required",
                "safety_gate": "tmux_present",
                "skipped_reason": "live tmux evidence exists",
                "post_repair_mailbox": before,
                "post_repair_watcher_inflight": before_watcher_inflight
            })),
        )
            .into_response();
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
        crate::services::discord::clear_inflight_state_for_channel(&provider, request.channel_id);
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
            .unwrap_or(health::ProviderMailboxState {
                channel_id: request.channel_id,
                has_cancel_token: false,
                queue_depth: 0,
                recovery_started: false,
            }),
            None => health::ProviderMailboxState {
                channel_id: request.channel_id,
                has_cancel_token: false,
                queue_depth: 0,
                recovery_started: false,
            },
        }
    } else if let Some(handle) = global_handle.as_ref() {
        let snapshot = handle.snapshot().await;
        health::ProviderMailboxState {
            channel_id: request.channel_id,
            has_cancel_token: snapshot.cancel_token.is_some(),
            queue_depth: snapshot.intervention_queue.len(),
            recovery_started: snapshot.recovery_started_at.is_some(),
        }
    } else {
        health::ProviderMailboxState {
            channel_id: request.channel_id,
            has_cancel_token: false,
            queue_depth: 0,
            recovery_started: false,
        }
    };
    let after_session_state =
        health_diagnostics::load_channel_session_state(state.pg_pool_ref(), request.channel_id)
            .await;
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
    // #3293 (c): optional registry purge — only after the full gate chain
    // above passed AND the repair fully applied. `remove_idle_entry` re-checks
    // actor idleness right before the in-memory unlink.
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
            "fix_safety": "safe_local_repair",
            "safety_gate": "no_live_work_evidence",
            "inflight_cleared": inflight_cleared,
            "session_disconnected_count": session_disconnected_count,
            "session_disconnect_error": session_disconnect_error,
            "pre_repair_session": before_session_state,
            "post_repair_session": after_session_state,
            "delivery_completed": false,
            "registry_entry_removed": registry_entry_removed,
            "registry_purge_skipped_reason": registry_purge_skipped_reason,
            "post_repair_mailbox": after,
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
/// This only rotates the utility bots backed by `HealthRegistry`
/// (`credential/announce_bot_token`, `credential/notify_bot_token`). Provider
/// runtime gateway token caches are `OnceCell`s and still require a dcserver
/// restart; the response reports each reload scope explicitly.
///
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor and non-loopback Bearer requirement.
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
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
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
/// See `send_handler` for the rationale on the mandatory
/// `ConnectInfo<SocketAddr>` extractor.
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

    fn test_api_router_with_config_and_registry(
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

    struct EnvVarGuard {
        key: String,
        previous_value: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set_path(key: &str, path: &Path) -> Self {
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
            "active_session_audit": {
                "enabled": true,
                "candidate_count": 1,
                "candidates": [{"session_key": "host/mini:codex-1", "confidence": 0.8}]
            }
        }));

        assert_eq!(public["status"], "degraded");
        assert_eq!(public["version"], "0.1.2");
        assert_eq!(public["db"], true);
        assert_eq!(public["dashboard"], false);
        assert_eq!(public["degraded"], true);
        assert!(public.get("providers").is_none());
        assert!(public.get("mailboxes").is_none());
        assert!(public.get("config_audit").is_none());
        // #4382: the live `degraded_reasons` that DECIDES `degraded` must now be
        // carried into public health verbatim (was dropped, forcing consumers to
        // misattribute the cause to the unrelated `startup_degraded_reasons`).
        assert_eq!(
            public["degraded_reasons"],
            json!(["provider:codex:pending_queue_depth:2"])
        );
        // TEST-004: the detail-only audit block is dropped from public health.
        assert!(public.get("active_session_audit").is_none());
    }

    /// #4382 invariant: on the PUBLIC projection, `degraded` is true IFF
    /// `degraded_reasons` is present and non-empty (both directions). A degraded
    /// health with no reasons, or a healthy one carrying reasons, is unreachable
    /// through this projection contract.
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

        // absent upstream array => still present as [] (the invariant never sees
        // a missing key), and healthy stays not-degraded.
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

    /// #4382 regression: `degraded_reasons` (the live axis that decides
    /// `degraded`) and `startup_degraded_reasons` (a startup-only axis that does
    /// NOT decide `degraded`) must surface as two DISTINCT public fields so a
    /// consumer can no longer misattribute a runtime-degraded cause to startup.
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
        // The two axes are genuinely different values, not aliases.
        assert_ne!(
            public["degraded_reasons"],
            public["startup_degraded_reasons"]
        );
    }

    /// #4386-review defect 1 + round-2 (P0 security): an operator-chosen provider
    /// id — a legacy `bot_settings.json` value preserved verbatim as
    /// `Unsupported(_)`, and thus possibly containing `:` — must NOT leak in ANY
    /// part on the unauthenticated public `/api/health`. The public projection
    /// replaces the WHOLE name with `unsupported`, keeping only the fixed reason
    /// classification (and an all-digits count); unknown shapes fail closed to
    /// `provider:unsupported`. Known providers pass through verbatim; the detail
    /// path (raw snapshot json) retains the value untouched.
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
            // 1:1 rewrite keeps the degraded<=>non-empty invariant.
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

    /// Guards the whitelist source: the sanitizer trusts exactly the registry
    /// ids, and those ids must never contain `:` (the delimiter the right-anchor
    /// parser relies on to tell a single-segment trusted name from a crafted
    /// multi-segment one).
    #[test]
    fn supported_provider_ids_contain_no_colon() {
        for id in crate::services::provider::supported_provider_ids() {
            assert!(
                !id.contains(':'),
                "supported provider id {id:?} contains ':'"
            );
        }
    }

    /// [TEST-003] public health omits the per-server OpenCode warm_servers
    /// array but may keep the count-only summary; no pid/port/tail leaks.
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
        // The per-server array and all sensitive fields are gone.
        assert!(opencode.get("warm_servers").is_none());
        let text = public.to_string();
        assert!(!text.contains("12345"));
        assert!(!text.contains("54321"));
        assert!(!text.contains("startup_output_tail"));
        assert!(!text.contains("base_url"));
        assert!(!text.contains("abcdef0123456789"));
    }

    /// The standalone (no-HealthRegistry) branch now mirrors the registry
    /// branch: when `opencode_warm_pool_degraded_reasons()` reports a bad warm
    /// server it sets `status: "degraded"`, which the public projection turns
    /// into `ok: false` / `degraded: true` instead of leaving health "healthy".
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
                "warning_count": 1,
                "configuration_warnings": [
                    "delivery_record_authority_disabled: durable frontiers are not the default committed-offset authority"
                ]
            }
        }));
        assert_eq!(public["delivery_record_rollout"]["mode"], json!("off"));
        assert_eq!(public["delivery_record_rollout"]["warning_count"], json!(1));
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
                "mode": "observe",
                "source": "yaml",
                "yaml": {
                    "enabled": true,
                    "mode": "observe",
                    "forward_pre_claim_timeout_secs": 12,
                    "stale_claim_recovery_secs": 60
                },
                "env_override": null,
                "warning_count": 0,
                "configuration_warnings": []
            }
        }));
        assert_eq!(public["intake_routing"]["mode"], json!("observe"));
        assert_eq!(public["intake_routing"]["source"], json!("yaml"));
        assert_eq!(public["intake_routing"]["warning_count"], json!(0));
        assert_eq!(public["ok"], json!(true));
    }

    #[test]
    fn stale_mailbox_repair_applied_includes_session_only_disconnect() {
        assert!(stale_mailbox_repair_applied(false, false, 1));
        assert!(stale_mailbox_repair_applied(true, false, 0));
        assert!(stale_mailbox_repair_applied(false, true, 0));
        assert!(!stale_mailbox_repair_applied(false, false, 0));
    }

    /// #3293 (c): the registry purge runs ONLY when explicitly requested AND
    /// the repair fully applied; a partial repair reports the skip reason that
    /// surfaces as `registry_purge_skipped_reason` in the response.
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

    /// `fully_recovered` is the startup/recovery completion signal. Runtime
    /// degradation is reported separately through status and degraded reasons.
    #[test]
    fn compute_fully_recovered_preserves_recovery_axis_when_runtime_degrades() {
        use super::compute_fully_recovered;
        use crate::services::discord::health;

        // Clean state — healthy + no reasons → fully_recovered=true.
        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Healthy,
            &[]
        ));

        // Runtime degradations are exposed through status/degraded_reasons,
        // but do not rewrite the startup/recovery axis.
        let reasons_db = vec![json!("db_unavailable")];
        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Healthy,
            &reasons_db
        ));

        // Multiple reasons with a Degraded status still leave fully_recovered=true.
        let reasons_outbox_disk = vec![
            json!("dispatch_outbox_oldest_pending_age:120"),
            json!("disk_low_free_bytes:104857600"),
        ];
        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Degraded,
            &reasons_outbox_disk
        ));

        // Unhealthy runtime status also stays separate from recovery state.
        assert!(compute_fully_recovered(
            true,
            health::HealthStatus::Unhealthy,
            &[]
        ));

        // Existing recovery-in-progress state remains false.
        assert!(!compute_fully_recovered(
            false,
            health::HealthStatus::Healthy,
            &[]
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
