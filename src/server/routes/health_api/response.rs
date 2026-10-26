//! `/api/health` response assembly for the registry and standalone arms.
use super::db_collection::{self, Initial};
use super::*;

/// Build combined DB + Discord provider health: a redacted summary for public
/// callers, full diagnostics for authenticated/local detail callers.
pub(super) async fn health_response(state: &AppState, detailed: bool) -> Response {
    let deadline = db_collection::deadline();
    let Some(registry) = state.health_registry.as_ref() else {
        let (initial, ()) = db_collection::read_initial_alongside(state, deadline, async {}).await;
        return assemble(state, detailed, initial, None).await;
    };
    let snapshot = async {
        // The mailbox reads end by the same deadline as the database reads.
        let snapshot = registry.health_snapshot_by(detailed, deadline).await;
        let status = snapshot.status();
        (
            status,
            serde_json::to_value(snapshot).unwrap_or_else(|_| serde_json::json!({})),
        )
    };
    registry_response(state, registry, detailed, deadline, snapshot).await
}

/// The registry arm; `snapshot` runs alongside the initial reads, then the late reads use
/// whatever time it left before `deadline`.
pub(super) async fn registry_response(
    state: &AppState,
    registry: &health::HealthRegistry,
    detailed: bool,
    deadline: tokio::time::Instant,
    snapshot: impl std::future::Future<Output = (health::HealthStatus, serde_json::Value)>,
) -> Response {
    let alongside = async {
        let (status, json) = snapshot.await;
        let late = db_collection::read_late(state, detailed, &json, deadline).await;
        (status, json, late)
    };
    let (initial, snapshot) =
        db_collection::read_initial_alongside(state, deadline, alongside).await;
    assemble(state, detailed, initial, Some((registry, snapshot))).await
}

type Snapshot<'a> = (
    &'a health::HealthRegistry,
    (
        health::HealthStatus,
        serde_json::Value,
        db_collection::LateReads,
    ),
);

async fn assemble(
    state: &AppState,
    detailed: bool,
    initial: Initial,
    snapshot: Option<Snapshot<'_>>,
) -> Response {
    let server_up = matches!(initial.server_up, Ok(true));
    // A SELECT 1 still pending at the deadline is `db_slow`, not a failed database.
    let db_unavailable = !server_up && initial.server_up.is_ok();
    let late_timed_out = snapshot
        .as_ref()
        .is_some_and(|(_, (_, _, late))| late.timed_out());
    let db_slow = initial.timed_out() || late_timed_out;
    let release_source = crate::services::release_source::health_json(detailed);

    let dashboard_ok = state.config.cluster.runtime_profile.modules().dashboard && {
        let dashboard_dir = crate::cli::agentdesk_runtime_root()
            .map(|r| r.join("dashboard/dist"))
            .unwrap_or_else(|| std::path::PathBuf::from("dashboard/dist"));
        dashboard_dir.join("index.html").exists()
    };

    // ENOSPC silently breaks inflight state writes and tool buffers, so surface free
    // disk on the runtime partition for the dashboard and `agentdesk doctor` to warn on.
    let disk_probe_path =
        crate::cli::agentdesk_runtime_root().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let disk_snapshot = disk_monitor::probe(&disk_probe_path);

    let outbox_stats = initial.outbox.ok().flatten();
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
    // Auto-queue cleanup outbox backlog. `dead_lettered` rows hit the attempt cap and are
    // never retried, stranding a slot token and provider session.
    let auto_queue_cleanup_json = initial.cleanup.ok().flatten().map(|backlog| {
        serde_json::json!({
            "pending": backlog.pending,
            "dead_lettered": backlog.dead_lettered,
        })
    });
    let kv = initial.kv.ok();
    let config_audit_report = kv.as_ref().and_then(|kv| kv.config_audit.clone());
    let pipeline_override_report = kv.as_ref().and_then(|kv| kv.pipeline_overrides.clone());
    if let Some((registry, (mut status, mut json, late))) = snapshot {
        if let Some(lookups) = &late.sessions {
            health_diagnostics::enrich_mailbox_sessions(&mut json, lookups).await;
        }
        let mut degraded_reasons = json["degraded_reasons"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        health::attach_turn_presence_unknown(
            &mut status,
            &mut degraded_reasons,
            &mut json,
            detailed,
        );
        health::attach_o_stall_reasons(&mut status, &mut degraded_reasons);
        health::attach_tmux_probe_preflight(
            registry,
            &mut status,
            &mut degraded_reasons,
            &mut json,
        );
        // Read from the snapshot's reasons before the `retain` below drops
        // `no_providers_registered` on a standby node.
        let cluster_standby_without_gateway = late.standby_without_gateway && server_up;
        let all_registered_providers_standby = registry.all_providers_are_standby().await;
        let cluster_standby = cluster_standby_without_gateway || all_registered_providers_standby;
        if cluster_standby {
            degraded_reasons.retain(|reason| reason.as_str() != Some("no_providers_registered"));
            json["cluster_standby"] = serde_json::json!(true);
        }

        if !server_up {
            status = status.worsen(health::HealthStatus::Unhealthy);
        }
        if db_unavailable {
            degraded_reasons.push(serde_json::json!("db_unavailable"));
        }
        if db_slow {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!("db_slow"));
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

        for reason in opencode_warm_pool_degraded_reasons() {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(reason);
        }

        apply_worker_recovery_reasons(&mut status, &mut degraded_reasons);
        let worker_restart_flapping = crate::server::worker_recovery::recovery_flapping_info();
        if !worker_restart_flapping.is_empty() {
            json["worker_restart_flapping"] = serde_json::Value::Array(worker_restart_flapping);
        }

        // Startup doctor findings are boot diagnostics, not runtime health; a separate
        // startup axis keeps them from blocking gates that read runtime health.
        let live_deferred_hooks = json["deferred_hooks"].as_u64().unwrap_or(0);
        let suppress_recovered_provider_deferred_hooks_backlog =
            provider_deferred_hooks_backlog_recovered(live_deferred_hooks, &degraded_reasons);
        let (doctor_failed, doctor_warned) =
            crate::cli::doctor::startup::latest_startup_doctor_effective_counts(
                suppress_recovered_provider_deferred_hooks_backlog,
            );
        json["startup_degraded_reasons"] =
            startup_doctor_count_reasons(doctor_failed, doctor_warned);

        // A standby without a gateway is relay-dead even while HTTP and the worker
        // heartbeat are live, so it must never read as healthy.
        if cluster_standby_without_gateway {
            status = status.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!("gateway_standby"));
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
        if let Some(backlog) = auto_queue_cleanup_json {
            json["auto_queue_cleanup"] = backlog;
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

        // Credential-free dispatch-gate counters. Attached on both paths, but only the detail
        // body carries them: `public_health_json` does not allowlist this key.
        let (gate_enabled_override, gate_danger_override) = kv
            .as_ref()
            .map(|kv| kv.dispatch_gate_overrides())
            .unwrap_or((None, None));
        let mut gate =
            serde_json::to_value(crate::services::dispatch_gate::diagnostics_with_overrides(
                gate_enabled_override,
                gate_danger_override,
            ))
            .unwrap_or_else(|_| serde_json::json!({}));
        // A cut kv read leaves the persisted overrides unknown, so no fallback is shown as effective.
        if let (None, Some(gate)) = (&kv, gate.as_object_mut()) {
            gate.remove("enabled");
            gate.remove("danger_pct");
        }
        json["rate_limit_dispatch_gate"] = gate;
        json["delivery_record_rollout"] = delivery_record_rollout_health_json();
        json["resident_loops"] = crate::services::resident_loops::health_json();
        json["release_source"] = release_source;
        json["boot_incarnation"] = serde_json::json!(boot_incarnation());
        attach_runtime_profile(&mut json, &state.config);
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
        if db_unavailable {
            degraded_reasons.push(serde_json::json!("db_unavailable"));
        }
        if db_slow {
            health_state = health_state.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(serde_json::json!("db_slow"));
        }

        // Mirror the registry branch so a bad resident warm server cannot sit under
        // `opencode` while standalone health still reports `ok: true`.
        for reason in opencode_warm_pool_degraded_reasons() {
            health_state = health_state.worsen(health::HealthStatus::Degraded);
            degraded_reasons.push(reason);
        }

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
        // Recovery axis only, as in `compute_fully_recovered`: warm-pool and worker-recovery
        // reasons must not flip it.
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
            "boot_incarnation": boot_incarnation(),
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
        if let Some(backlog) = auto_queue_cleanup_json {
            json["auto_queue_cleanup"] = backlog;
        }
        if let Some(report) = config_audit_report {
            json["config_audit"] = report;
        }
        if let Some(report) = pipeline_override_report {
            json["pipeline_overrides"] = report;
        }
        if let Some(opencode_block) = opencode_warm_pool_json(detailed) {
            json["opencode"] = opencode_block;
        }
        json["delivery_record_rollout"] = delivery_record_rollout_health_json();
        json["resident_loops"] = crate::services::resident_loops::health_json();
        json["release_source"] = release_source;
        attach_runtime_profile(&mut json, &state.config);
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

/// Random once per process, so a changed value proves the server restarted.
/// Detail-only: `public_health_json` does not allowlist it.
fn boot_incarnation() -> &'static str {
    static INCARNATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INCARNATION.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

#[cfg(test)]
#[path = "kv_gate_tests.rs"]
mod kv_gate_tests;

#[cfg(test)]
#[path = "boot_incarnation_tests.rs"]
mod boot_incarnation_tests;
