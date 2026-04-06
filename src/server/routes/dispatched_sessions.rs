use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

use super::AppState;
use super::session_activity::SessionActivityResolver;
use crate::db::agents::AgentChannelBindings;
use crate::db::session_transcripts::SessionTranscriptSearchHit;
use crate::services::provider::parse_provider_and_channel_from_tmux_name;
use crate::utils::format::safe_prefix;

const STALE_FIXED_WORKING_SESSION_MAX_AGE_SQL: &str = "-6 hours";
const SEARCH_SUMMARY_MODEL: &str = "haiku";

/// Extract parent channel name from a thread channel name.
/// Thread names follow the convention `{parent}-t{thread_id}` where thread_id
/// is a numeric Discord channel ID (15+ digits).
/// Returns `(parent_channel_name, thread_id)` if the name matches.
fn parse_thread_channel_name(channel_name: &str) -> Option<(&str, &str)> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
        Some((&channel_name[..pos], suffix))
    } else {
        None
    }
}

fn parse_channel_name_from_session_key(session_key: &str) -> Option<String> {
    let (_, tmux_name) = session_key.split_once(':')?;
    let (_, channel_name) = parse_provider_and_channel_from_tmux_name(tmux_name)?;
    Some(channel_name)
}

fn resolve_agent_id_from_channel_name(
    conn: &rusqlite::Connection,
    channel_name: &str,
) -> Option<String> {
    if channel_name.is_empty() {
        return None;
    }

    conn.query_row(
        "SELECT id FROM agents
         WHERE discord_channel_id = ?1 OR discord_channel_alt = ?1
            OR discord_channel_cc = ?1 OR discord_channel_cdx = ?1",
        [channel_name],
        |row| row.get(0),
    )
    .ok()
    .or_else(|| {
        let mut stmt = conn
            .prepare(
                "SELECT id, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 FROM agents",
            )
            .ok()?;
        let mut rows = stmt.query([]).ok()?;
        while let Ok(Some(row)) = rows.next() {
            let id: String = row.get(0).ok()?;
            let bindings = AgentChannelBindings {
                provider: row.get(1).ok()?,
                discord_channel_id: row.get(2).ok()?,
                discord_channel_alt: row.get(3).ok()?,
                discord_channel_cc: row.get(4).ok()?,
                discord_channel_cdx: row.get(5).ok()?,
            };
            if bindings
                .all_channels()
                .iter()
                .any(|channel| channel_name.contains(channel))
            {
                return Some(id);
            }
        }
        None
    })
}

fn spawn_auto_queue_activate_for_agent(agent_id: String) {
    let port = crate::config::load_graceful().server.port;
    tokio::spawn(async move {
        // Let the session/dispatch cleanup commit before queue activation probes.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let url = crate::config::local_api_url(port, "/api/auto-queue/activate");
        let _ = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({
                "agent_id": agent_id,
                "active_only": true,
            }))
            .send()
            .await;
    });
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
pub struct HookSessionBody {
    pub session_key: String,
    pub status: Option<String>,
    pub provider: Option<String>,
    pub session_info: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u64>,
    pub cwd: Option<String>,
    pub dispatch_id: Option<String>,
    pub claude_session_id: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteSessionQuery {
    pub session_key: String,
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchSessionsQuery {
    pub q: String,
    pub limit: Option<usize>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
struct SearchSummary {
    model: &'static str,
    text: String,
}

fn summary_requested(raw: Option<&str>) -> bool {
    !matches!(
        raw.map(|value| value.trim().to_ascii_lowercase()),
        Some(value) if value == "0" || value == "false" || value == "no"
    )
}

fn build_search_summary_prompt(query: &str, hits: &[SessionTranscriptSearchHit]) -> String {
    let mut sections = Vec::new();
    for (idx, hit) in hits.iter().take(8).enumerate() {
        let user = safe_prefix(hit.user_message.trim(), 700);
        let assistant = safe_prefix(hit.assistant_message.trim(), 900);
        let snippet = safe_prefix(hit.snippet.trim(), 220);
        sections.push(format!(
            "[{index}] session_key={session_key}\nprovider={provider}\nagent_id={agent_id}\ncreated_at={created_at}\nsnippet={snippet}\n\nUser:\n{user}\n\nAssistant:\n{assistant}",
            index = idx + 1,
            session_key = hit.session_key.as_deref().unwrap_or("-"),
            provider = hit.provider.as_deref().unwrap_or("-"),
            agent_id = hit.agent_id.as_deref().unwrap_or("-"),
            created_at = hit.created_at,
            snippet = if snippet.is_empty() { "-" } else { snippet },
        ));
    }

    format!(
        "당신은 AgentDesk의 과거 세션 검색 결과를 요약하는 분석기입니다.\n\
         검색어: {query}\n\n\
         규칙:\n\
         - 검색 결과에 실제로 나온 정보만 사용합니다.\n\
         - 추측하지 않습니다.\n\
         - 한국어로 3개 이하 bullet로 답합니다.\n\
         - 반복 설명 대신 공통 주제, 관련 이슈/기능, 눈에 띄는 결론만 압축합니다.\n\n\
         검색 결과:\n{results}",
        results = sections.join("\n\n---\n\n")
    )
}

async fn summarize_search_hits(
    query: &str,
    hits: &[SessionTranscriptSearchHit],
) -> Result<Option<SearchSummary>, String> {
    if hits.is_empty() {
        return Ok(None);
    }

    let prompt = build_search_summary_prompt(query, hits);
    let task = tokio::task::spawn_blocking(move || {
        crate::services::claude::execute_command_simple_with_model(
            &prompt,
            Some(SEARCH_SUMMARY_MODEL),
        )
    });

    let text = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .map_err(|_| "summary generation timed out".to_string())?
        .map_err(|e| format!("summary task join failed: {e}"))?
        .map_err(|e| format!("summary generation failed: {e}"))?;

    let text = text.trim().to_string();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(SearchSummary {
            model: SEARCH_SUMMARY_MODEL,
            text,
        }))
    }
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/sessions/search?q=keyword
pub async fn search_session_transcripts(
    State(state): State<AppState>,
    Query(params): Query<SearchSessionsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let raw_query = params.q.trim();
    if raw_query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "q is required"})),
        );
    }

    let conn = match state.db.read_conn() {
        Ok(conn) => conn,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db read_conn failed: {e}")})),
            );
        }
    };

    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    let (match_query, hits) =
        match crate::db::session_transcripts::search_transcripts(&conn, raw_query, limit) {
            Ok(result) => result,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("search failed: {e}")})),
                );
            }
        };
    drop(conn);

    let want_summary = summary_requested(params.summary.as_deref());
    let (summary, summary_error) = if want_summary && !hits.is_empty() {
        match summarize_search_hits(raw_query, &hits).await {
            Ok(summary) => (summary, None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };

    (
        StatusCode::OK,
        Json(json!({
            "query": raw_query,
            "match_query": match_query,
            "count": hits.len(),
            "summary_requested": want_summary,
            "summary": summary.as_ref().map(|summary| json!({
                "model": summary.model,
                "text": summary.text,
            })),
            "summary_error": summary_error,
            "results": hits,
        })),
    )
}

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
    let session_key_channel_name = parse_channel_name_from_session_key(&body.session_key);
    let thread_channel_id = body
        .name
        .as_deref()
        .and_then(parse_thread_channel_name)
        .map(|(_, tid)| tid.to_string())
        .or_else(|| {
            session_key_channel_name
                .as_deref()
                .and_then(parse_thread_channel_name)
                .map(|(_, tid)| tid.to_string())
        });

    let agent_id = [body.name.as_deref(), session_key_channel_name.as_deref()]
        .into_iter()
        .flatten()
        .map(|name| {
            parse_thread_channel_name(name)
                .map(|(parent, _)| parent)
                .unwrap_or(name)
        })
        .find_map(|channel_name| resolve_agent_id_from_channel_name(&conn, channel_name));

    let status = body.status.as_deref().unwrap_or("working");
    let provider = body.provider.as_deref().unwrap_or("claude");
    let tokens = body.tokens.unwrap_or(0) as i64;
    // #107: Normalize empty claude_session_id to None (SQL NULL) so stale empty
    // strings are never persisted — prevents invalid --resume attempts after restart.
    let claude_session_id = body.claude_session_id.as_deref().filter(|s| !s.is_empty());
    let idle_auto_complete_dispatch = if status == "idle" {
        body.dispatch_id.as_ref().and_then(|did| {
            conn.query_row(
                "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?1",
                [did],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok()
            .and_then(|(dtype, dstatus)| {
                // Only review dispatches are auto-completed on idle.
                // implementation/rework require explicit completion via
                // PATCH /api/dispatches/:id (turn_bridge calls this at turn end).
                (dtype == "review" && dstatus == "pending").then_some(did.clone())
            })
        })
    } else {
        None
    };

    // Check if session exists before upsert to determine new vs update for WS event
    let is_new_session: bool = conn
        .query_row(
            "SELECT COUNT(*) = 0 FROM sessions WHERE session_key = ?1",
            [&body.session_key],
            |row| row.get(0),
        )
        .unwrap_or(true);

    let result = conn.execute(
        "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, model, tokens, cwd, active_dispatch_id, thread_channel_id, claude_session_id, last_heartbeat)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))
         ON CONFLICT(session_key) DO UPDATE SET
           status = excluded.status,
           provider = excluded.provider,
           session_info = COALESCE(excluded.session_info, sessions.session_info),
           model = COALESCE(excluded.model, sessions.model),
           tokens = excluded.tokens,
           cwd = COALESCE(excluded.cwd, sessions.cwd),
           active_dispatch_id = CASE
             WHEN excluded.status IN ('idle', 'disconnected') THEN NULL
             WHEN excluded.active_dispatch_id IS NOT NULL THEN excluded.active_dispatch_id
             ELSE sessions.active_dispatch_id
           END,
           agent_id = COALESCE(excluded.agent_id, sessions.agent_id),
           thread_channel_id = COALESCE(excluded.thread_channel_id, sessions.thread_channel_id),
           claude_session_id = COALESCE(excluded.claude_session_id, sessions.claude_session_id),
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
            body.dispatch_id,
            thread_channel_id,
            claude_session_id,
        ],
    );

    match result {
        Ok(_) => {
            let dispatch_id = body.dispatch_id.clone();
            drop(conn);

            if let Some(ref did) = idle_auto_complete_dispatch {
                if let Err(e) = crate::dispatch::finalize_dispatch(
                    &state.db,
                    &state.engine,
                    did,
                    "session_idle",
                    Some(&json!({ "auto_completed": true })),
                ) {
                    tracing::warn!(
                        "[session] Failed to auto-complete dispatch {} on idle: {}",
                        did,
                        e
                    );
                } else {
                    tracing::info!(
                        "[session] Auto-completed dispatch {} on idle session update",
                        did
                    );
                    // Send any follow-up dispatch (e.g. review dispatch) that was
                    // created by hooks during complete_dispatch to Discord.
                    super::dispatches::queue_dispatch_followup(&state.db, did);
                }
            }

            // Capture card status BEFORE hook fires.
            // If idle auto-completion created a new review dispatch, `latest_dispatch_id`
            // has already moved forward and this intentionally becomes `None`.
            let pre_hook_card: Option<(String, String)> = dispatch_id.as_ref().and_then(|did| {
                let conn = state.db.lock().ok()?;
                conn.query_row(
                    "SELECT kc.id, kc.status FROM kanban_cards kc WHERE kc.latest_dispatch_id = ?1",
                    [did],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok()
            });

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

            // After the hook fires, policies may have changed card status via kanban.setStatus.
            // Fire transition hooks if status actually changed.
            if let Some((card_id, old_card_status)) = &pre_hook_card {
                let new_card_status: Option<String> = {
                    let conn = state.db.lock().ok();
                    conn.and_then(|c| {
                        c.query_row(
                            "SELECT status FROM kanban_cards WHERE id = ?1",
                            [card_id],
                            |row| row.get(0),
                        )
                        .ok()
                    })
                };
                if let Some(ref new_s) = new_card_status {
                    if new_s != old_card_status {
                        crate::kanban::fire_transition_hooks(
                            &state.db,
                            &state.engine,
                            card_id,
                            old_card_status,
                            new_s,
                        );
                        // Drain any transitions accumulated by hooks (e.g., OnReviewEnter → pending_decision)
                        loop {
                            let extra = state.engine.drain_pending_transitions();
                            if extra.is_empty() {
                                break;
                            }
                            for (cid, os, ns) in &extra {
                                crate::kanban::fire_transition_hooks(
                                    &state.db,
                                    &state.engine,
                                    cid,
                                    os,
                                    ns,
                                );
                            }
                        }
                    }
                }
            }

            // NOTE: The additional idle-specific re-fire of OnDispatchCompleted was removed.
            // complete_dispatch() already fires OnDispatchCompleted + handle_completed_dispatch_followups
            // is spawned from the auto-complete path above (line ~252). Re-firing here caused
            // double hook execution → duplicate review-decision dispatches.

            // #179: When session transitions to idle, trigger auto-queue to dispatch next entry.
            // This closes the chain gap where onCardTerminal hasn't fired yet (card still in review)
            // but the agent is already idle and could start the next queued item.
            if status == "idle" {
                if let Some(ref aid) = agent_id {
                    spawn_auto_queue_activate_for_agent(aid.clone());
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
            "SELECT claude_session_id FROM sessions WHERE session_key = ?1 AND provider = ?2",
            rusqlite::params![&params.session_key, provider],
            |row| row.get::<_, Option<String>>(0),
        )
    } else {
        conn.query_row(
            "SELECT claude_session_id FROM sessions WHERE session_key = ?1",
            [&params.session_key],
            |row| row.get::<_, Option<String>>(0),
        )
    };

    match result {
        Ok(claude_session_id) => (
            StatusCode::OK,
            Json(json!({"claude_session_id": claude_session_id, "session_id": claude_session_id})),
        ),
        Err(rusqlite::Error::QueryReturnedNoRows) => (
            StatusCode::OK,
            Json(json!({"claude_session_id": null, "session_id": null})),
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
            "UPDATE sessions SET claude_session_id = NULL WHERE claude_session_id = ?1",
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
            "UPDATE sessions SET claude_session_id = NULL WHERE session_key = ?1",
            [key],
        )
        .unwrap_or(0);
    (StatusCode::OK, Json(json!({"cleared": changes})))
}

/// GC stale thread sessions from DB: idle/disconnected + older than 1 hour.
/// Thread sessions are identified by having a non-NULL thread_channel_id.
pub fn gc_stale_thread_sessions_db(conn: &rusqlite::Connection) -> usize {
    conn.execute(
        "DELETE FROM sessions
         WHERE thread_channel_id IS NOT NULL
           AND status IN ('idle', 'disconnected')
           AND last_heartbeat < datetime('now', '-1 hour')",
        [],
    )
    .unwrap_or(0)
}

/// Mark stale fixed-channel working sessions as disconnected so they cannot
/// keep restoring dead provider session IDs after restart.
pub fn gc_stale_fixed_working_sessions_db(conn: &rusqlite::Connection) -> usize {
    conn.execute(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL
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
    conn.execute(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL
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
}

pub(crate) async fn force_kill_session_impl(
    state: &AppState,
    session_key: &str,
    retry: bool,
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
    let (active_dispatch_id, agent_id, _thread_channel_id) = {
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
            "SELECT active_dispatch_id, agent_id, thread_channel_id FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        ) {
            Ok(row) => row,
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

    // 1. Kill tmux session
    let tmux_killed = {
        let sess = tmux_name.clone();
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::record_tmux_exit_reason(
                &sess,
                "force-kill API invoked",
            );
            crate::services::platform::tmux::kill_session(&sess)
        })
        .await
        .unwrap_or(false)
    };

    // 2. Clear inflight state by scanning provider directory for matching tmux_session_name
    let inflight_cleared = if let Some((ref provider, _)) = provider_info {
        clear_inflight_by_tmux_name(provider, &tmux_name)
    } else {
        false
    };

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
             claude_session_id = NULL WHERE session_key = ?1",
            [session_key],
        )
        .ok();

        if let Some(ref did) = active_dispatch_id {
            conn.execute(
                "UPDATE task_dispatches SET status = 'failed', updated_at = datetime('now') \
                 WHERE id = ?1 AND status NOT IN ('completed')",
                [did],
            )
            .ok();

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
            spawn_auto_queue_activate_for_agent(aid.clone());
            true
        } else {
            false
        }
    } else {
        false
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    eprintln!(
        "  [{ts}] ⚡ force-kill: session={}, tmux_killed={}, inflight_cleared={}, dispatch_failed={:?}",
        session_key, tmux_killed, inflight_cleared, active_dispatch_id
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "tmux_killed": tmux_killed,
            "inflight_cleared": inflight_cleared,
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
    force_kill_session_impl(&state, &session_key, body.retry).await
}

/// POST /api/sessions/force-kill
///
/// Legacy body-based wrapper retained for compatibility with older policy scripts.
pub async fn force_kill_session_legacy(
    State(state): State<AppState>,
    Json(body): Json<ForceKillBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    force_kill_session_impl(&state, &body.session_key, body.retry).await
}

/// Scan inflight directory for the provider and delete the file matching the given tmux_session_name.
fn clear_inflight_by_tmux_name(
    provider: &crate::services::provider::ProviderKind,
    tmux_name: &str,
) -> bool {
    let inflight_root = match crate::config::runtime_root() {
        Some(root) => root.join("runtime").join("discord_inflight"),
        None => return false,
    };

    let provider_dir = inflight_root.join(provider.as_str());
    let Ok(entries) = std::fs::read_dir(&provider_dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(state) = serde_json::from_str::<serde_json::Value>(&data) {
                if state.get("tmux_session_name").and_then(|v| v.as_str()) == Some(tmux_name) {
                    let _ = std::fs::remove_file(&path);
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use serde_json::Value;
    use std::ffi::OsString;
    use std::process::Command;
    use std::sync::{Mutex, MutexGuard, OnceLock};

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
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
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

    #[tokio::test]
    async fn search_session_transcripts_returns_fts_hits_without_summary() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let mut conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name) VALUES ('agent-search', 'Agent Search')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, created_at)
                 VALUES ('host:search-1', 'agent-search', 'claude', 'idle', datetime('now'))",
                [],
            )
            .unwrap();
            crate::db::session_transcripts::persist_turn_on_conn(
                &mut conn,
                crate::db::session_transcripts::PersistSessionTranscript {
                    turn_id: "discord:search:1",
                    session_key: Some("host:search-1"),
                    channel_id: Some("1490559149790986270"),
                    agent_id: None,
                    provider: Some("claude"),
                    dispatch_id: Some("dispatch-search"),
                    user_message: "FTS5 세션검색 구현 상태 알려줘",
                    assistant_message: "LLM 요약과 session transcript FTS 검색 API를 추가했습니다.",
                },
            )
            .unwrap();
        }

        let (status, body) = search_session_transcripts(
            State(state),
            Query(SearchSessionsQuery {
                q: "FTS5 요약".to_string(),
                limit: Some(5),
                summary: Some("0".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        assert_eq!(body["count"], 1);
        assert_eq!(body["summary_requested"], false);
        assert!(body["summary"].is_null());
        assert_eq!(body["results"][0]["session_key"], "host:search-1");
    }

    #[tokio::test]
    async fn search_session_transcripts_rejects_empty_query() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db, engine);

        let (status, body) = search_session_transcripts(
            State(state),
            Query(SearchSessionsQuery {
                q: "   ".to_string(),
                limit: None,
                summary: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body = response_json(body);
        assert_eq!(body["error"], "q is required");
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
            Json(ForceKillOptions { retry: true }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = response_json(body);
        let retry_dispatch_id = body["retry_dispatch_id"].as_str().unwrap().to_string();
        assert!(!retry_dispatch_id.is_empty());
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
                "tmux_session_name": tmux_name,
                "channel_id": "123456789012345678"
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
            Json(ForceKillOptions { retry: false }),
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
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-1".to_string(),
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(42),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
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
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-rework".to_string(),
                status: Some("idle".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(10),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
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

        // Card stays in rework — must NOT advance to review (which would happen
        // if idle auto-completed the rework dispatch).
        assert_eq!(card_status, "rework", "card should not advance past rework");
        assert_eq!(
            dispatch_status, "pending",
            "rework dispatch should stay pending on idle"
        );
    }

    #[tokio::test]
    async fn idle_hook_auto_completes_pending_review_dispatch() {
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
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-review".to_string(),
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(11),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
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

        // review dispatches are auto-completed on idle (989043b)
        assert_eq!(dispatch_status, "completed");
        assert!(
            dispatch_result
                .unwrap_or_default()
                .contains("\"completion_source\":\"session_idle\"")
        );
        assert_eq!(active_dispatch_id, None);
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
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("working".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
                session_id: None,
            }),
        )
        .await;
        assert_eq!(working_status, StatusCode::OK);

        let (idle_status, _) = hook_session(
            State(state),
            Json(HookSessionBody {
                session_key: "session-review-decision".to_string(),
                status: Some("idle".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("idle".to_string()),
                name: None,
                model: None,
                tokens: Some(17),
                cwd: None,
                dispatch_id: Some(dispatch_id.to_string()),
                claude_session_id: None,
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

        // review-decision dispatches must NOT be auto-completed on idle —
        // they require explicit agent action (accept/dispute/dismiss)
        assert_eq!(dispatch_status, "pending");
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
                status: Some("working".to_string()),
                provider: Some("claude".to_string()),
                session_info: Some("thread work".to_string()),
                name: Some("adk-cc-t1485400795435372796".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
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
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("thread work".to_string()),
                name: None,
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: Some("dispatch-1".to_string()),
                claude_session_id: None,
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
                status: Some("working".to_string()),
                provider: Some("codex".to_string()),
                session_info: Some("direct channel work".to_string()),
                name: Some("adk-cdx".to_string()),
                model: None,
                tokens: None,
                cwd: None,
                dispatch_id: None,
                claude_session_id: None,
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
