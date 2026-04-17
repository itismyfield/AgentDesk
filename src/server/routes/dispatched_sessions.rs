use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::session_activity::SessionActivityResolver;
use crate::db::agents::resolve_agent_channel_for_provider_on_conn;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
    parse_thread_channel_name, resolve_agent_id_for_session as resolve_session_agent_id,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};

const STALE_FIXED_WORKING_SESSION_MAX_AGE_SQL: &str = "-6 hours";
const STALE_THREAD_SESSION_MAX_AGE_SQL: &str = "-1 hour";
const STALE_THREAD_SESSION_ACTIVE_DISPATCH_MAX_AGE_SQL: &str = "-3 hours";

fn load_dispatch_thread_id(conn: &rusqlite::Connection, dispatch_id: &str) -> Option<String> {
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    normalize_thread_channel_id(thread_id.as_deref())
}

fn spawn_auto_queue_activate_for_agent(state: AppState, agent_id: String) {
    tokio::spawn(async move {
        // Let the session/dispatch cleanup commit before queue activation probes.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ = super::auto_queue::activate(
            State(state),
            Json(super::auto_queue::ActivateBody {
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
    if status.eq_ignore_ascii_case("disconnected") {
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

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct HookSessionBody {
    pub session_key: String,
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
}

#[derive(Debug, Deserialize)]
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let include_all = params.include_merged.as_deref() == Some("1");

    let sql = if include_all {
        "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, s.active_dispatch_id,
                s.model, s.tokens, s.cwd, s.last_heartbeat, s.session_info,
                a.department, a.sprite_number, a.avatar_emoji, a.xp,
                d.name AS department_name, d.name_ko AS department_name_ko, d.color AS department_color,
                s.thread_channel_id
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         ORDER BY s.id"
    } else {
        "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, s.active_dispatch_id,
                s.model, s.tokens, s.cwd, s.last_heartbeat, s.session_info,
                a.department, a.sprite_number, a.avatar_emoji, a.xp,
                d.name AS department_name, d.name_ko AS department_name_ko, d.color AS department_color,
                s.thread_channel_id
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         WHERE s.active_dispatch_id IS NOT NULL
         ORDER BY s.id"
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    struct SessionRow {
        id: i64,
        session_key: Option<String>,
        agent_id: Option<String>,
        provider: Option<String>,
        status: Option<String>,
        active_dispatch_id: Option<String>,
        model: Option<String>,
        tokens: i64,
        cwd: Option<String>,
        last_heartbeat: Option<String>,
        session_info: Option<String>,
        department_id: Option<String>,
        sprite_number: Option<i64>,
        avatar_emoji: Option<String>,
        stats_xp: i64,
        department_name: Option<String>,
        department_name_ko: Option<String>,
        department_color: Option<String>,
        thread_channel_id: Option<String>,
    }

    let rows = stmt
        .query_map([], |row| {
            Ok(SessionRow {
                id: row.get::<_, i64>(0)?,
                session_key: row.get::<_, Option<String>>(1)?,
                agent_id: row.get::<_, Option<String>>(2)?,
                provider: row.get::<_, Option<String>>(3)?,
                status: row.get::<_, Option<String>>(4)?,
                active_dispatch_id: row.get::<_, Option<String>>(5)?,
                model: row.get::<_, Option<String>>(6)?,
                tokens: row.get::<_, i64>(7)?,
                cwd: row.get::<_, Option<String>>(8)?,
                last_heartbeat: row.get::<_, Option<String>>(9)?,
                session_info: row.get::<_, Option<String>>(10)?,
                department_id: row.get::<_, Option<String>>(11)?,
                sprite_number: row.get::<_, Option<i64>>(12)?,
                avatar_emoji: row.get::<_, Option<String>>(13).ok().flatten(),
                stats_xp: row.get::<_, i64>(14).unwrap_or(0),
                department_name: row.get::<_, Option<String>>(15)?,
                department_name_ko: row.get::<_, Option<String>>(16)?,
                department_color: row.get::<_, Option<String>>(17)?,
                thread_channel_id: row.get::<_, Option<String>>(18).ok().flatten(),
            })
        })
        .ok();

    let mut resolver = SessionActivityResolver::new();
    let sessions: Vec<serde_json::Value> = match rows {
        Some(iter) => iter
            .filter_map(|r| r.ok())
            .filter_map(|row| {
                let effective = resolver.resolve(
                    row.session_key.as_deref(),
                    row.status.as_deref(),
                    row.active_dispatch_id.as_deref(),
                    row.last_heartbeat.as_deref(),
                );
                if !include_all && !effective.is_working && effective.active_dispatch_id.is_none() {
                    return None;
                }
                // Hide idle/disconnected thread sessions in default view
                if !include_all && row.thread_channel_id.is_some() && !effective.is_working {
                    return None;
                }
                Some(json!({
                    "id": row.id.to_string(),
                    "session_key": row.session_key,
                    "agent_id": row.agent_id,
                    "provider": row.provider,
                    "status": effective.status,
                    "active_dispatch_id": effective.active_dispatch_id,
                    "model": row.model,
                    "tokens": row.tokens,
                    "cwd": row.cwd,
                    "last_heartbeat": row.last_heartbeat,
                    "session_info": row.session_info,
                    // alias fields for frontend compatibility
                    "linked_agent_id": row.agent_id,
                    "last_seen_at": row.last_heartbeat,
                    "name": row.session_key,
                    // joined agent fields
                    "department_id": row.department_id,
                    "sprite_number": row.sprite_number,
                    "avatar_emoji": row.avatar_emoji.unwrap_or_else(|| "\u{1F916}".to_string()),
                    "stats_xp": row.stats_xp,
                    "connected_at": null,
                    // joined department fields
                    "department_name": row.department_name,
                    "department_name_ko": row.department_name_ko,
                    "department_color": row.department_color,
                    "thread_channel_id": row.thread_channel_id,
                }))
            })
            .collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"sessions": sessions})))
}

/// POST /api/hook/session — upsert session from dcserver
pub async fn hook_session(
    State(state): State<AppState>,
    Json(body): Json<HookSessionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Resolve agent_id from channel name: check discord_channel_id or discord_channel_alt.
    // For thread channels (e.g. "adk-cc-t1485400795435372796"), extract the parent channel
    // name ("adk-cc") and resolve using that.
    let thread_channel_id = normalize_thread_channel_id(body.thread_channel_id.as_deref())
        .or_else(|| {
            body.name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        })
        .or_else(|| parse_thread_channel_id_from_session_key(&body.session_key))
        .or_else(|| {
            body.dispatch_id
                .as_deref()
                .and_then(|dispatch_id| load_dispatch_thread_id(&conn, dispatch_id))
        });

    // /api/hook/session is intentionally auth-exempt, so never trust a client-supplied
    // agent_id from this payload. Resolve ownership from session/channel/dispatch context only.
    let agent_id = resolve_session_agent_id(
        &conn,
        None,
        Some(&body.session_key),
        body.name.as_deref(),
        thread_channel_id.as_deref(),
        body.dispatch_id.as_deref(),
    );

    let status = body.status.as_deref().unwrap_or("working");
    let provider = body.provider.as_deref().unwrap_or("claude");
    let tokens = body.tokens.unwrap_or(0) as i64;
    let active_dispatch_id = normalize_hook_active_dispatch_id(status, body.dispatch_id.as_deref());
    // #107: Normalize empty claude_session_id to None (SQL NULL) so stale empty
    // strings are never persisted — prevents invalid --resume attempts after restart.
    let claude_session_id = body.claude_session_id.as_deref().filter(|s| !s.is_empty());
    let raw_provider_session_id = body.session_id.as_deref().filter(|s| !s.is_empty());
    // Check if session exists before upsert to determine new vs update for WS event
    let is_new_session: bool = conn
        .query_row(
            "SELECT COUNT(*) = 0 FROM sessions WHERE session_key = ?1",
            [&body.session_key],
            |row| row.get(0),
        )
        .unwrap_or(true);

    let result = conn.execute(
        "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, model, tokens, cwd, active_dispatch_id, thread_channel_id, claude_session_id, raw_provider_session_id, last_heartbeat)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now'))
         ON CONFLICT(session_key) DO UPDATE SET
           status = excluded.status,
           provider = excluded.provider,
           session_info = COALESCE(excluded.session_info, sessions.session_info),
           model = COALESCE(excluded.model, sessions.model),
           tokens = excluded.tokens,
           cwd = COALESCE(excluded.cwd, sessions.cwd),
           active_dispatch_id = CASE
             WHEN lower(excluded.status) = 'disconnected' THEN NULL
             WHEN excluded.active_dispatch_id IS NOT NULL THEN excluded.active_dispatch_id
             ELSE sessions.active_dispatch_id
           END,
           agent_id = COALESCE(NULLIF(TRIM(excluded.agent_id), ''), NULLIF(TRIM(sessions.agent_id), '')),
           thread_channel_id = COALESCE(excluded.thread_channel_id, sessions.thread_channel_id),
           claude_session_id = COALESCE(excluded.claude_session_id, sessions.claude_session_id),
           raw_provider_session_id = COALESCE(excluded.raw_provider_session_id, sessions.raw_provider_session_id),
           last_heartbeat = datetime('now')",
        rusqlite::params![
            body.session_key,
            agent_id,
            provider,
            status,
            body.session_info,
            body.model,
            tokens,
            body.cwd,
            active_dispatch_id,
            thread_channel_id,
            claude_session_id,
            raw_provider_session_id,
        ],
    );

    match result {
        Ok(_) => {
            let dispatch_id = body.dispatch_id.clone();
            drop(conn);

            // Fire event hooks for session status change (#134)
            crate::kanban::fire_event_hooks(
                &state.db,
                &state.engine,
                "on_session_status_change",
                "OnSessionStatusChange",
                json!({
                    "session_key": body.session_key,
                    "status": status,
                    "agent_id": agent_id,
                    "dispatch_id": dispatch_id,
                    "provider": provider,
                }),
            );

            // NOTE: The additional idle-specific re-fire of OnDispatchCompleted was removed.
            // Dispatch finalization already fires OnDispatchCompleted elsewhere in the
            // pipeline. Re-firing here caused duplicate hook execution and duplicate
            // review-decision dispatches.

            // #179: When session transitions to idle, trigger auto-queue to dispatch next entry.
            // This closes the chain gap where onCardTerminal hasn't fired yet (card still in review)
            // but the agent is already idle and could start the next queued item.
            if status == "idle" {
                if let Some(ref aid) = agent_id {
                    spawn_auto_queue_activate_for_agent(state.clone(), aid.clone());
                }
            }

            // Emit session event for real-time dashboard update (#156)
            // Read the full session row (joined with agent data) from sessions table
            // to ensure fresh status/session_info rather than stale agents table data.
            if let Ok(conn) = state.db.lock() {
                let session_event: Option<(i64, serde_json::Value, bool)> = conn.query_row(
                    "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, \
                     s.active_dispatch_id, s.model, s.tokens, s.cwd, s.last_heartbeat, \
                     s.session_info, a.department, a.sprite_number, a.avatar_emoji, \
                     COALESCE(a.xp, 0), s.thread_channel_id, \
                     d.name, d.name_ko, d.color \
                     FROM sessions s \
                     LEFT JOIN agents a ON s.agent_id = a.id \
                     LEFT JOIN departments d ON a.department = d.id \
                     WHERE s.session_key = ?1",
                    [&body.session_key],
                    |row| {
                        let sid: i64 = row.get(0)?;
                        let skey: Option<String> = row.get(1)?;
                        Ok((sid, json!({
                            "id": sid.to_string(),
                            "session_key": skey,
                            "name": skey,
                            "linked_agent_id": row.get::<_, Option<String>>(2)?,
                            "provider": row.get::<_, Option<String>>(3)?,
                            "status": row.get::<_, Option<String>>(4)?,
                            "active_dispatch_id": row.get::<_, Option<String>>(5)?,
                            "model": row.get::<_, Option<String>>(6)?,
                            "tokens": row.get::<_, i64>(7)?,
                            "cwd": row.get::<_, Option<String>>(8)?,
                            "last_seen_at": row.get::<_, Option<String>>(9)?,
                            "session_info": row.get::<_, Option<String>>(10)?,
                            "department_id": row.get::<_, Option<String>>(11)?,
                            "sprite_number": row.get::<_, Option<i64>>(12)?,
                            "avatar_emoji": row.get::<_, Option<String>>(13).ok().flatten().unwrap_or_else(|| "\u{1F916}".to_string()),
                            "stats_xp": row.get::<_, i64>(14).unwrap_or(0),
                            "thread_channel_id": row.get::<_, Option<String>>(15).ok().flatten(),
                            "department_name": row.get::<_, Option<String>>(16)?,
                            "department_name_ko": row.get::<_, Option<String>>(17)?,
                            "department_color": row.get::<_, Option<String>>(18)?,
                            "connected_at": null,
                        }), false))
                    },
                ).ok();

                if let Some((_sid, payload, _)) = session_event {
                    if is_new_session {
                        // New sessions must be emitted immediately — batching
                        // can suppress the insert if an update arrives within
                        // the same flush window (dashboard needs the "new" first).
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
            }

            // Also emit agent_status for agent-level dashboard (batched)
            if let Some(ref aid) = agent_id {
                if let Ok(conn) = state.db.lock() {
                    if let Ok(agent) = conn.query_row(
                        "SELECT a.id, a.name, a.name_ko, s.status, s.session_info, \
                         a.cli_provider, a.avatar_emoji, a.department, \
                         a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx \
                         FROM agents a LEFT JOIN sessions s ON s.agent_id = a.id \
                         AND s.session_key = ?2 \
                         WHERE a.id = ?1",
                        rusqlite::params![aid, body.session_key],
                        |row| {
                            Ok(json!({
                                "id": row.get::<_, String>(0)?,
                                "name": row.get::<_, String>(1)?,
                                "name_ko": row.get::<_, Option<String>>(2)?,
                                "status": row.get::<_, Option<String>>(3)?,
                                "session_info": row.get::<_, Option<String>>(4)?,
                                "cli_provider": row.get::<_, Option<String>>(5)?,
                                "avatar_emoji": row.get::<_, Option<String>>(6)?,
                                "department": row.get::<_, Option<String>>(7)?,
                                "discord_channel_id": row.get::<_, Option<String>>(8)?,
                                "discord_channel_alt": row.get::<_, Option<String>>(9)?,
                                "discord_channel_cc": row.get::<_, Option<String>>(10)?,
                                "discord_channel_cdx": row.get::<_, Option<String>>(11)?,
                                "discord_channel_id_codex": row.get::<_, Option<String>>(11)?,
                            }))
                        },
                    ) {
                        crate::server::ws::emit_batched_event(
                            &state.batch_buffer,
                            "agent_status",
                            aid,
                            agent,
                        );
                    }
                }
            }

            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/dispatched-sessions/cleanup — manual: delete disconnected sessions
pub async fn cleanup_sessions(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.execute("DELETE FROM sessions WHERE status = 'disconnected'", []) {
        Ok(n) => (StatusCode::OK, Json(json!({"ok": true, "deleted": n}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/dispatched-sessions/gc-threads — periodic: delete stale thread sessions
pub async fn gc_thread_sessions(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let deleted = gc_stale_thread_sessions_db(&conn);
    (
        StatusCode::OK,
        Json(json!({"ok": true, "gc_threads": deleted})),
    )
}

/// DELETE /api/hook/session — delete a session by session_key
pub async fn delete_session(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Read session id before delete for WS event
    let session_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM sessions WHERE session_key = ?1",
            [&params.session_key],
            |row| row.get(0),
        )
        .ok();

    match conn.execute(
        "DELETE FROM sessions WHERE session_key = ?1",
        [&params.session_key],
    ) {
        Ok(n) if n > 0 => {
            if let Some(sid) = session_id {
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "dispatched_session_disconnect",
                    json!({"id": sid.to_string()}),
                );
            }
            (StatusCode::OK, Json(json!({"ok": true, "deleted": n})))
        }
        Ok(n) => (StatusCode::OK, Json(json!({"ok": true, "deleted": n}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// GET /api/dispatched-sessions/claude-session-id?session_key=...
/// Returns the stored provider session_id for the given session_key.
pub async fn get_claude_session_id(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Fixed-channel rows can survive a dcserver crash with status=working even
    // when the underlying tmux/provider session is long dead. Clear those stale
    // rows before attempting to restore a provider session_id.
    let _ = disconnect_stale_fixed_session_by_key_db(&conn, &params.session_key);

    let provider = params.provider.as_deref().filter(|s| !s.is_empty());
    let result = if let Some(provider) = provider {
        conn.query_row(
            "SELECT claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = ?1 AND provider = ?2",
            rusqlite::params![&params.session_key, provider],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
    } else {
        conn.query_row(
            "SELECT claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = ?1",
            [&params.session_key],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
    };

    match result {
        Ok((claude_session_id, raw_provider_session_id)) => (
            StatusCode::OK,
            Json(json!({
                "claude_session_id": claude_session_id,
                "session_id": claude_session_id,
                "raw_provider_session_id": raw_provider_session_id,
            })),
        ),
        Err(rusqlite::Error::QueryReturnedNoRows) => (
            StatusCode::OK,
            Json(json!({
                "claude_session_id": null,
                "session_id": null,
                "raw_provider_session_id": null,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let changes = conn
        .execute(
            "UPDATE sessions
             SET claude_session_id = NULL,
                 raw_provider_session_id = NULL
             WHERE claude_session_id = ?1
                OR raw_provider_session_id = ?1",
            [sid],
        )
        .unwrap_or(0);
    (StatusCode::OK, Json(json!({"cleared": changes})))
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let changes = conn
        .execute(
            "UPDATE sessions
             SET claude_session_id = NULL,
                 raw_provider_session_id = NULL
             WHERE session_key = ?1",
            [key],
        )
        .unwrap_or(0);
    (StatusCode::OK, Json(json!({"cleared": changes})))
}

fn backfill_legacy_thread_channel_ids(conn: &rusqlite::Connection) -> usize {
    let legacy_rows: Vec<(String, Option<String>)> = {
        let mut stmt = match conn.prepare(
            "SELECT session_key, active_dispatch_id
             FROM sessions
             WHERE thread_channel_id IS NULL",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return 0,
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        }) {
            Ok(rows) => rows,
            Err(_) => return 0,
        };
        rows.filter_map(|row| row.ok()).collect()
    };

    let updates: Vec<(String, String)> = legacy_rows
        .into_iter()
        .filter_map(|(session_key, active_dispatch_id)| {
            parse_thread_channel_id_from_session_key(&session_key)
                .or_else(|| {
                    active_dispatch_id
                        .as_deref()
                        .and_then(|dispatch_id| load_dispatch_thread_id(conn, dispatch_id))
                })
                .map(|thread_channel_id| (session_key, thread_channel_id))
        })
        .collect();

    updates
        .into_iter()
        .map(|(session_key, thread_channel_id)| {
            conn.execute(
                "UPDATE sessions
                 SET thread_channel_id = ?1
                 WHERE session_key = ?2 AND thread_channel_id IS NULL",
                rusqlite::params![thread_channel_id, session_key],
            )
            .unwrap_or(0)
        })
        .sum()
}

fn collect_stale_fixed_session_dispatch_ids<P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
    log_context: &str,
) -> Vec<String> {
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] {log_context}: failed to prepare stale fixed-session dispatch query: {error}"
            );
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(params, |row| row.get::<_, Option<String>>(0)) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] {log_context}: failed to query stale fixed-session dispatch ids: {error}"
            );
            return Vec::new();
        }
    };

    rows.filter_map(|row| match row {
        Ok(Some(dispatch_id)) => Some(dispatch_id),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] {log_context}: failed to read stale fixed-session dispatch row: {error}"
            );
            None
        }
    })
    .collect()
}

fn mark_stale_fixed_session_dispatches_failed(
    conn: &rusqlite::Connection,
    dispatch_ids: &[String],
    transition_source: &str,
) {
    for dispatch_id in dispatch_ids {
        match crate::dispatch::set_dispatch_status_on_conn(
            conn,
            dispatch_id,
            "failed",
            None,
            transition_source,
            Some(&["pending", "dispatched"]),
            false,
        ) {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    "[dispatched-sessions] {transition_source}: failed to mark stale dispatch {} as failed: {}",
                    dispatch_id,
                    error
                );
            }
        }
    }
}

/// GC stale thread sessions from DB.
/// Legacy rows may only encode the Discord thread ID inside session_key, so
/// backfill thread_channel_id before applying thread-session GC.
///
/// Idle/disconnected thread sessions without an active dispatch are dropped
/// after 1 hour. Rows that still carry an active_dispatch_id are preserved
/// until the 3-hour safety TTL so warm-resume sessions cannot lose their DB
/// ownership before idle-kill has a chance to reap them.
pub fn gc_stale_thread_sessions_db(conn: &rusqlite::Connection) -> usize {
    let _ = backfill_legacy_thread_channel_ids(conn);
    conn.execute(
        "DELETE FROM sessions
         WHERE thread_channel_id IS NOT NULL
           AND status IN ('idle', 'disconnected')
           AND (
             (active_dispatch_id IS NULL
               AND COALESCE(last_heartbeat, created_at) < datetime('now', ?1))
             OR COALESCE(last_heartbeat, created_at) < datetime('now', ?2)
           )",
        rusqlite::params![
            STALE_THREAD_SESSION_MAX_AGE_SQL,
            STALE_THREAD_SESSION_ACTIVE_DISPATCH_MAX_AGE_SQL,
        ],
    )
    .unwrap_or(0)
}

/// Mark stale fixed-channel working sessions as disconnected so they cannot
/// keep restoring dead provider session IDs after restart.
pub fn gc_stale_fixed_working_sessions_db(conn: &rusqlite::Connection) -> usize {
    let stale_dispatches = collect_stale_fixed_session_dispatch_ids(
        conn,
        "SELECT active_dispatch_id
         FROM sessions
         WHERE thread_channel_id IS NULL
           AND status = 'working'
           AND active_dispatch_id IS NOT NULL
           AND COALESCE(last_heartbeat, created_at) < datetime('now', ?1)",
        [STALE_FIXED_WORKING_SESSION_MAX_AGE_SQL],
        "gc_stale_fixed_working_session",
    );
    mark_stale_fixed_session_dispatches_failed(
        conn,
        &stale_dispatches,
        "gc_stale_fixed_working_session",
    );

    conn.execute(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL,
             raw_provider_session_id = NULL
         WHERE thread_channel_id IS NULL
           AND status = 'working'
           AND COALESCE(last_heartbeat, created_at) < datetime('now', ?1)",
        [STALE_FIXED_WORKING_SESSION_MAX_AGE_SQL],
    )
    .unwrap_or(0)
}

fn disconnect_stale_fixed_session_by_key_db(
    conn: &rusqlite::Connection,
    session_key: &str,
) -> usize {
    let stale_dispatches = collect_stale_fixed_session_dispatch_ids(
        conn,
        "SELECT active_dispatch_id
         FROM sessions
         WHERE session_key = ?1
           AND thread_channel_id IS NULL
           AND status = 'working'
           AND active_dispatch_id IS NOT NULL
           AND COALESCE(last_heartbeat, created_at) < datetime('now', ?2)",
        rusqlite::params![session_key, STALE_FIXED_WORKING_SESSION_MAX_AGE_SQL],
        "disconnect_stale_fixed_session_by_key",
    );
    mark_stale_fixed_session_dispatches_failed(
        conn,
        &stale_dispatches,
        "disconnect_stale_fixed_session_by_key",
    );

    conn.execute(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL,
             raw_provider_session_id = NULL
         WHERE session_key = ?1
           AND thread_channel_id IS NULL
           AND status = 'working'
           AND COALESCE(last_heartbeat, created_at) < datetime('now', ?2)",
        rusqlite::params![session_key, STALE_FIXED_WORKING_SESSION_MAX_AGE_SQL],
    )
    .unwrap_or(0)
}

/// PATCH /api/dispatched-sessions/:id
pub async fn update_dispatched_session(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDispatchedSessionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(ref status) = body.status {
        sets.push(format!("status = ?{}", idx));
        values.push(Box::new(status.clone()));
        idx += 1;
    }
    if let Some(ref dispatch_id) = body.active_dispatch_id {
        sets.push(format!("active_dispatch_id = ?{}", idx));
        values.push(Box::new(dispatch_id.clone()));
        idx += 1;
    }
    if let Some(ref model) = body.model {
        sets.push(format!("model = ?{}", idx));
        values.push(Box::new(model.clone()));
        idx += 1;
    }
    if let Some(tokens) = body.tokens {
        sets.push(format!("tokens = ?{}", idx));
        values.push(Box::new(tokens));
        idx += 1;
    }
    if let Some(ref cwd) = body.cwd {
        sets.push(format!("cwd = ?{}", idx));
        values.push(Box::new(cwd.clone()));
        idx += 1;
    }
    if let Some(ref session_info) = body.session_info {
        sets.push(format!("session_info = ?{}", idx));
        values.push(Box::new(session_info.clone()));
        idx += 1;
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    let sql = format!(
        "UPDATE sessions SET {} WHERE id = ?{}",
        sets.join(", "),
        idx
    );
    values.push(Box::new(id));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "session not found"})),
        ),
        Ok(_) => {
            // Read back session and emit update event (batched: 150ms dedup)
            if let Ok(session) = conn.query_row(
                "SELECT id, session_key, agent_id, status, provider, session_info, model, tokens, cwd, active_dispatch_id, last_heartbeat \
                 FROM sessions WHERE id = ?1",
                [id],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?.to_string(),
                        "session_key": row.get::<_, String>(1)?,
                        "agent_id": row.get::<_, Option<String>>(2)?,
                        "status": row.get::<_, Option<String>>(3)?,
                        "provider": row.get::<_, Option<String>>(4)?,
                        "session_info": row.get::<_, Option<String>>(5)?,
                        "model": row.get::<_, Option<String>>(6)?,
                        "tokens": row.get::<_, i64>(7)?,
                        "cwd": row.get::<_, Option<String>>(8)?,
                        "active_dispatch_id": row.get::<_, Option<String>>(9)?,
                        "last_heartbeat": row.get::<_, Option<String>>(10)?,
                    }))
                },
            ) {
                crate::server::ws::emit_batched_event(
                    &state.batch_buffer, "dispatched_session_update", &id.to_string(), session,
                );
            }
            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
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

    // Query session from DB
    let (active_dispatch_id, agent_id, runtime_channel_id, session_provider) = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        match conn.query_row(
            "SELECT active_dispatch_id, agent_id, thread_channel_id, provider FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        ) {
            Ok((active_dispatch_id, agent_id, thread_channel_id, session_provider)) => {
                let provider_name = provider_info
                    .as_ref()
                    .map(|(provider, _)| provider.as_str())
                    .or(session_provider.as_deref());
                let runtime_channel_id = normalize_thread_channel_id(thread_channel_id.as_deref())
                    .or_else(|| {
                        agent_id.as_deref().and_then(|agent_id| {
                            resolve_agent_channel_for_provider_on_conn(
                                &conn,
                                agent_id,
                                provider_name,
                            )
                            .ok()
                            .flatten()
                        })
                    });
                (
                    active_dispatch_id,
                    agent_id,
                    runtime_channel_id,
                    session_provider,
                )
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "session not found"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }
    };

    let lifecycle = stop_turn_preserving_queue(
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
    )
    .await;

    // 1. Kill tmux session (or confirm the runtime path already stopped it).
    let tmux_killed = lifecycle.tmux_killed;

    // 2. Clear inflight state by scanning provider directory for matching tmux_session_name.
    let inflight_cleared = lifecycle.inflight_cleared;

    // 3. Update session → disconnected, clear active fields
    // 4. Mark dispatch → failed
    // 5. Optionally create retry dispatch via central path (#108)
    let mut retry_dispatch_id: Option<String> = None;
    let mut retry_meta: Option<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    )> = None;
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        conn.execute(
            "UPDATE sessions SET status = 'disconnected', active_dispatch_id = NULL, \
             claude_session_id = NULL, raw_provider_session_id = NULL WHERE session_key = ?1",
            [session_key],
        )
        .ok();

        if let Some(ref did) = active_dispatch_id {
            let current_status: Option<String> = conn
                .query_row(
                    "SELECT status FROM task_dispatches WHERE id = ?1",
                    [did],
                    |row| row.get(0),
                )
                .ok();
            if current_status.as_deref() != Some("completed") {
                crate::dispatch::set_dispatch_status_on_conn(
                    &conn,
                    did,
                    "failed",
                    None,
                    "force_kill_session",
                    None,
                    false,
                )
                .ok();
            }

            // Prepare retry metadata from the failed dispatch (read while lock held)
            if retry {
                retry_meta = conn
                    .query_row(
                        "SELECT kanban_card_id, to_agent_id, dispatch_type, title, context, retry_count \
                         FROM task_dispatches WHERE id = ?1",
                        [did],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<String>>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, i64>(5)?,
                            ))
                        },
                    )
                    .ok();
            }
        }
    }

    // Create retry dispatch via central authoritative path (#108)
    if let Some((card_id, to_agent_id, dispatch_type, title, context, retry_count)) = retry_meta {
        let agent = to_agent_id.as_deref().unwrap_or("unknown");
        let dtype = dispatch_type.as_deref().unwrap_or("implementation");
        let dtitle = title.as_deref().unwrap_or("retry dispatch");
        let ctx: serde_json::Value = context
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));

        match crate::dispatch::create_dispatch(
            &state.db,
            &state.engine,
            &card_id,
            agent,
            dtype,
            dtitle,
            &ctx,
        ) {
            Ok(dispatch_row) => {
                // Stamp retry_count on the new dispatch
                let new_id = dispatch_row["id"].as_str().unwrap_or("").to_string();
                if let Ok(conn) = state.db.lock() {
                    conn.execute(
                        "UPDATE task_dispatches SET retry_count = ?1 WHERE id = ?2",
                        rusqlite::params![retry_count + 1, new_id],
                    )
                    .ok();
                }
                retry_dispatch_id = Some(new_id.clone());
            }
            Err(e) => {
                tracing::warn!(
                    "[force-kill] retry dispatch creation via central path failed for card {}: {e}",
                    card_id
                );
            }
        }
    }

    let queue_activation_requested = if retry_dispatch_id.is_none() {
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
        crate::services::termination_audit::record_termination_with_db(
            &state.db,
            session_key,
            active_dispatch_id.as_deref(),
            "force_kill_api",
            "force_kill",
            Some(reason),
            None,
            None,
            Some(false),
        );
    }

    // Notify bot message for force-kill visibility
    if tmux_killed && let Some(ref channel_id_str) = runtime_channel_id {
        // Build human-readable message: agent name + reason from tmux exit file
        let agent_label = agent_id.as_deref().unwrap_or("unknown");
        let exit_reason = crate::services::tmux_diagnostics::read_tmux_exit_reason(&tmux_name)
            .map(|r| {
                // Strip timestamp prefix "[2026-...] " if present
                let trimmed = if let Some(idx) = r.find("] ") {
                    &r[idx + 2..]
                } else {
                    &r
                };
                let s = trimmed.trim();
                if s.len() > 80 {
                    format!("{}…", &s[..80])
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| lifecycle.lifecycle_path.to_string());
        if let Ok(conn) = state.db.lock() {
            let _ = conn.execute(
                "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
                rusqlite::params![
                    format!("channel:{channel_id_str}"),
                    format!("🔴 세션 종료: {agent_label}\n사유: {exit_reason}"),
                ],
            );
        }
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
            "queue_activation_requested": queue_activation_requested,
        })),
    )
}

/// POST /api/sessions/{session_key}/force-kill
///
/// Atomically: kill tmux session + clear inflight file + set session disconnected
/// + mark active dispatch failed. Optionally creates a retry dispatch.
pub async fn force_kill_session(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
    Json(body): Json<ForceKillOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    let reason = body.reason.as_deref().unwrap_or("force-kill API invoked");
    force_kill_session_impl_with_reason(&state, &session_key, body.retry, reason).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::Value;
    use std::ffi::OsString;
    use std::process::Command;
    use std::sync::MutexGuard;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
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

    fn seed_card(conn: &rusqlite::Connection, card_id: &str, dispatch_id: &str, status: &str) {
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
             VALUES (?1, 'Force Kill Card', ?2, ?3, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status, dispatch_id],
        )
        .unwrap();
    }

    fn seed_dispatch(
        conn: &rusqlite::Connection,
        dispatch_id: &str,
        card_id: &str,
        agent_id: &str,
    ) {
        conn.execute(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, retry_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'implementation', 'pending', 'Recover me', '{}', 0, datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, card_id, agent_id],
        )
        .unwrap();
    }

    fn seed_agent(conn: &rusqlite::Connection, agent_id: &str) {
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES (?1, ?2, 'codex', ?3, datetime('now'), datetime('now'))",
            rusqlite::params![agent_id, format!("Agent {agent_id}"), "123456789012345678"],
        )
        .unwrap();
    }

    fn seed_session(
        conn: &rusqlite::Connection,
        session_key: &str,
        agent_id: &str,
        dispatch_id: &str,
    ) {
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES (?1, ?2, 'working', ?3, datetime('now'), datetime('now'))",
            rusqlite::params![session_key, agent_id, dispatch_id],
        )
        .unwrap();
    }

    fn seed_session_without_dispatch(
        conn: &rusqlite::Connection,
        session_key: &str,
        agent_id: &str,
    ) {
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, status, last_heartbeat, created_at)
             VALUES (?1, ?2, 'working', datetime('now'), datetime('now'))",
            rusqlite::params![session_key, agent_id],
        )
        .unwrap();
    }

    fn response_json(resp: Json<Value>) -> Value {
        resp.0
    }

    fn count_message_outbox_rows(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .unwrap()
    }

    fn count_termination_events(conn: &rusqlite::Connection, session_key: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM session_termination_events WHERE session_key = ?1",
            [session_key],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn force_kill_session_path_route_retries_active_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            seed_agent(&conn, "agent-force");
            seed_card(&conn, "card-force", "dispatch-force", "requested");
            seed_dispatch(&conn, "dispatch-force", "card-force", "agent-force");
            seed_session(
                &conn,
                "host:codex-agent-force",
                "agent-force",
                "dispatch-force",
            );
        }

        let (status, body) = force_kill_session(
            State(state),
            Path("host:codex-agent-force".to_string()),
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
        assert_eq!(body["lifecycle_path"], "direct-fallback");
        assert_eq!(body["queue_activation_requested"], false);

        let conn = db.lock().unwrap();
        let session_state: (String, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id FROM sessions WHERE session_key = ?1",
                ["host:codex-agent-force"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(session_state.0, "disconnected");
        assert!(session_state.1.is_none());

        let old_dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                ["dispatch-force"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_dispatch_status, "failed");

        let new_dispatch: (String, i64) = conn
            .query_row(
                "SELECT status, retry_count FROM task_dispatches WHERE id = ?1",
                [&retry_dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(new_dispatch.0, "pending");
        assert_eq!(new_dispatch.1, 1);
    }

    #[tokio::test]
    async fn force_kill_session_legacy_wrapper_uses_same_core_without_retry() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            seed_agent(&conn, "agent-force-legacy");
            seed_card(
                &conn,
                "card-force-legacy",
                "dispatch-force-legacy",
                "requested",
            );
            seed_dispatch(
                &conn,
                "dispatch-force-legacy",
                "card-force-legacy",
                "agent-force-legacy",
            );
            seed_session(
                &conn,
                "host:claude-agent-force-legacy",
                "agent-force-legacy",
                "dispatch-force-legacy",
            );
        }

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

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                ["dispatch-force-legacy"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "failed");
    }

    #[tokio::test]
    async fn force_kill_session_clears_matching_inflight_and_live_tmux() {
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

        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);
        {
            let conn = db.lock().unwrap();
            seed_agent(&conn, "agent-force-live");
            seed_session_without_dispatch(&conn, &session_key, "agent-force-live");
        }

        let (status, body) = force_kill_session(
            State(state),
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

        let conn = db.lock().unwrap();
        let session_status: String = conn
            .query_row(
                "SELECT status FROM sessions WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_status, "disconnected");
        assert_eq!(count_message_outbox_rows(&conn), 1);
        assert_eq!(count_termination_events(&conn, &session_key), 1);
    }

    #[tokio::test]
    async fn force_kill_session_skips_notify_and_audit_when_tmux_is_already_gone() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);
        let session_key = format!(
            "host:AgentDesk-codex-force-kill-dead-{}",
            std::process::id()
        );

        {
            let conn = db.lock().unwrap();
            seed_agent(&conn, "agent-force-dead");
            seed_session_without_dispatch(&conn, &session_key, "agent-force-dead");
        }

        let (status, body) = force_kill_session(
            State(state),
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

        let conn = db.lock().unwrap();
        let session_status: String = conn
            .query_row(
                "SELECT status FROM sessions WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_status, "disconnected");
        assert_eq!(count_message_outbox_rows(&conn), 0);
        assert_eq!(count_termination_events(&conn, &session_key), 0);
    }

    #[tokio::test]
    async fn idle_hook_does_not_auto_complete_implementation_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let card_id = "card-1";
        let dispatch_id = "dispatch-1";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
                 VALUES (?1, 'Test Card', 'requested', ?2, datetime('now'), datetime('now'))",
                rusqlite::params![card_id, dispatch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (?1, ?2, 'ch-td', 'implementation', 'pending', 'Test Card', '{}', datetime('now'), datetime('now'))",
                rusqlite::params![dispatch_id, card_id],
            )
            .unwrap();
        }

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-1".to_string(),
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

        let conn = db.lock().unwrap();
        // implementation dispatches must NOT be auto-completed on idle —
        // they require explicit completion from turn_bridge
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let active_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-1'",
                [],
                |row| row.get(0),
            )
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
    }

    #[tokio::test]
    async fn working_hook_records_single_transition_audit_for_requested_to_in_progress() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let card_id = "card-working-audit";
        let dispatch_id = "dispatch-working-audit";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
                 VALUES (?1, 'Audit Card', 'requested', ?2, datetime('now'), datetime('now'))",
                rusqlite::params![card_id, dispatch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (?1, ?2, 'project-agentdesk', 'implementation', 'pending', 'Audit Card', '{}', datetime('now'), datetime('now'))",
                rusqlite::params![dispatch_id, card_id],
            )
            .unwrap();
        }

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-working-audit".to_string(),
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
        assert_eq!(status, StatusCode::OK);

        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .unwrap();
        let audit_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_audit_logs
                 WHERE card_id = ?1 AND from_status = 'requested' AND to_status = 'in_progress' AND source = 'hook'",
                [card_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(card_status, "in_progress");
        assert_eq!(
            audit_count, 1,
            "session status hook should not replay the same requested -> in_progress transition"
        );
    }

    #[tokio::test]
    async fn idle_hook_does_not_auto_complete_rework_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let card_id = "card-rework";
        let dispatch_id = "dispatch-rework";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
                 VALUES (?1, 'Rework Card', 'rework', ?2, datetime('now'), datetime('now'))",
                rusqlite::params![card_id, dispatch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (?1, ?2, 'ch-td', 'rework', 'pending', 'Rework Card', '{}', datetime('now'), datetime('now'))",
                rusqlite::params![dispatch_id, card_id],
            )
            .unwrap();
        }

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-rework".to_string(),
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

        let conn = db.lock().unwrap();
        // rework dispatches must NOT be auto-completed on idle —
        // they require explicit completion from turn_bridge
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let active_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-rework'",
                [],
                |row| row.get(0),
            )
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
    }

    #[tokio::test]
    async fn idle_hook_does_not_auto_complete_pending_review_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let card_id = "card-review";
        let dispatch_id = "dispatch-review";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
                 VALUES (?1, 'Review Card', 'review', ?2, datetime('now'), datetime('now'))",
                rusqlite::params![card_id, dispatch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (?1, ?2, 'project-agentdesk', 'review', 'pending', '[Review R1] Review Card', '{}', datetime('now'), datetime('now'))",
                rusqlite::params![dispatch_id, card_id],
            )
            .unwrap();
        }

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-review".to_string(),
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

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let dispatch_result: Option<String> = conn
            .query_row(
                "SELECT result FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let active_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-review'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // review dispatches must stay pending until an explicit review-verdict arrives
        assert_eq!(dispatch_status, "pending");
        assert!(dispatch_result.is_none());
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle review sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );
    }

    #[tokio::test]
    async fn idle_hook_does_not_auto_complete_review_decision_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let card_id = "card-review-decision";
        let dispatch_id = "dispatch-review-decision";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, review_status, created_at, updated_at)
                 VALUES (?1, 'Review Decision Card', 'review', ?2, 'suggestion_pending', datetime('now'), datetime('now'))",
                rusqlite::params![card_id, dispatch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (?1, ?2, 'project-agentdesk', 'review-decision', 'pending', '[Review Decision] Review Decision Card', '{}', datetime('now'), datetime('now'))",
                rusqlite::params![dispatch_id, card_id],
            )
            .unwrap();
        }

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-review-decision".to_string(),
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
                session_key: "session-review-decision".to_string(),
                agent_id: None,
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(17),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                thread_channel_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(idle_status, StatusCode::OK);

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        let active_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-review-decision'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // review-decision dispatches must NOT be auto-completed on idle —
        // they require explicit agent action (accept/dispute/dismiss)
        assert_eq!(dispatch_status, "pending");
        assert_eq!(
            active_dispatch_id.as_deref(),
            Some(dispatch_id),
            "idle review-decision sessions must keep sticky active_dispatch_id for 180-minute TTL cleanup"
        );
    }

    #[tokio::test]
    async fn idle_hook_without_dispatch_id_preserves_existing_dispatch_binding() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-sticky', 'Sticky Card', 'in_progress', 'dispatch-sticky', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES ('dispatch-sticky', 'card-sticky', 'project-agentdesk', 'implementation', 'completed', 'Sticky', '{}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let (working_status, _) = hook_session(
            State(state.clone()),
            Json(HookSessionBody {
                session_key: "session-sticky".to_string(),
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

        let conn = db.lock().unwrap();
        let active_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-sticky'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_dispatch_id.as_deref(), Some("dispatch-sticky"));
    }

    #[tokio::test]
    async fn heartbeat_without_dispatch_id_does_not_resurrect_cleared_binding() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions
                 (session_key, provider, status, active_dispatch_id, last_heartbeat, created_at)
                 VALUES ('session-cleared', 'codex', 'working', 'dispatch-cleared', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE sessions SET active_dispatch_id = NULL WHERE session_key = 'session-cleared'",
                [],
            )
            .unwrap();
        }

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-cleared".to_string(),
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

        let conn = db.lock().unwrap();
        let active_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT active_dispatch_id FROM sessions WHERE session_key = 'session-cleared'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_dispatch_id, None);
    }

    #[tokio::test]
    async fn hook_session_turn_activity_refreshes_last_heartbeat_from_created_at() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions
                 (session_key, provider, status, created_at, last_heartbeat)
                 VALUES ('session-heartbeat', 'codex', 'idle', '2026-04-09 01:02:03', NULL)",
                [],
            )
            .unwrap();
        }

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-heartbeat".to_string(),
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

        let conn = db.lock().unwrap();
        let (created_at, last_heartbeat): (String, Option<String>) = conn
            .query_row(
                "SELECT created_at, last_heartbeat
                 FROM sessions
                 WHERE session_key = 'session-heartbeat'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(created_at, "2026-04-09 01:02:03");
        assert!(
            last_heartbeat
                .as_deref()
                .is_some_and(|value| value > created_at.as_str()),
            "turn activity must refresh last_heartbeat beyond the original created_at"
        );
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

    fn insert_gc_candidate_session(
        conn: &rusqlite::Connection,
        session_key: &str,
        status: &str,
        thread_channel_id: Option<&str>,
        active_dispatch_id: Option<&str>,
        heartbeat_age_sql: &str,
    ) {
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, thread_channel_id, active_dispatch_id, last_heartbeat, created_at)
             VALUES (?1, 'codex', ?2, ?3, ?4, datetime('now', ?5), datetime('now', ?5))",
            rusqlite::params![
                session_key,
                status,
                thread_channel_id,
                active_dispatch_id,
                heartbeat_age_sql
            ],
        )
        .unwrap();
    }

    #[test]
    fn gc_stale_thread_sessions_db_deletes_legacy_rows_without_touching_fixed_channels() {
        let db = test_db();
        let conn = db.lock().unwrap();
        let legacy_thread_session = "mac-mini:AgentDesk-codex-adk-cdx-t1490653467734446120";
        let fixed_channel_session = "mac-mini:AgentDesk-claude-adk-cc-token-test";
        let recent_thread_session = "mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796";

        insert_gc_candidate_session(&conn, legacy_thread_session, "idle", None, None, "-2 hours");
        insert_gc_candidate_session(
            &conn,
            fixed_channel_session,
            "disconnected",
            None,
            None,
            "-2 hours",
        );
        insert_gc_candidate_session(
            &conn,
            recent_thread_session,
            "idle",
            None,
            None,
            "-10 minutes",
        );

        let deleted = gc_stale_thread_sessions_db(&conn);
        assert_eq!(deleted, 1);

        let legacy_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_key = ?1",
                [legacy_thread_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_count, 0);

        let fixed_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_key = ?1",
                [fixed_channel_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fixed_count, 1);

        let fixed_thread_channel_id: Option<String> = conn
            .query_row(
                "SELECT thread_channel_id FROM sessions WHERE session_key = ?1",
                [fixed_channel_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fixed_thread_channel_id, None);

        let recent_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_key = ?1",
                [recent_thread_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recent_count, 1);

        let recent_thread_channel_id: Option<String> = conn
            .query_row(
                "SELECT thread_channel_id FROM sessions WHERE session_key = ?1",
                [recent_thread_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            recent_thread_channel_id.as_deref(),
            Some("1485400795435372796")
        );
    }

    #[test]
    fn gc_stale_thread_sessions_db_keeps_active_dispatch_rows_until_safety_ttl() {
        let db = test_db();
        let conn = db.lock().unwrap();
        let protected_session = "mac-mini:AgentDesk-codex-adk-cdx-t1495400795435372796";
        let expired_session = "mac-mini:AgentDesk-codex-adk-cdx-t1495400795435372797";

        insert_gc_candidate_session(
            &conn,
            protected_session,
            "idle",
            None,
            Some("dispatch-492-protected"),
            "-2 hours",
        );
        insert_gc_candidate_session(
            &conn,
            expired_session,
            "idle",
            None,
            Some("dispatch-492-expired"),
            "-4 hours",
        );

        let deleted = gc_stale_thread_sessions_db(&conn);
        assert_eq!(deleted, 1);

        let protected_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_key = ?1",
                [protected_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(protected_count, 1);

        let expired_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_key = ?1",
                [expired_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(expired_count, 0);
    }

    #[test]
    fn gc_stale_fixed_working_sessions_db_disconnects_session_and_fails_dispatch() {
        let db = test_db();
        let conn = db.lock().unwrap();
        seed_agent(&conn, "agent-fixed-gc");
        seed_card(&conn, "card-fixed-gc", "dispatch-fixed-gc", "requested");
        seed_dispatch(
            &conn,
            "dispatch-fixed-gc",
            "card-fixed-gc",
            "agent-fixed-gc",
        );
        insert_gc_candidate_session(
            &conn,
            "mac-mini:AgentDesk-codex-adk-cdx-fixed-gc",
            "working",
            None,
            Some("dispatch-fixed-gc"),
            "-7 hours",
        );

        let disconnected = gc_stale_fixed_working_sessions_db(&conn);
        assert_eq!(disconnected, 1);

        let session_state: (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, claude_session_id
                 FROM sessions
                 WHERE session_key = 'mac-mini:AgentDesk-codex-adk-cdx-fixed-gc'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(session_state.0, "disconnected");
        assert_eq!(session_state.1, None);
        assert_eq!(session_state.2, None);

        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-fixed-gc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "failed");
    }

    #[test]
    fn disconnect_stale_fixed_session_by_key_db_fails_target_dispatch() {
        let db = test_db();
        let conn = db.lock().unwrap();
        seed_agent(&conn, "agent-fixed-key");
        seed_card(&conn, "card-fixed-key", "dispatch-fixed-key", "requested");
        seed_dispatch(
            &conn,
            "dispatch-fixed-key",
            "card-fixed-key",
            "agent-fixed-key",
        );
        insert_gc_candidate_session(
            &conn,
            "mac-mini:AgentDesk-codex-adk-cdx-fixed-key",
            "working",
            None,
            Some("dispatch-fixed-key"),
            "-7 hours",
        );

        let disconnected = disconnect_stale_fixed_session_by_key_db(
            &conn,
            "mac-mini:AgentDesk-codex-adk-cdx-fixed-key",
        );
        assert_eq!(disconnected, 1);

        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-fixed-key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "failed");
    }

    #[test]
    fn backfill_legacy_thread_channel_ids_uses_active_dispatch_thread_id() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-backfill', 'Backfill Card', 'in_progress', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
             VALUES ('dispatch-backfill', 'card-backfill', 'project-agentdesk', 'implementation', 'dispatched', 'Backfill dispatch', '1486333430516945008', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES ('mac-mini:AgentDesk-codex-adk-cdx', 'codex', 'working', 'dispatch-backfill', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        let updated = backfill_legacy_thread_channel_ids(&conn);
        assert_eq!(updated, 1);

        let thread_channel_id: Option<String> = conn
            .query_row(
                "SELECT thread_channel_id FROM sessions WHERE session_key = 'mac-mini:AgentDesk-codex-adk-cdx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_channel_id.as_deref(), Some("1486333430516945008"));
    }

    #[tokio::test]
    async fn gc_thread_sessions_handler_reports_deleted_legacy_thread_rows() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            insert_gc_candidate_session(
                &conn,
                "mac-mini:AgentDesk-codex-adk-cdx-t1490653467734446120",
                "idle",
                None,
                None,
                "-2 hours",
            );
        }

        let (status, body) = gc_thread_sessions(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_json(body)["gc_threads"], 1);

        let conn = db.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn thread_session_resolves_agent_from_parent_channel() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
                 VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
                [],
            )
            .unwrap();
        }

        // Post session with thread channel name
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796".to_string(),
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

        let conn = db.lock().unwrap();
        let (agent_id, thread_channel_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = ?1",
                ["mac-mini:AgentDesk-claude-adk-cc-t1485400795435372796"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485400795435372796"));
    }

    #[tokio::test]
    async fn thread_session_resolves_alt_channel_agent_from_session_key_fallback() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
                 VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
                [],
            )
            .unwrap();
        }

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
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

        let conn = db.lock().unwrap();
        let (agent_id, thread_channel_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485506232256168011"));
    }

    #[tokio::test]
    async fn direct_session_resolves_agent_from_dispatch_when_tmux_channel_is_truncated() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(long_channel);
        let session_key = format!("mac-mini:{tmux_name}");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_alt)
                 VALUES ('project-skillmanager', 'SkillManager', ?1)",
                [long_channel],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
                 VALUES ('card-dispatch-fallback', 'Dispatch Fallback', 'in_progress', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-dispatch-fallback', 'card-dispatch-fallback', 'project-skillmanager', 'implementation', 'dispatched', 'Dispatch fallback', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
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

        let conn = db.lock().unwrap();
        let agent_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-skillmanager"));
    }

    #[tokio::test]
    async fn direct_session_ignores_missing_agent_from_dispatch_fallback() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(long_channel);
        let session_key = format!("mac-mini:{tmux_name}");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
                 VALUES ('card-missing-dispatch-agent', 'Missing Dispatch Agent', 'in_progress', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-missing-dispatch-agent', 'card-missing-dispatch-agent', 'project-missing-agent', 'implementation', 'dispatched', 'Dispatch fallback', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let (status, body) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
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

        let conn = db.lock().unwrap();
        let agent_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_id, None);
    }

    #[tokio::test]
    async fn direct_session_ignores_explicit_agent_id_without_other_resolution_context() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name("project-skillmanager-extremely-verbose-channel-cdx");
        let session_key = format!("codex/hash123/mac-mini:{tmux_name}");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_alt)
                 VALUES ('project-spoofed', 'Spoofed Agent', 'spoofed-channel')",
                [],
            )
            .unwrap();
        }

        let (status, body) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
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

        let conn = db.lock().unwrap();
        let agent_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_id, None);
    }

    #[tokio::test]
    async fn thread_session_resolves_agent_from_thread_id_when_parent_channel_is_truncated() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);
        let thread_id = "1487044675541012490";
        let long_parent_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name(&format!("{long_parent_channel}-t{thread_id}"));
        let session_key = format!("mac-mini:{tmux_name}");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_alt)
                 VALUES ('project-skillmanager', 'SkillManager', ?1)",
                [long_parent_channel],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
                 VALUES ('card-thread-fallback', 'Thread Fallback', 'in_progress', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at)
                 VALUES ('dispatch-thread-fallback', 'card-thread-fallback', 'project-skillmanager', 'implementation', 'dispatched', 'Thread fallback', ?1, datetime('now'), datetime('now'))",
                [thread_id],
            )
            .unwrap();
        }

        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.clone(),
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

        let conn = db.lock().unwrap();
        let (agent_id, stored_thread_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-skillmanager"));
        assert_eq!(stored_thread_id.as_deref(), Some(thread_id));
    }

    #[tokio::test]
    async fn thread_session_accepts_explicit_thread_channel_id_without_thread_name() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
                 VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
                [],
            )
            .unwrap();
        }

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
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

        let conn = db.lock().unwrap();
        let (agent_id, thread_channel_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id.as_deref(), Some("1485506232256168011"));
    }

    #[tokio::test]
    async fn direct_channel_session_keeps_agent_mapping_without_thread_id() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
                 VALUES ('project-agentdesk', 'AgentDesk', 'adk-cc', 'adk-cdx')",
                [],
            )
            .unwrap();
        }

        let session_key = "mac-mini:AgentDesk-codex-adk-cdx";
        let (status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: session_key.to_string(),
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

        let conn = db.lock().unwrap();
        let (agent_id, thread_channel_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT agent_id, thread_channel_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("project-agentdesk"));
        assert_eq!(thread_channel_id, None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn stale_local_tmux_session_is_filtered_from_active_dispatch_list() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        let hostname = crate::services::platform::hostname_short();
        let session_key = format!("{hostname}:AgentDesk-stale-test-{}", std::process::id());

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, name_ko, provider, avatar_emoji, status, created_at)
                 VALUES ('ch-ad', 'AD', 'AD', 'claude', '🤖', 'idle', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, active_dispatch_id, last_heartbeat)
                 VALUES (?1, 'ch-ad', 'claude', 'working', 'stale session', 'dispatch-stale', datetime('now'))",
                rusqlite::params![session_key],
            )
            .unwrap();
        }

        let (status, Json(body)) = list_dispatched_sessions(
            State(state),
            Query(ListDispatchedSessionsQuery {
                include_merged: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"].as_array().unwrap().len(), 0);
    }
}
