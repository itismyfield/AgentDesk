use crate::db::dispatched_sessions as dispatched_sessions_db;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
    parse_thread_channel_name, resolve_agent_id_for_session_pg,
};
use crate::db::session_status::{
    is_live_status, is_user_wait_status, normalize_incoming_session_status,
};
use crate::server::routes::AppState;
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

async fn hook_session_pg(
    state: &AppState,
    pool: &sqlx::PgPool,
    body: HookSessionBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut thread_channel_id = normalize_thread_channel_id(body.thread_channel_id.as_deref())
        .or_else(|| {
            body.name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        })
        .or_else(|| parse_thread_channel_id_from_session_key(&body.session_key));
    if thread_channel_id.is_none()
        && let Some(dispatch_id) = body.dispatch_id.as_deref()
    {
        thread_channel_id =
            dispatched_sessions_db::load_dispatch_thread_id_pg(pool, dispatch_id).await;
    }

    let agent_id = resolve_agent_id_for_session_pg(
        pool,
        None,
        Some(&body.session_key),
        body.name.as_deref(),
        thread_channel_id.as_deref(),
        body.dispatch_id.as_deref(),
        body.channel_id.as_deref(),
    )
    .await;

    let status = normalize_incoming_session_status(body.status.as_deref());
    let provider = body.provider.as_deref().unwrap_or("claude");
    // `None` here means "metadata-only hook" — the upsert must preserve the
    // existing `sessions.tokens` (#2045 follow-up: `save_provider_session_id`
    // and similar callers used to zero this column on every metadata update).
    let tokens = body.tokens.map(|t| t as i64);
    let active_dispatch_id = normalize_hook_active_dispatch_id(status, body.dispatch_id.as_deref());
    let instance_id = body
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(state.cluster_instance_id.as_deref());
    let claude_session_id = body.claude_session_id.as_deref().filter(|s| !s.is_empty());
    let raw_provider_session_id = body.session_id.as_deref().filter(|s| !s.is_empty());

    // #2045 Finding 7 (P2): the upsert helper now reports whether the row
    // was inserted in this transaction (`xmax = 0` RETURNING). The earlier
    // pattern of "SELECT exists, then upsert" raced under cluster hand-off
    // and could broadcast `dispatched_session_new` twice for the same
    // session_key — once per concurrent webhook.
    let result = dispatched_sessions_db::upsert_hook_session_pg(
        pool,
        dispatched_sessions_db::HookSessionUpsert {
            session_key: &body.session_key,
            instance_id,
            agent_id: agent_id.as_deref(),
            provider,
            status,
            session_info: body.session_info.as_deref(),
            model: body.model.as_deref(),
            tokens,
            cwd: body.cwd.as_deref(),
            active_dispatch_id: active_dispatch_id.as_deref(),
            thread_channel_id: thread_channel_id.as_deref(),
            claude_session_id,
            raw_provider_session_id,
        },
    )
    .await;

    match result {
        Ok(is_new_session) => {
            let dispatch_id = body.dispatch_id.clone();

            crate::kanban::fire_event_hooks_with_backends(
                None,
                &state.engine,
                "on_session_status_change",
                "OnSessionStatusChange",
                json!({
                    "session_key": body.session_key,
                    "instance_id": instance_id,
                    "status": status,
                    "agent_id": agent_id,
                    "dispatch_id": dispatch_id,
                    "provider": provider,
                }),
            );

            if is_user_wait_status(status)
                && let Some(aid) = agent_id.as_ref()
            {
                spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
            }

            match dispatched_sessions_db::load_session_event_payload_pg(pool, &body.session_key)
                .await
            {
                Ok(Some(payload)) => {
                    if is_new_session {
                        crate::server::ws::emit_event(
                            &state.broadcast_tx,
                            "dispatched_session_new",
                            payload,
                        );
                    } else {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "dispatched_session_update",
                            &body.session_key,
                            payload,
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    "[dispatched-sessions] hook_session_pg: failed to load session payload for {}: {}",
                    body.session_key,
                    error
                ),
            }

            if let Some(aid) = agent_id.as_deref() {
                match dispatched_sessions_db::load_agent_status_payload_pg(
                    pool,
                    aid,
                    &body.session_key,
                )
                .await
                {
                    Ok(Some(agent)) => {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "agent_status",
                            aid,
                            agent,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        "[dispatched-sessions] hook_session_pg: failed to load agent payload for {} / {}: {}",
                        aid,
                        body.session_key,
                        error
                    ),
                }
            }

            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

fn spawn_auto_queue_activate_for_agent(state: AppState, agent_id: String) {
    tokio::spawn(async move {
        // Let the session/dispatch cleanup commit before queue activation probes.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ = crate::server::routes::auto_queue::activate(
            State(state),
            Json(crate::server::routes::auto_queue::ActivateBody {
                run_id: None,
                repo: None,
                agent_id: Some(agent_id),
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
    });
}

fn normalize_hook_active_dispatch_id(status: &str, dispatch_id: Option<&str>) -> Option<String> {
    if !is_live_status(status) {
        return None;
    }

    dispatch_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ── Query / Body types ────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDispatchedSessionsQuery {
    #[serde(rename = "includeMerged")]
    pub include_merged: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDispatchedSessionBody {
    pub status: Option<String>,
    pub active_dispatch_id: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<i64>,
    pub cwd: Option<String>,
    pub session_info: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct HookSessionBody {
    pub session_key: String,
    pub instance_id: Option<String>,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub provider: Option<String>,
    pub session_info: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u64>,
    pub cwd: Option<String>,
    pub dispatch_id: Option<String>,
    pub thread_channel_id: Option<String>,
    pub claude_session_id: Option<String>,
    pub session_id: Option<String>,
    /// Numeric Discord channel id of the originating channel (#2097). Lets
    /// the upsert resolve `sessions.agent_id` by matching `agents.discord_channel_*`
    /// directly, which the session_key-derived channel *name* path can never
    /// satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DeleteSessionQuery {
    pub session_key: String,
    pub provider: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/dispatched-sessions
pub async fn list_dispatched_sessions(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchedSessionsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let include_all = params.include_merged.as_deref() == Some("1");
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::list_dispatched_sessions_pg(pool, include_all).await {
            Ok(mut sessions) => {
                let worker_nodes = match crate::services::cluster::node_registry::list_worker_nodes(
                    pool,
                    state.config.cluster.lease_ttl_secs.max(1),
                )
                .await
                {
                    Ok(nodes) => nodes,
                    Err(error) => {
                        tracing::warn!(
                            "failed to list worker nodes for dispatched session owner routing: {error}"
                        );
                        Vec::new()
                    }
                };
                crate::services::cluster::session_routing::enrich_session_owner_routing(
                    &mut sessions,
                    state.cluster_instance_id.as_deref(),
                    &worker_nodes,
                );
                (StatusCode::OK, Json(json!({"sessions": sessions})))
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// POST /api/dispatched-sessions/webhook — upsert session from dcserver
pub async fn hook_session(
    State(state): State<AppState>,
    Json(body): Json<HookSessionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return hook_session_pg(&state, pool, body).await;
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// DELETE /api/dispatched-sessions/cleanup — manual: delete disconnected sessions
pub async fn cleanup_sessions(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::cleanup_disconnected_sessions_pg(pool).await {
            Ok(result) => (StatusCode::OK, Json(json!({"ok": true, "deleted": result}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// DELETE /api/dispatched-sessions/gc-threads — periodic: delete stale thread sessions
pub async fn gc_thread_sessions(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let deleted = dispatched_sessions_db::gc_stale_thread_sessions_pg(pool).await;
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "gc_threads": deleted.len()})),
        );
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// DELETE /api/dispatched-sessions/webhook — delete a session by session_key
pub async fn delete_session(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::delete_session_by_key_pg(pool, &params.session_key)
            .await
        {
            Ok(result) => {
                if let Some(session_id) = result.session_id {
                    crate::server::ws::emit_event(
                        &state.broadcast_tx,
                        "dispatched_session_disconnect",
                        json!({"id": session_id.to_string()}),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({"ok": true, "deleted": result.deleted})),
                )
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// GET /api/dispatched-sessions/claude-session-id?session_key=...
/// Returns the stored provider session_id for the given session_key.
pub async fn get_claude_session_id(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let _ = dispatched_sessions_db::disconnect_stale_fixed_session_by_key_pg(
            pool,
            &params.session_key,
        )
        .await;

        let provider = params.provider.as_deref().filter(|s| !s.is_empty());
        return match dispatched_sessions_db::load_provider_session_ids_pg(
            pool,
            &params.session_key,
            provider,
        )
        .await
        {
            Ok(Some(ids)) => (
                StatusCode::OK,
                Json(json!({
                    "claude_session_id": ids.claude_session_id,
                    "session_id": ids.claude_session_id,
                    "raw_provider_session_id": ids.raw_provider_session_id,
                })),
            ),
            Ok(None) => (
                StatusCode::OK,
                Json(json!({
                    "claude_session_id": null,
                    "session_id": null,
                    "raw_provider_session_id": null,
                })),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// POST /api/dispatched-sessions/clear-stale-session-id
/// Clears provider session_id from ALL sessions that have the given stale ID.
pub async fn clear_stale_session_id(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(sid) = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("claude_session_id").and_then(|v| v.as_str()))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session_id required"})),
        );
    };
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::clear_stale_session_id_pg(pool, sid).await {
            Ok(result) => (StatusCode::OK, Json(json!({"cleared": result}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// POST /api/dispatched-sessions/clear-session-id
/// Clears claude_session_id for a specific session_key.
/// Used when /clear is called so the next turn doesn't resume a dead session.
pub async fn clear_session_id_by_key(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(key) = body.get("session_key").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session_key required"})),
        );
    };
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::clear_session_id_by_key_pg(pool, key).await {
            Ok(result) => (StatusCode::OK, Json(json!({"cleared": result}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// PATCH /api/dispatched-sessions/:id
pub async fn update_dispatched_session(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDispatchedSessionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        if body.status.is_none()
            && body.active_dispatch_id.is_none()
            && body.model.is_none()
            && body.tokens.is_none()
            && body.cwd.is_none()
            && body.session_info.is_none()
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no fields to update"})),
            );
        }

        let normalized_status = body
            .status
            .as_deref()
            .map(|status| normalize_incoming_session_status(Some(status)));

        return match dispatched_sessions_db::update_session_pg(
            pool,
            id,
            dispatched_sessions_db::UpdateSessionParams {
                status: normalized_status,
                active_dispatch_id: body.active_dispatch_id.as_deref(),
                model: body.model.as_deref(),
                tokens: body.tokens,
                cwd: body.cwd.as_deref(),
                session_info: body.session_info.as_deref(),
            },
        )
        .await
        {
            Ok(0) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "session not found"})),
            ),
            Ok(_) => {
                match dispatched_sessions_db::load_session_update_payload_pg(pool, id).await {
                    Ok(Some(session)) => {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "dispatched_session_update",
                            &id.to_string(),
                            session,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        "[dispatched-sessions] update_dispatched_session: failed to load postgres session payload {}: {}",
                        id,
                        error
                    ),
                }
                (StatusCode::OK, Json(json!({"ok": true})))
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

#[derive(Deserialize)]
pub struct ForceKillOptions {
    /// If true, mark the dispatch as 'failed' and create a retry dispatch.
    #[serde(default)]
    pub retry: bool,
    /// Human-readable reason for the kill (e.g. "idle timeout", "slot reclaim").
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) async fn force_kill_session_impl(
    state: &AppState,
    session_key: &str,
    retry: bool,
) -> (StatusCode, Json<serde_json::Value>) {
    force_kill_session_impl_with_reason(
        state,
        session_key,
        retry,
        "force-kill API 직접 호출 (호출자 미상)",
    )
    .await
}

pub(crate) async fn force_kill_session_impl_with_reason(
    state: &AppState,
    session_key: &str,
    retry: bool,
    reason: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    force_kill_session_impl_with_reason_and_forwarding(
        state,
        &HeaderMap::new(),
        session_key,
        retry,
        reason,
    )
    .await
}

async fn force_kill_session_impl_with_reason_and_forwarding(
    state: &AppState,
    headers: &HeaderMap,
    session_key: &str,
    retry: bool,
    reason: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let session_key = session_key;

    // Parse tmux session name from session_key (format: "hostname:tmux_name")
    let tmux_name = match session_key.split_once(':') {
        Some((_, name)) => name.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid session_key format — expected hostname:tmux_name"})),
            );
        }
    };

    // Parse provider from tmux name
    let provider_info =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);

    // Query session from the authoritative store.
    let provider_name = provider_info
        .as_ref()
        .map(|(provider, _)| provider.as_str());
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let (active_dispatch_id, agent_id, runtime_channel_id, session_provider, owner_instance_id) =
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

    if !crate::services::session_forwarding::is_forwarded_request(headers) {
        match crate::services::session_forwarding::resolve_forward_target(
            state,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_force_kill(
                    state,
                    &target,
                    session_key,
                    retry,
                    reason,
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

    let termination_reason_code = classify_session_termination_reason(reason);

    let lifecycle = force_kill_turn(
        state.health_registry.as_deref(),
        &TurnLifecycleTarget {
            provider: provider_info
                .as_ref()
                .map(|(provider, _)| provider.clone())
                .or_else(|| session_provider.as_deref().and_then(ProviderKind::from_str)),
            channel_id: runtime_channel_id
                .as_deref()
                .and_then(|channel_id| channel_id.parse::<u64>().ok())
                .map(poise::serenity_prelude::ChannelId::new),
            tmux_name: tmux_name.clone(),
        },
        reason,
        termination_reason_code,
    )
    .await;

    // 1. Kill tmux session (or confirm the runtime path already stopped it).
    let tmux_killed = lifecycle.tmux_killed;

    // 2. Clear persistent inflight state by matching tmux_session_name/channel_id.
    let inflight_cleared = lifecycle.inflight_cleared;

    // 3. Update session → disconnected, clear active fields
    // 4. Mark dispatch → failed
    // 5. Optionally create retry dispatch via central path (#108)
    let mut retry_dispatch_id: Option<String> = None;
    let retry_meta = match dispatched_sessions_db::disconnect_session_and_prepare_retry_pg(
        pool,
        session_key,
        active_dispatch_id.as_deref(),
        retry,
    )
    .await
    {
        Ok(meta) => meta.map(|meta| {
            (
                meta.card_id,
                meta.to_agent_id,
                meta.dispatch_type,
                meta.title,
                meta.context,
                meta.retry_count,
            )
        }),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    // Create retry dispatch via central authoritative path (#108)
    let mut retry_skipped_reason: Option<&'static str> = None;
    if let Some((card_id, to_agent_id, dispatch_type, title, context, retry_count)) = retry_meta {
        if retry_count >= FORCE_KILL_RETRY_LIMIT {
            retry_skipped_reason = Some("retry_limit_reached");
            tracing::warn!(
                "[force-kill] retry dispatch skipped for card {}: retry_count={} limit={}",
                card_id,
                retry_count,
                FORCE_KILL_RETRY_LIMIT
            );
        } else {
            let ctx: serde_json::Value = context
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));

            let meta = dispatched_sessions_db::RetryDispatchMeta {
                card_id,
                to_agent_id,
                dispatch_type,
                title,
                context: Some(ctx.to_string()),
                retry_count,
            };
            match dispatched_sessions_db::create_retry_dispatch_pg(pool, &meta).await {
                Ok(new_id) => {
                    retry_dispatch_id = Some(new_id);
                }
                Err(e) => {
                    tracing::warn!(
                        "[force-kill] retry dispatch creation via postgres path failed for card {}: {e}",
                        meta.card_id
                    );
                }
            }
        }
    }

    let queue_activation_requested =
        if retry_dispatch_id.is_none() && retry_skipped_reason.is_none() {
            if let Some(ref aid) = agent_id {
                spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
                true
            } else {
                false
            }
        } else {
            false
        };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚡ force-kill: session={}, tmux_killed={}, inflight_cleared={}, dispatch_failed={:?}, lifecycle={}",
        session_key,
        tmux_killed,
        inflight_cleared,
        active_dispatch_id,
        lifecycle.lifecycle_path
    );

    if tmux_killed && !lifecycle.termination_recorded {
        crate::services::termination_audit::record_termination_with_handles(
            None,
            state.pg_pool_ref(),
            session_key,
            active_dispatch_id.as_deref(),
            "force_kill_api",
            termination_reason_code,
            Some(reason),
            None,
            None,
            Some(false),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tmux_killed": tmux_killed,
            "inflight_cleared": inflight_cleared,
            "lifecycle_path": lifecycle.lifecycle_path,
            "queued_remaining": lifecycle.queue_depth,
            "queue_preserved": lifecycle.queue_preserved,
            "dispatch_failed": active_dispatch_id,
            "retry_dispatch_id": retry_dispatch_id,
            "retry_limit": FORCE_KILL_RETRY_LIMIT,
            "retry_skipped_reason": retry_skipped_reason,
            "queue_activation_requested": queue_activation_requested,
        })),
    )
}

fn classify_session_termination_reason(reason: &str) -> &'static str {
    // #2045 Finding 16 (P3): honor an explicit `auto:` / `manual:` prefix
    // from callers that know their own intent. Substring matching alone
    // mis-classifies user-supplied reasons such as "user idle for 5 min"
    // as `auto_cleanup`, which downstream metrics treat as a system event.
    let trimmed = reason.trim();
    let lower_trimmed = trimmed.to_ascii_lowercase();
    if lower_trimmed.starts_with("auto:") || lower_trimmed.starts_with("system:") {
        return "auto_cleanup";
    }
    if lower_trimmed.starts_with("manual:") || lower_trimmed.starts_with("user:") {
        return "force_kill";
    }

    let lower = lower_trimmed.as_str();
    if lower.contains("idle")
        || lower.contains("auto cleanup")
        || lower.contains("자동 정리")
        || lower.contains("turn cap")
        || lower.contains("cleanup")
    {
        "auto_cleanup"
    } else {
        "force_kill"
    }
}

/// Query parameters for the tmux-output endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct TmuxOutputQuery {
    /// Number of trailing tmux pane lines to capture. Default: 80. Clamped to
    /// the inclusive range [1, 2000] to avoid accidental giant captures.
    pub lines: Option<i32>,
}

const TMUX_OUTPUT_DEFAULT_LINES: i32 = 80;
const TMUX_OUTPUT_MAX_LINES: i32 = 2000;
const FORCE_KILL_RETRY_LIMIT: i64 = 5;

/// GET /api/sessions/{id}/tmux-output?lines=N
///
/// #1067: Skill promotion for watch-agent-turn. Returns the latest N lines of
/// the tmux pane bound to the session identified by the numeric session id
/// (`sessions.id`). Reads the session row to derive `hostname:tmux_name` from
/// `session_key`, then shells out via [`crate::services::platform::tmux::capture_pane`].
pub async fn tmux_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(params): Query<TmuxOutputQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let requested_lines = params.lines.unwrap_or(TMUX_OUTPUT_DEFAULT_LINES);
    let effective_lines = requested_lines.max(1).min(TMUX_OUTPUT_MAX_LINES);

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    // Lookup session row. Prefer Postgres (authoritative) when available.
    let session_row = match dispatched_sessions_db::load_session_by_id_pg(pool, id).await {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    let Some((session_key, agent_id, provider, status, owner_instance_id)) = session_row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("session #{id} not found"),
                "session_id": id,
            })),
        );
    };

    if !crate::services::session_forwarding::is_forwarded_request(&headers) {
        match crate::services::session_forwarding::resolve_forward_target(
            &state,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_tmux_output(
                    &state,
                    &target,
                    id,
                    effective_lines,
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

    // session_key format: "hostname:tmux_name"
    let tmux_name = match session_key.split_once(':') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "session #{id} session_key does not follow hostname:tmux_name format"
                    ),
                    "session_id": id,
                    "session_key": session_key,
                })),
            );
        }
    };

    let captured_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0);

    // capture_pane takes scroll_back as a negative offset from the pane bottom.
    let recent_output = crate::services::platform::tmux::capture_pane(&tmux_name, -effective_lines);
    let tmux_alive = recent_output.is_some();

    (
        StatusCode::OK,
        Json(json!({
            "session_id": id,
            "session_key": session_key,
            "tmux_name": tmux_name,
            "tmux_alive": tmux_alive,
            "agent_id": agent_id,
            "provider": provider,
            "status": status,
            "lines_requested": requested_lines,
            "lines_effective": effective_lines,
            "recent_output": recent_output.unwrap_or_default(),
            "captured_at_ms": captured_at_ms,
        })),
    )
}

/// POST /api/sessions/{session_key}/force-kill
///
/// Atomically: kill tmux session + clear inflight file + set session disconnected
/// + mark active dispatch failed. Optionally creates a retry dispatch.
pub async fn force_kill_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_key): Path<String>,
    Json(body): Json<ForceKillOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    let reason = body.reason.as_deref().unwrap_or("force-kill API invoked");
    force_kill_session_impl_with_reason_and_forwarding(
        &state,
        &headers,
        &session_key,
        body.retry,
        reason,
    )
    .await
}

#[derive(Deserialize)]
pub struct KillTmuxOptions {
    /// Human-readable reason for the kill (e.g. "idle 15시간 초과").
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /api/sessions/{session_key}/kill-tmux
///
/// Tmux-only kill: terminates the tmux session but leaves the DB session row
/// intact (status preserved, `claude_session_id`/`raw_provider_session_id`
/// untouched, `active_dispatch_id` untouched). Designed for idle cleanup paths
/// that want the next user turn to be able to resume the provider session via
/// recap rather than start a fresh conversation.
///
/// Caveat vs. force-kill: this does NOT mark any active dispatch failed and does
/// NOT clear inflight state. It is only safe to call on sessions whose
/// `active_dispatch_id IS NULL` (i.e. nothing in flight); the idle-kill policy
/// already enforces that for its `idleSessions` query.
pub async fn kill_tmux_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_key): Path<String>,
    Json(body): Json<KillTmuxOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    let reason = body.reason.as_deref().unwrap_or("kill-tmux API invoked");
    kill_tmux_session_impl(&state, &headers, &session_key, reason).await
}

/// #3053: most-recent runtime activity instant for a tmux session, as a
/// unix-epoch nanosecond count. Considers the relay output (`jsonl`) file mtime,
/// the `.generation` marker mtime, and (best-effort) the provider transcript
/// mtime. These files are touched by the live wrapper/relay even when the
/// session-key heartbeat path silently misses the idle-kill row, so they are a
/// reliable "is this tmux actually doing work?" signal at kill time. Returns 0
/// when nothing is observable.
fn latest_runtime_activity_unix_nanos(tmux_session_name: &str) -> i64 {
    fn mtime_nanos(path: &str) -> i64 {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
            .unwrap_or(0)
    }

    let mut latest = 0i64;

    // Relay output (jsonl) — written by the live wrapper on every provider event.
    if let Some(output_path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl")
    {
        latest = latest.max(mtime_nanos(&output_path));
    }
    // `.generation` marker — written once per spawn; covers fresh sessions whose
    // jsonl has not yet been created.
    if let Some(generation_path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "generation")
    {
        latest = latest.max(mtime_nanos(&generation_path));
    }

    latest
}

async fn kill_tmux_session_impl(
    state: &AppState,
    headers: &HeaderMap,
    session_key: &str,
    reason: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let tmux_name = match session_key.split_once(':') {
        Some((_, name)) => name.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid session_key format — expected hostname:tmux_name"})),
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
    let (active_dispatch_id, _agent_id, _runtime_channel_id, _session_provider, owner_instance_id) =
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

    if !crate::services::session_forwarding::is_forwarded_request(headers) {
        match crate::services::session_forwarding::resolve_forward_target(
            state,
            owner_instance_id.as_deref(),
            pool,
        )
        .await
        {
            crate::services::session_forwarding::ForwardResolution::Local => {}
            crate::services::session_forwarding::ForwardResolution::Forward(target) => {
                return crate::services::session_forwarding::forward_kill_tmux(
                    state,
                    &target,
                    session_key,
                    reason,
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

    let tmux_was_alive = crate::services::platform::tmux::has_session(&tmux_name);

    // #3053: live-activity guard. idle-kill selects on COALESCE(last_heartbeat,
    // created_at); if the matching heartbeat path silently missed this row,
    // a still-working tmux session can be selected for kill while it is alive.
    // Before killing a live session, compare its idle-kill "last seen" instant
    // against the most recent runtime activity (relay output / generation
    // marker mtime). When runtime activity is NEWER, the session is not idle:
    // refresh the heartbeat and SKIP the kill so the next idle-kill tick no
    // longer selects it. Forced/explicit reasons are not affected — this guard
    // only fires for the idle-cleanup reason shape and a live tmux.
    let reason_is_idle_cleanup = reason.contains("idle") || reason.contains("자동 정리");
    if tmux_was_alive && reason_is_idle_cleanup && active_dispatch_id.is_none() {
        let last_seen_nanos =
            dispatched_sessions_db::session_last_seen_unix_nanos_pg(pool, session_key)
                .await
                .unwrap_or(0);
        let runtime_activity_nanos = latest_runtime_activity_unix_nanos(&tmux_name);
        if runtime_activity_nanos > 0 && runtime_activity_nanos > last_seen_nanos {
            let refreshed =
                dispatched_sessions_db::refresh_session_heartbeat_by_key_pg(pool, session_key)
                    .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                session_key,
                tmux_session = %tmux_name,
                last_seen_unix_nanos = last_seen_nanos,
                runtime_activity_unix_nanos = runtime_activity_nanos,
                heartbeat_refreshed = refreshed,
                "  [{ts}] 🛡 kill-tmux: SKIPPED idle kill — runtime activity newer than last_heartbeat, session is live (#3053). Heartbeat refreshed.",
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
                    "active_dispatch_id": active_dispatch_id,
                })),
            );
        }
    }

    let tmux_killed = if tmux_was_alive {
        crate::services::platform::tmux::kill_session(&tmux_name, reason)
    } else {
        false
    };

    // #3052: a tmux-only idle cleanup must not silently claim "preserved for
    // resume". Verify the provider resume selector is actually present in the
    // DB row before logging that claim. Either selector column
    // (claude_session_id namespaced selector or raw_provider_session_id native
    // fallback) is sufficient for provider-native resume.
    let resumable = match dispatched_sessions_db::load_provider_session_ids_pg(
        pool,
        session_key,
        provider_name,
    )
    .await
    {
        Ok(Some(ids)) => {
            let has_claude_selector = ids
                .claude_session_id
                .as_deref()
                .map(|value| !value.is_empty())
                .unwrap_or(false);
            let has_raw_selector = ids
                .raw_provider_session_id
                .as_deref()
                .map(|value| !value.is_empty())
                .unwrap_or(false);
            has_claude_selector || has_raw_selector
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
            "  [{ts}] ✂ kill-tmux: session={}, tmux_killed={}, tmux_was_alive={}, active_dispatch_id={:?} (DB row retained but no provider selector present, resumable=false)",
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
                None,
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
            None,
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

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tmux_killed": tmux_killed,
            "tmux_was_alive": tmux_was_alive,
            "tmux_session_name": tmux_name,
            "session_row_preserved": true,
            "session_row_disconnected": session_row_disconnected,
            "active_dispatch_id": active_dispatch_id,
        })),
    )
}
