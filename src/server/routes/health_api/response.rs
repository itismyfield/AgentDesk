//! `/api/health` response assembly for the registry and standalone arms.
use super::*;

/// Build combined DB + Discord provider health: a redacted summary for public
/// callers, full diagnostics for authenticated/local detail callers.
pub(super) async fn health_response(state: &AppState, detailed: bool) -> Response {
    let server_up = health_diagnostics::probe_server_up(state.pg_pool_ref()).await;
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
    // Auto-queue cleanup outbox backlog. `dead_lettered` rows hit the attempt cap and are
    // never retried, stranding a slot token and provider session.
    let auto_queue_cleanup_json =
        health_diagnostics::load_auto_queue_cleanup_backlog(state.pg_pool_ref())
            .await
            .map(|backlog| {
                serde_json::json!({
                    "pending": backlog.pending,
                    "dead_lettered": backlog.dead_lettered,
                })
            });
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
            // Read-only DB active-session mismatch audit; detail-only, mutates nothing.
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
        // Must run before the `retain` below: it keys on the `no_providers_registered`
        // reason that a standby node then drops as noise.
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
        let (gate_enabled_override, gate_danger_override) =
            health_diagnostics::load_dispatch_gate_runtime_overrides(state.pg_pool_ref()).await;
        json["rate_limit_dispatch_gate"] =
            serde_json::to_value(crate::services::dispatch_gate::diagnostics_with_overrides(
                gate_enabled_override,
                gate_danger_override,
            ))
            .unwrap_or_else(|_| serde_json::json!({}));
        json["delivery_record_rollout"] = delivery_record_rollout_health_json();
        json["release_source"] = release_source;
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
        if !server_up {
            degraded_reasons.push(serde_json::json!("db_unavailable"));
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
