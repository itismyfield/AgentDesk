use crate::db::dispatched_sessions as dispatched_sessions_db;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::session_agent_resolution::resolve_agent_id_for_session;
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
                let worker_nodes = match crate::server::cluster::list_worker_nodes(
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
                crate::server::cluster_session_routing::enrich_session_owner_routing(
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

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db().cloned() {
        return hook_session_sqlite_for_tests(&state, db, body).await;
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
async fn hook_session_sqlite_for_tests(
    state: &AppState,
    db: crate::db::Db,
    body: HookSessionBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match db.lock() {
        Ok(conn) => conn,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    let thread_channel_id = normalize_thread_channel_id(body.thread_channel_id.as_deref())
        .or_else(|| {
            body.name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        })
        .or_else(|| parse_thread_channel_id_from_session_key(&body.session_key))
        .or_else(|| {
            body.dispatch_id.as_deref().and_then(|dispatch_id| {
                dispatched_sessions_db::load_dispatch_thread_id_sqlite(&conn, dispatch_id)
            })
        });

    let agent_id = resolve_agent_id_for_session(
        &conn,
        None,
        Some(&body.session_key),
        body.name.as_deref(),
        thread_channel_id.as_deref(),
        body.dispatch_id.as_deref(),
    );

    let status = normalize_incoming_session_status(body.status.as_deref());
    let provider = body.provider.as_deref().unwrap_or("claude");
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

    let result = dispatched_sessions_db::upsert_hook_session_sqlite_for_tests(
        &conn,
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
    );

    match result {
        Ok(_) => {
            let dispatch_id = body.dispatch_id.clone();
            drop(conn);

            crate::kanban::fire_event_hooks(
                &db,
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

            if is_user_wait_status(status) {
                if let Some(aid) = agent_id {
                    spawn_auto_queue_activate_for_agent(state.clone(), aid);
                }
            }

            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
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
            Json(json!({"ok": true, "gc_threads": deleted})),
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
pub struct ForceKillBody {
    pub session_key: String,
    /// If true, mark the dispatch as 'failed' and create a retry dispatch.
    #[serde(default)]
    pub retry: bool,
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

/// Legacy body-based wrapper retained for compatibility tests and direct callers.
///
/// This helper is no longer exposed as an HTTP route; use
/// `POST /api/sessions/{session_key}/force-kill` instead.
pub async fn force_kill_session_legacy(
    State(state): State<AppState>,
    Json(body): Json<ForceKillBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    force_kill_session_impl(&state, &body.session_key, body.retry).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::Value;
    use std::ffi::OsString;
    use std::process::Command;
    use std::sync::MutexGuard;

    fn test_db() -> Db {
        crate::db::test_db()
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatched_sessions_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatched sessions tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatched sessions tests",
            )
            .await
            .expect("apply postgres migration")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatched sessions tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn response_json(resp: Json<Value>) -> Value {
        resp.0
    }

    async fn seed_agent_pg(pool: &sqlx::PgPool, agent_id: &str) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ($1, $2, 'codex', $3, NOW(), NOW())",
        )
        .bind(agent_id)
        .bind(format!("Agent {agent_id}"))
        .bind("123456789012345678")
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_card_pg(pool: &sqlx::PgPool, card_id: &str, dispatch_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Force Kill Card', $2, $3, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(status)
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_dispatch_pg(
        pool: &sqlx::PgPool,
        dispatch_id: &str,
        card_id: &str,
        agent_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, retry_count, created_at, updated_at)
             VALUES ($1, $2, $3, 'implementation', 'pending', 'Recover me', '{}', 0, NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_session_pg(
        pool: &sqlx::PgPool,
        session_key: &str,
        agent_id: &str,
        dispatch_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, status, active_dispatch_id, provider, last_heartbeat, created_at)
             VALUES ($1, $2, 'turn_active', $3, 'codex', NOW(), NOW())",
        )
        .bind(session_key)
        .bind(agent_id)
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_session_without_dispatch_pg(
        pool: &sqlx::PgPool,
        session_key: &str,
        agent_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, status, provider, last_heartbeat, created_at)
             VALUES ($1, $2, 'turn_active', 'codex', NOW(), NOW())",
        )
        .bind(session_key)
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn count_message_outbox_rows_pg(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn count_termination_events_pg(pool: &sqlx::PgPool, session_key: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM session_termination_events WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn force_kill_session_path_route_retries_active_dispatch_pg_path() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let db = test_db();
        let engine = test_engine(&db);
        let mut state = AppState::test_state(db, engine);
        state.pg_pool = Some(pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ($1, $2, 'codex', $3, NOW(), NOW())",
        )
        .bind("agent-force-pg")
        .bind("Agent agent-force-pg")
        .bind("123456789012345678")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Force Kill Card', 'requested', $2, NOW(), NOW())",
        )
        .bind("card-force-pg")
        .bind("dispatch-force-pg")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                context,
                retry_count,
                created_at,
                updated_at
            ) VALUES ($1, $2, $3, 'implementation', 'pending', 'Recover me', '{}', 0, NOW(), NOW())",
        )
        .bind("dispatch-force-pg")
        .bind("card-force-pg")
        .bind("agent-force-pg")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions (
                session_key,
                agent_id,
                status,
                active_dispatch_id,
                provider,
                last_heartbeat,
                created_at
            ) VALUES ($1, $2, 'turn_active', $3, 'codex', NOW(), NOW())",
        )
        .bind("host:codex-agent-force-pg")
        .bind("agent-force-pg")
        .bind("dispatch-force-pg")
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = force_kill_session(
            State(state),
            HeaderMap::new(),
            Path("host:codex-agent-force-pg".to_string()),
            Json(ForceKillOptions {
                retry: true,
                reason: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        let retry_dispatch_id = body["retry_dispatch_id"].as_str().unwrap().to_string();
        assert!(!retry_dispatch_id.is_empty());
        assert_eq!(body["queue_activation_requested"], false);

        let session_state = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind("host:codex-agent-force-pg")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(session_state.0, "disconnected");
        assert!(session_state.1.is_none());

        let old_dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-force-pg")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(old_dispatch_status, "failed");

        let new_dispatch = sqlx::query_as::<_, (String, i64)>(
            "SELECT status, retry_count::BIGINT FROM task_dispatches WHERE id = $1",
        )
        .bind(&retry_dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(new_dispatch.0, "pending");
        assert_eq!(new_dispatch.1, 1);

        let latest_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
        )
        .bind("card-force-pg")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            latest_dispatch_id.as_deref(),
            Some(retry_dispatch_id.as_str())
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_kill_session_legacy_wrapper_pg_uses_same_core_without_retry() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        seed_agent_pg(&pool, "agent-force-legacy").await;
        seed_card_pg(
            &pool,
            "card-force-legacy",
            "dispatch-force-legacy",
            "requested",
        )
        .await;
        seed_dispatch_pg(
            &pool,
            "dispatch-force-legacy",
            "card-force-legacy",
            "agent-force-legacy",
        )
        .await;
        seed_session_pg(
            &pool,
            "host:claude-agent-force-legacy",
            "agent-force-legacy",
            "dispatch-force-legacy",
        )
        .await;

        let (status, body) = force_kill_session_legacy(
            State(state),
            Json(ForceKillBody {
                session_key: "host:claude-agent-force-legacy".to_string(),
                retry: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        assert_eq!(body["lifecycle_path"], "direct-fallback");
        assert!(body["retry_dispatch_id"].is_null());
        assert_eq!(body["queue_activation_requested"], true);

        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-force-legacy")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(dispatch_status, "failed");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_kill_session_pg_clears_matching_inflight_and_live_tmux() {
        let _env_lock = env_lock();
        if Command::new("tmux").arg("-V").output().is_err() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let tmux_name = format!("AgentDesk-codex-force-kill-{}", std::process::id());
        let session_key = format!("host:{tmux_name}");
        let inflight_dir = temp
            .path()
            .join("runtime")
            .join("discord_inflight")
            .join("codex");
        std::fs::create_dir_all(&inflight_dir).unwrap();
        let inflight_path = inflight_dir.join("force-kill.json");
        std::fs::write(
            &inflight_path,
            serde_json::to_string(&json!({
                "version": 1,
                "provider": "codex",
                "channel_id": 123456789012345678u64,
                "channel_name": "force-kill",
                "request_owner_user_id": 1u64,
                "user_msg_id": 2u64,
                "current_msg_id": 3u64,
                "current_msg_len": 0,
                "user_text": "kill this",
                "session_id": null,
                "tmux_session_name": tmux_name,
                "output_path": null,
                "input_fifo_path": null,
                "last_offset": 0u64,
                "full_response": "",
                "response_sent_offset": 0,
                "started_at": "2026-04-06 10:20:00",
                "updated_at": "2026-04-06 10:20:01"
            }))
            .unwrap(),
        )
        .unwrap();

        let tmux_started = Command::new("tmux")
            .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !tmux_started {
            return;
        }

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        seed_agent_pg(&pool, "agent-force-live").await;
        seed_session_without_dispatch_pg(&pool, &session_key, "agent-force-live").await;

        let (status, body) = force_kill_session(
            State(state),
            HeaderMap::new(),
            Path(session_key.clone()),
            Json(ForceKillOptions {
                retry: false,
                reason: None,
            }),
        )
        .await;

        let body = response_json(body);
        let tmux_still_alive = Command::new("tmux")
            .args(["has-session", "-t", &tmux_name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if tmux_still_alive {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .status();
        }

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tmux_killed"], true);
        assert_eq!(body["inflight_cleared"], true);
        assert_eq!(body["lifecycle_path"], "direct-fallback");
        assert_eq!(body["queue_activation_requested"], true);
        assert!(
            !tmux_still_alive,
            "tmux session should be gone after force-kill"
        );
        assert!(
            !inflight_path.exists(),
            "matching inflight file should be deleted"
        );

        let session_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(session_status, "disconnected");
        assert_eq!(count_message_outbox_rows_pg(&pool).await, 1);
        assert_eq!(count_termination_events_pg(&pool, &session_key).await, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn force_kill_session_pg_skips_notify_and_audit_when_tmux_is_already_gone() {
        let _env_lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let session_key = format!(
            "host:AgentDesk-codex-force-kill-dead-{}",
            std::process::id()
        );

        seed_agent_pg(&pool, "agent-force-dead").await;
        seed_session_without_dispatch_pg(&pool, &session_key, "agent-force-dead").await;

        let (status, body) = force_kill_session(
            State(state),
            HeaderMap::new(),
            Path(session_key.clone()),
            Json(ForceKillOptions {
                retry: false,
                reason: Some("idle 60분 초과 — 자동 정리".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        assert_eq!(body["tmux_killed"], false);
        assert_eq!(body["inflight_cleared"], false);
        assert_eq!(body["queue_activation_requested"], true);

        let session_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(session_status, "disconnected");
        assert_eq!(count_message_outbox_rows_pg(&pool).await, 0);
        assert_eq!(count_termination_events_pg(&pool, &session_key).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_pg_does_not_auto_complete_implementation_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let card_id = "card-1";
        let dispatch_id = "dispatch-1";
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Test Card', 'requested', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ($1, $2, 'ch-td', 'implementation', 'pending', 'Test Card', '{}', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-1".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-1".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(42),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        // implementation dispatches must NOT be auto-completed on idle —
        // they require explicit completion from turn_bridge
        let card_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // Card may move to in_progress via kanban-rules policy when session reports working,
        // but must NOT advance to review (which would happen if idle auto-completed the dispatch).
        assert!(
            card_status == "requested" || card_status == "in_progress",
            "card should not advance past in_progress, got: {card_status}"
        );
        assert_eq!(
            dispatch_status, "pending",
            "implementation dispatch should stay pending on idle"
        );
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle dispatch sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_pg_does_not_auto_complete_rework_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let card_id = "card-rework";
        let dispatch_id = "dispatch-rework";
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Rework Card', 'rework', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ($1, $2, 'ch-td', 'rework', 'pending', 'Rework Card', '{}', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-rework".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-rework".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(10),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        // rework dispatches must NOT be auto-completed on idle —
        // they require explicit completion from turn_bridge
        let card_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-rework'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // Card stays in rework — must NOT advance to review (which would happen
        // if idle auto-completed the rework dispatch).
        assert_eq!(card_status, "rework", "card should not advance past rework");
        assert_eq!(
            dispatch_status, "pending",
            "rework dispatch should stay pending on idle"
        );
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle rework sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_pg_does_not_auto_complete_pending_review_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let card_id = "card-review";
        let dispatch_id = "dispatch-review";
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ($1, 'Review Card', 'review', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ($1, $2, 'project-agentdesk', 'review', 'pending', '[Review R1] Review Card', '{}', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-review".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-review".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(11),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        let dispatch_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let dispatch_result = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result FROM task_dispatches WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-review'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // review dispatches must stay pending until an explicit review-verdict arrives
        assert_eq!(dispatch_status, "pending");
        assert!(dispatch_result.is_none());
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle review sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn idle_hook_without_dispatch_id_pg_preserves_existing_dispatch_binding() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-sticky', 'Sticky Card', 'in_progress', 'dispatch-sticky', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES ('dispatch-sticky', 'card-sticky', 'project-agentdesk', 'implementation', 'completed', 'Sticky', '{}', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-sticky".to_string()),
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (working_refresh_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: Some(9),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_refresh_status, StatusCode::OK);
        let (idle_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(17),
                cwd: None,
                dispatch_id: Some("dispatch-sticky".to_string()),
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        let (idle_refresh_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(33),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_refresh_status, StatusCode::OK);

        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-sticky'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_dispatch_id.as_deref(), Some("dispatch-sticky"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn heartbeat_without_dispatch_id_pg_does_not_resurrect_cleared_binding() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES ('session-cleared', 'codex', 'turn_active', 'dispatch-cleared', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE sessions SET active_dispatch_id = NULL WHERE session_key = 'session-cleared'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-cleared".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: Some(21),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let active_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-cleared'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_dispatch_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn hook_session_turn_activity_pg_refreshes_last_heartbeat_from_created_at() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let seeded_created_at: chrono::DateTime<chrono::Utc> = "2026-04-09T01:02:03Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, created_at, last_heartbeat)
             VALUES ('session-heartbeat', 'codex', 'idle', $1, NULL)",
        )
        .bind(seeded_created_at)
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-heartbeat".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (created_at, last_heartbeat) = sqlx::query_as::<
            _,
            (
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >(
            "SELECT created_at, last_heartbeat
             FROM sessions
             WHERE session_key = 'session-heartbeat'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(created_at, seeded_created_at);
        assert!(
            last_heartbeat.is_some_and(|value| value > created_at),
            "turn activity must refresh last_heartbeat beyond the original created_at"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn parse_thread_channel_name_extracts_parent_and_thread_id() {
        let result = parse_thread_channel_name("adk-cc-t1485400795435372796");
        assert_eq!(result, Some(("adk-cc", "1485400795435372796")));
    }

    #[test]
    fn parse_thread_channel_name_with_complex_parent() {
        let result = parse_thread_channel_name("cookingheart-dev-cc-t1485503849761607815");
        assert_eq!(result, Some(("cookingheart-dev-cc", "1485503849761607815")));
    }

    #[test]
    fn parse_thread_channel_name_returns_none_for_regular_channel() {
        assert_eq!(parse_thread_channel_name("adk-cc"), None);
        assert_eq!(parse_thread_channel_name("cookingheart-dev-cc"), None);
    }

    #[test]
    fn parse_thread_channel_name_returns_none_for_short_suffix() {
        // "-t" followed by less than 15 digits is not a thread ID
        assert_eq!(parse_thread_channel_name("test-t123"), None);
    }

    #[test]
    fn parse_thread_channel_id_from_session_key_extracts_thread_id() {
        assert_eq!(
            parse_thread_channel_id_from_session_key(
                "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011"
            )
            .as_deref(),
            Some("1485506232256168011")
        );
    }

    #[test]
    fn parse_thread_channel_id_from_session_key_rejects_non_thread_suffix() {
        assert_eq!(
            parse_thread_channel_id_from_session_key("mac-mini:AgentDesk-claude-adk-cc-token-test"),
            None
        );
    }

    #[tokio::test]
    async fn gc_thread_sessions_handler_pg_reports_deleted_legacy_thread_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'codex', 'idle', NOW() - INTERVAL '2 hours', NOW() - INTERVAL '2 hours')",
        )
        .bind("mac-mini:AgentDesk-codex-adk-cdx-t1490653467734446120")
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = gc_thread_sessions(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_json(body)["gc_threads"], 1);

        let remaining = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_resolves_agent_from_parent_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Post session with thread channel name
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796".to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("thread work".to_string()),
                name: Some("adk-cc-t1485400795435372796".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind("mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485400795435372796"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_resolves_alt_channel_agent_from_session_key_fallback() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread work".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-1".to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485506232256168011"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_session_pg_resolves_agent_from_dispatch_when_tmux_channel_is_truncated() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(long_channel);
        let session_key = format!("mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-skillmanager', 'SkillManager', $1)",
        )
        .bind(long_channel)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-dispatch-fallback', 'Dispatch Fallback', 'in_progress', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-dispatch-fallback', 'card-dispatch-fallback', 'project-skillmanager', 'implementation', 'dispatched', 'Dispatch fallback', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("dispatch fallback".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-dispatch-fallback".to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let agent_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT agent_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-skillmanager"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_session_pg_ignores_missing_agent_from_dispatch_fallback() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(long_channel);
        let session_key = format!("mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-missing-dispatch-agent', 'Missing Dispatch Agent', 'in_progress', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-missing-dispatch-agent', 'card-missing-dispatch-agent', 'project-missing-agent', 'implementation', 'dispatched', 'Dispatch fallback', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("dispatch fallback".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-missing-dispatch-agent".to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");

        let agent_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT agent_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_session_pg_ignores_explicit_agent_id_without_other_resolution_context() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name("project-skillmanager-extremely-verbose-channel-cdx");
        let session_key = format!("codex/hash123/mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-spoofed', 'Spoofed Agent', 'spoofed-channel')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                instance_id: None,
                agent_id: Some("project-spoofed".to_string()),
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("explicit agent".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");

        let agent_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT agent_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_resolves_agent_from_thread_id_when_parent_channel_is_truncated() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());
        let thread_id = "1487044675541012490";
        let long_parent_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name(&format!("{long_parent_channel}-t{thread_id}"));
        let session_key = format!("mac-mini:{tmux_name}");

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_alt)
             VALUES ('project-skillmanager', 'SkillManager', $1)",
        )
        .bind(long_parent_channel)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-thread-fallback', 'Thread Fallback', 'in_progress', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-thread-fallback', 'card-thread-fallback', 'project-skillmanager', 'implementation', 'dispatched', 'Thread fallback', $1, NOW(), NOW())",
        )
        .bind(thread_id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread fallback".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, stored_thread_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-skillmanager"));
        assert_eq!(stored_thread_id.as_deref(), Some(thread_id));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn thread_session_pg_accepts_explicit_thread_channel_id_without_thread_name() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread work".to_string()),
                name: Some("adk-cdx".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: Some("1485506232256168011".to_string()),
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485506232256168011"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn direct_channel_session_pg_keeps_agent_mapping_without_thread_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("direct channel work".to_string()),
                name: Some("adk-cdx".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (agent_id, thread_channel_id) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn stale_local_tmux_session_pg_is_filtered_from_active_dispatch_list() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let hostname = crate::services::platform::hostname_short();
        let session_key = format!("{hostname}:AgentDesk-stale-test-{}", std::process::id());

        sqlx::query(
            "INSERT INTO agents (id, name, name_ko, provider, avatar_emoji, status, created_at)
             VALUES ('ch-ad', 'AD', 'AD', 'claude', '🤖', 'idle', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, active_dispatch_id, last_heartbeat)
             VALUES ($1, 'ch-ad', 'claude', 'turn_active', 'stale session', 'dispatch-stale', NOW())",
        )
        .bind(&session_key)
        .execute(&pool)
        .await
        .unwrap();

        let (status, Json(body)) = list_dispatched_sessions(
            State(state),
            Query(ListDispatchedSessionsQuery {
                include_merged: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"].as_array().unwrap().len(), 0);

        pool.close().await;
        pg_db.drop().await;
    }

    // #1067: sessions_tmux_output tests — watch-agent-turn skill promotion.
    // #1238: Migrated to PG fixtures. `tmux_output` now requires `pg_pool_ref()`
    // — without a populated pool the route returns 500 ("postgres pool unavailable")
    // and the 404 assertion fails.
    #[tokio::test]
    async fn sessions_tmux_output_pg_returns_404_for_unknown_session_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let db = test_db();
        let engine = test_engine(&db);
        let mut state = AppState::test_state(db, engine);
        state.pg_pool = Some(pool.clone());

        let (status, body) = tmux_output(
            State(state),
            HeaderMap::new(),
            Path(999_999),
            Query(TmuxOutputQuery { lines: None }),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: Value = response_json(body);
        assert_eq!(body["session_id"], 999_999);
        assert!(
            body["error"]
                .as_str()
                .map(|s| s.contains("not found"))
                .unwrap_or(false)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_tmux_output_pg_shape_for_seeded_session_without_live_tmux() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let tmux_name = format!("AgentDesk-codex-1067-output-{}", std::process::id());
        let session_key = format!("mac-mini:{tmux_name}");

        seed_agent_pg(&pool, "agent-1067").await;
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'agent-1067', 'codex', 'turn_active', NOW(), NOW())",
        )
        .bind(&session_key)
        .execute(&pool)
        .await
        .unwrap();
        let session_id =
            sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = AppState::test_state_with_pg(db, engine, pool.clone());

        let (status, body) = tmux_output(
            State(state),
            HeaderMap::new(),
            Path(session_id),
            Query(TmuxOutputQuery { lines: Some(20) }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body: Value = response_json(body);
        assert_eq!(body["session_id"], session_id);
        assert_eq!(body["session_key"], session_key);
        assert_eq!(body["tmux_name"], tmux_name);
        assert_eq!(body["agent_id"], "agent-1067");
        assert_eq!(body["provider"], "codex");
        assert_eq!(body["status"], "turn_active");
        assert_eq!(body["lines_requested"], 20);
        assert_eq!(body["lines_effective"], 20);
        // tmux session was never created, so capture_pane returns None → empty string + alive=false.
        assert_eq!(body["tmux_alive"], false);
        assert_eq!(body["recent_output"], "");
        assert!(body["captured_at_ms"].as_i64().unwrap() > 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_tmux_output_pg_clamps_lines_to_allowed_range() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let session_key = format!("mac-mini:AgentDesk-codex-1067-clamp-{}", std::process::id());

        seed_agent_pg(&pool, "agent-1067-clamp").await;
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'agent-1067-clamp', 'codex', 'idle', NOW(), NOW())",
        )
        .bind(&session_key)
        .execute(&pool)
        .await
        .unwrap();
        let session_id =
            sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = AppState::test_state_with_pg(db, engine, pool.clone());

        let (status_hi, body_hi) = tmux_output(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id),
            Query(TmuxOutputQuery { lines: Some(9_999) }),
        )
        .await;
        assert_eq!(status_hi, StatusCode::OK);
        let body_hi: Value = response_json(body_hi);
        assert_eq!(body_hi["lines_requested"], 9_999);
        assert_eq!(body_hi["lines_effective"], 2_000);

        let (status_lo, body_lo) = tmux_output(
            State(state),
            HeaderMap::new(),
            Path(session_id),
            Query(TmuxOutputQuery { lines: Some(-42) }),
        )
        .await;
        assert_eq!(status_lo, StatusCode::OK);
        let body_lo: Value = response_json(body_lo);
        assert_eq!(body_lo["lines_requested"], -42);
        assert_eq!(body_lo["lines_effective"], 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_tmux_output_pg_rejects_malformed_session_key() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        // session_key without ":" — conflicts with hostname:tmux_name format.
        let bad_session_key = "no-colon-here".to_string();

        seed_agent_pg(&pool, "agent-1067-bad").await;
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES ($1, 'agent-1067-bad', 'codex', 'idle', NOW(), NOW())",
        )
        .bind(&bad_session_key)
        .execute(&pool)
        .await
        .unwrap();
        let session_id =
            sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&bad_session_key)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = AppState::test_state_with_pg(db, engine, pool.clone());

        let (status, body) = tmux_output(
            State(state),
            HeaderMap::new(),
            Path(session_id),
            Query(TmuxOutputQuery { lines: None }),
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        let body: Value = response_json(body);
        assert_eq!(body["session_id"], session_id);
        assert_eq!(body["session_key"], bad_session_key);

        pool.close().await;
        pg_db.drop().await;
    }

    /// PR #2: `sessions.tokens` must NOT be zeroed by a follow-up
    /// metadata-only hook (e.g. `save_provider_session_id`, which carries
    /// `tokens: None`). Before this fix the upsert ran `tokens = EXCLUDED.tokens`
    /// unconditionally, so any post-turn metadata hook reset the just-saved
    /// snapshot to 0 and made `/api/sessions` always report `tokens: 0` even
    /// after a token-heavy turn. The follow-up also bumps `tokens_updated_at`
    /// only on real snapshots so the recap UI can tell stale columns apart.
    #[tokio::test]
    async fn hook_session_metadata_only_preserves_tokens_and_updated_at() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state_with_pg(db.clone(), engine, pool.clone());

        let session_key = "session-tokens-preserve";

        // 1) Initial turn-end hook with a real token snapshot.
        let (status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: Some("claude".to_string()),
                tokens: Some(42_000),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (initial_tokens, initial_updated_at) =
            sqlx::query_as::<_, (i64, Option<chrono::DateTime<chrono::Utc>>)>(
                "SELECT tokens, tokens_updated_at FROM sessions WHERE session_key = $1",
            )
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(initial_tokens, 42_000);
        assert!(
            initial_updated_at.is_some(),
            "tokens_updated_at must be set on the first explicit token snapshot"
        );

        // 2) Follow-up metadata-only hook (typical: save_provider_session_id)
        //    must not zero out the token column or refresh tokens_updated_at.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status2, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                instance_id: None,
                agent_id: None,
                status: None,
                provider: Some("claude".to_string()),
                session_info: None,
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: Some("sess-abc".to_string()),
                thread_channel_id: None,
                session_id: Some("raw-xyz".to_string()),
            }),
        )
        .await;
        assert_eq!(status2, StatusCode::OK);

        let (after_meta_tokens, after_meta_updated_at) =
            sqlx::query_as::<_, (i64, Option<chrono::DateTime<chrono::Utc>>)>(
                "SELECT tokens, tokens_updated_at FROM sessions WHERE session_key = $1",
            )
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after_meta_tokens, 42_000,
            "metadata-only hook must preserve sessions.tokens (was 42000, found {after_meta_tokens})"
        );
        assert_eq!(
            after_meta_updated_at, initial_updated_at,
            "metadata-only hook must not bump tokens_updated_at"
        );

        // 3) A new explicit snapshot updates both columns and bumps the stamp.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status3, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
                instance_id: None,
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: None,
                name: None,
                model: None,
                tokens: Some(84_000),
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(status3, StatusCode::OK);

        let (final_tokens, final_updated_at) =
            sqlx::query_as::<_, (i64, Option<chrono::DateTime<chrono::Utc>>)>(
                "SELECT tokens, tokens_updated_at FROM sessions WHERE session_key = $1",
            )
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(final_tokens, 84_000);
        assert!(
            final_updated_at > initial_updated_at,
            "explicit snapshot must bump tokens_updated_at forward",
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
