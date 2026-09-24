//! Tmux-only cleanup, with owner forwarding and conservative idle guards.

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use serde_json::json;

use crate::app_state::AppState;
use crate::db::dispatched_sessions as dispatched_sessions_db;
use crate::services::discord::session_identity::tmux_name_from_session_key;

use super::{
    RawProviderTranscriptObservationMode, classify_session_termination_reason,
    latest_runtime_activity_unix_nanos, now_unix_nanos, provider_resume_selector_is_effective,
    record_raw_provider_transcript_len_watermark_if_observed,
};

pub(super) async fn kill_tmux_session_impl(
    state: &AppState,
    headers: &HeaderMap,
    session_key: &str,
    reason: &str,
    minimum_idle_minutes: Option<u64>,
) -> (StatusCode, Json<serde_json::Value>) {
    let tmux_name = match tmux_name_from_session_key(session_key) {
        Some(name) => name,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "invalid session_key format — expected legacy host:tmux or namespaced provider/token/host:tmux"}),
                ),
            );
        }
    };

    let provider_info =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);
    let provider_name = provider_info
        .as_ref()
        .map(|(provider, _)| provider.as_str());

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let (active_dispatch_id, _agent_id, _runtime_channel_id, session_provider, owner_instance_id) =
        match dispatched_sessions_db::load_force_kill_session_pg(pool, session_key, provider_name)
            .await
        {
            Ok(Some(tuple)) => tuple,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "session not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

    let forward_context = crate::services::session_forwarding::ForwardCallerContext::from(state);
    if let Err(response) = crate::services::session_forwarding::enforce_receiver_fence(
        headers,
        owner_instance_id.as_deref(),
        forward_context.cluster_instance_id.as_deref(),
    ) {
        return response;
    }
    if !crate::services::session_forwarding::is_forwarded_request(headers) {
        match crate::services::session_forwarding::resolve_forward_target(
            &forward_context,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_kill_tmux(
                    &forward_context,
                    &target,
                    session_key,
                    reason,
                    minimum_idle_minutes,
                )
                .await;
            }
            crate::services::session_forwarding::ForwardResolution::Unavailable {
                status,
                body,
            } => {
                return (status, Json(body));
            }
        }
    }
    let effective_provider_name = provider_name.or(session_provider.as_deref());

    let reason_is_idle_cleanup = reason_is_idle_cleanup_reason(reason);
    let tmux_presence = crate::services::platform::tmux::session_presence(&tmux_name);
    if (reason_is_idle_cleanup || minimum_idle_minutes.is_some())
        && tmux_presence == crate::services::platform::tmux::SessionPresence::ProbeFailed
    {
        record_idle_cleanup_preserved(pool, session_key, &tmux_name, "tmux_probe_failed", None)
            .await;
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "tmux_killed": false,
                "tmux_was_alive": null,
                "tmux_session_name": tmux_name,
                "session_row_preserved": true,
                "skipped_provider_activity_guard": true,
                "preserved_reason": "tmux_probe_failed",
            })),
        );
    }
    let tmux_was_alive = tmux_presence == crate::services::platform::tmux::SessionPresence::Present;
    let mut idle_decision_last_seen_nanos = None;
    let mut idle_decision_runtime_activity_nanos = None;
    let mut idle_decision_runtime_activity_age_minutes = None;

    if should_skip_idle_cleanup_for_active_dispatch(active_dispatch_id.as_deref(), reason) {
        let last_seen_nanos =
            dispatched_sessions_db::session_last_seen_unix_nanos_pg(pool, session_key)
                .await
                .unwrap_or(0);
        let runtime_activity_nanos = latest_runtime_activity_unix_nanos(&tmux_name);
        let runtime_activity_age_minutes =
            runtime_activity_age_minutes(runtime_activity_nanos, now_unix_nanos());
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            session_key,
            tmux_session = %tmux_name,
            active_dispatch_id = ?active_dispatch_id,
            last_seen_unix_nanos = last_seen_nanos,
            runtime_activity_unix_nanos = runtime_activity_nanos,
            runtime_activity_age_minutes,
            minimum_idle_minutes,
            reason,
            decision = "skip_active_dispatch",
            "  [{ts}] 🛡 kill-tmux: SKIPPED idle cleanup — active dispatch is still attached, dispatch cleanup owns this session (#3718).",
        );
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "tmux_killed": false,
                "tmux_was_alive": tmux_was_alive,
                "tmux_session_name": tmux_name,
                "session_row_preserved": true,
                "skipped_active_dispatch_guard": true,
                "runtime_activity_age_minutes": runtime_activity_age_minutes,
                "minimum_idle_minutes": minimum_idle_minutes,
                "active_dispatch_id": active_dispatch_id,
            })),
        );
    }

    // #3053: live-activity guard. idle-kill selects on COALESCE(last_heartbeat,
    // created_at); if the matching heartbeat path silently missed this row,
    // a still-working tmux session can be selected for kill while it is alive.
    // Before killing a live session, compare its idle-kill "last seen" instant
    // against the most recent runtime activity (relay output / generation
    // marker mtime). When runtime activity is NEWER, the session is not idle:
    // refresh the heartbeat and SKIP the kill so the next idle-kill tick no
    // longer selects it. Forced/explicit reasons are not affected — this guard
    // only fires for the idle-cleanup reason shape and a live tmux.
    if tmux_was_alive && reason_is_idle_cleanup && active_dispatch_id.is_none() {
        let last_seen_nanos =
            dispatched_sessions_db::session_last_seen_unix_nanos_pg(pool, session_key)
                .await
                .unwrap_or(0);
        let runtime_activity_nanos = latest_runtime_activity_unix_nanos(&tmux_name);
        let now_nanos = now_unix_nanos();
        let runtime_activity_age_minutes =
            runtime_activity_age_minutes(runtime_activity_nanos, now_nanos);
        idle_decision_last_seen_nanos = Some(last_seen_nanos);
        idle_decision_runtime_activity_nanos = Some(runtime_activity_nanos);
        idle_decision_runtime_activity_age_minutes = runtime_activity_age_minutes;
        if should_skip_idle_kill_for_live_runtime_activity(
            last_seen_nanos,
            runtime_activity_nanos,
            now_nanos,
            minimum_idle_minutes,
        ) {
            let refreshed =
                dispatched_sessions_db::refresh_session_heartbeat_by_key_to_unix_nanos_pg(
                    pool,
                    session_key,
                    runtime_activity_nanos,
                )
                .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                session_key,
                tmux_session = %tmux_name,
                last_seen_unix_nanos = last_seen_nanos,
                runtime_activity_unix_nanos = runtime_activity_nanos,
                runtime_activity_age_minutes,
                minimum_idle_minutes,
                heartbeat_refreshed = refreshed,
                reason,
                decision = "skip_live_output",
                "  [{ts}] 🛡 kill-tmux: SKIPPED idle kill — runtime activity newer than last_heartbeat and still within idle threshold, session is live (#3053). Heartbeat refreshed to runtime activity.",
            );
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "tmux_killed": false,
                    "tmux_was_alive": true,
                    "tmux_session_name": tmux_name,
                    "session_row_preserved": true,
                    "skipped_live_activity_guard": true,
                    "heartbeat_refreshed": refreshed,
                    "runtime_activity_age_minutes": runtime_activity_age_minutes,
                    "minimum_idle_minutes": minimum_idle_minutes,
                    "active_dispatch_id": active_dispatch_id,
                })),
            );
        }
    }

    // The final decision reads the provider-native transcript and live pane,
    // independently of inflight/relay health. Quiet long turns, approval waits,
    // background children and unknown evidence must all survive idle cleanup.
    let mut idle_tmux_kill_result = None;
    if tmux_was_alive && (reason_is_idle_cleanup || minimum_idle_minutes.is_some()) {
        let unoccupied = crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied(
            pool,
            session_key,
        )
        .await;
        let probe_name = tmux_name.clone();
        let probe_reason = reason.to_string();
        let probe = if unoccupied {
            tokio::task::spawn_blocking(move || {
                crate::services::tmux_turn_liveness::kill_proven_idle_provider_session(
                    &probe_name,
                    &probe_reason,
                )
            })
            .await
            .unwrap_or(Err("probe_task_failed"))
        } else {
            Err("session_occupied")
        };
        match probe {
            Ok(killed) => idle_tmux_kill_result = Some(killed),
            Err(preserved_reason) => {
                tracing::info!(
                    session_key,
                    preserved_reason,
                    "idle cleanup preserved provider: idle state not proven"
                );
                record_idle_cleanup_preserved(
                    pool,
                    session_key,
                    &tmux_name,
                    preserved_reason,
                    idle_decision_last_seen_nanos,
                )
                .await;
                return (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "tmux_killed": false,
                        "tmux_was_alive": true,
                        "tmux_session_name": tmux_name,
                        "session_row_preserved": true,
                        "skipped_provider_activity_guard": true,
                        "preserved_reason": preserved_reason,
                    })),
                );
            }
        }
    }

    let tmux_killed = if let Some(killed) = idle_tmux_kill_result {
        killed
    } else if tmux_was_alive {
        crate::services::platform::tmux::kill_session(&tmux_name, reason)
    } else {
        false
    };

    // #3052/#3693: a tmux-only idle cleanup must not silently claim
    // "preserved for resume". For Claude TUI, selector presence alone is not
    // enough: the next launch only resumes when the selected UUID has a
    // transcript under the persisted cwd; otherwise it forces a fresh UUID.
    // Non-Claude providers keep the existing selector-presence contract.
    let resumable = match dispatched_sessions_db::load_provider_session_ids_pg(
        pool,
        session_key,
        effective_provider_name,
    )
    .await
    {
        Ok(Some(ids)) => {
            let resumable = provider_resume_selector_is_effective(effective_provider_name, &ids);
            record_raw_provider_transcript_len_watermark_if_observed(
                pool,
                &ids.resolved_session_key,
                effective_provider_name,
                &ids,
                None,
                RawProviderTranscriptObservationMode::GrowthFlagOnly,
            )
            .await;
            resumable
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                "  [kill-tmux] failed to verify resume selector for {}: {}",
                session_key,
                error
            );
            false
        }
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    if resumable {
        tracing::info!(
            "  [{ts}] ✂ kill-tmux: session={}, tmux_killed={}, tmux_was_alive={}, active_dispatch_id={:?} (DB row preserved for resume, resumable=true)",
            session_key,
            tmux_killed,
            tmux_was_alive,
            active_dispatch_id
        );
    } else {
        tracing::info!(
            "  [{ts}] ✂ kill-tmux: session={}, tmux_killed={}, tmux_was_alive={}, active_dispatch_id={:?} (DB row retained but no effective provider resume selector present, resumable=false)",
            session_key,
            tmux_killed,
            tmux_was_alive,
            active_dispatch_id
        );
    }

    // #2861: when the tmux session is already gone, the row is a zombie — it
    // claims a live process that no longer exists. Reconcile it to
    // `disconnected` (selectors preserved) so idle-kill stops re-selecting it
    // every tick and starving genuinely-alive idle sessions behind it. Only
    // rows with no in-flight dispatch are touched (force-kill owns those).
    let mut session_row_disconnected = false;
    if !tmux_was_alive && active_dispatch_id.is_none() {
        session_row_disconnected =
            dispatched_sessions_db::reconcile_orphaned_tmuxless_session_pg(pool, session_key).await;
        if session_row_disconnected {
            tracing::info!(
                "  [{ts}] ↪ kill-tmux: tmux already gone for {} — reconciled stale row to disconnected (selectors preserved)",
                session_key
            );
            crate::services::termination_audit::record_termination_with_handles(
                state.pg_pool_ref(),
                session_key,
                None,
                "kill_tmux_api",
                "stale_tmux_reconcile",
                Some("tmux already gone; idle row reconciled to disconnected"),
                None,
                None,
                Some(false),
            );
        }
    }

    if tmux_killed {
        let termination_reason_code = classify_session_termination_reason(reason);
        crate::services::termination_audit::record_termination_with_handles(
            state.pg_pool_ref(),
            session_key,
            active_dispatch_id.as_deref(),
            "kill_tmux_api",
            termination_reason_code,
            Some(reason),
            None,
            None,
            Some(false),
        );
    }

    if reason_is_idle_cleanup {
        if idle_decision_last_seen_nanos.is_none() {
            idle_decision_last_seen_nanos = Some(
                dispatched_sessions_db::session_last_seen_unix_nanos_pg(pool, session_key)
                    .await
                    .unwrap_or(0),
            );
        }
        if idle_decision_runtime_activity_nanos.is_none() {
            let runtime_activity_nanos = latest_runtime_activity_unix_nanos(&tmux_name);
            idle_decision_runtime_activity_nanos = Some(runtime_activity_nanos);
            idle_decision_runtime_activity_age_minutes =
                runtime_activity_age_minutes(runtime_activity_nanos, now_unix_nanos());
        }
        tracing::info!(
            session_key,
            tmux_session = %tmux_name,
            tmux_killed,
            tmux_was_alive,
            session_row_disconnected,
            resumable,
            active_dispatch_id = ?active_dispatch_id,
            last_seen_unix_nanos = idle_decision_last_seen_nanos,
            runtime_activity_unix_nanos = idle_decision_runtime_activity_nanos,
            runtime_activity_age_minutes = idle_decision_runtime_activity_age_minutes,
            minimum_idle_minutes,
            reason,
            decision = "kill_idle",
            "kill-tmux: idle cleanup decision (#3718)"
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tmux_killed": tmux_killed,
            "tmux_was_alive": tmux_was_alive,
            "tmux_session_name": tmux_name,
            "session_row_preserved": true,
            "session_row_disconnected": session_row_disconnected,
            "resumable": resumable,
            "active_dispatch_id": active_dispatch_id,
        })),
    )
}

/// A kill skipped because idle could not be proven is recorded as an
/// `idle_cleanup_preserved` event and a WARN line (#5993): channel, preserve
/// reason and time since last heartbeat.
async fn record_idle_cleanup_preserved(
    pool: &sqlx::PgPool,
    session_key: &str,
    tmux_name: &str,
    preserved_reason: &str,
    last_seen_nanos: Option<i64>,
) {
    let last_seen_nanos = match last_seen_nanos {
        Some(nanos) => nanos,
        None => dispatched_sessions_db::session_last_seen_unix_nanos_pg(pool, session_key)
            .await
            .unwrap_or(0),
    };
    let channel = crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)
        .map_or_else(|| tmux_name.to_string(), |(_, channel)| channel);
    crate::services::observability::record_idle_cleanup_preserved(
        session_key,
        &channel,
        preserved_reason,
        runtime_activity_age_minutes(last_seen_nanos, now_unix_nanos()),
    );
}

fn reason_is_idle_cleanup_reason(reason: &str) -> bool {
    reason.contains("idle") || reason.contains("자동 정리")
}

pub(super) fn runtime_activity_age_minutes(
    runtime_activity_nanos: i64,
    now_nanos: i64,
) -> Option<u64> {
    if runtime_activity_nanos <= 0 {
        return None;
    }
    if now_nanos <= runtime_activity_nanos {
        return Some(0);
    }
    Some(((now_nanos - runtime_activity_nanos) / 60_000_000_000) as u64)
}

pub(super) fn should_skip_idle_kill_for_live_runtime_activity(
    last_seen_nanos: i64,
    runtime_activity_nanos: i64,
    now_nanos: i64,
    minimum_idle_minutes: Option<u64>,
) -> bool {
    if runtime_activity_nanos <= 0 || runtime_activity_nanos <= last_seen_nanos {
        return false;
    }
    match (
        runtime_activity_age_minutes(runtime_activity_nanos, now_nanos),
        minimum_idle_minutes,
    ) {
        (Some(age), Some(threshold)) => age < threshold,
        (Some(_), None) => true,
        _ => false,
    }
}

pub(super) fn should_skip_idle_cleanup_for_active_dispatch(
    active_dispatch_id: Option<&str>,
    reason: &str,
) -> bool {
    active_dispatch_id.is_some() && reason_is_idle_cleanup_reason(reason)
}
