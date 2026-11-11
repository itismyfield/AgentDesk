use crate::app_state::AppState;
use crate::db::dispatched_sessions as dispatched_sessions_db;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
    parse_thread_channel_name, resolve_agent_id_for_session_pg,
};
use crate::db::session_status::{
    is_live_status, is_user_wait_status, normalize_incoming_session_status,
};
use crate::domain::session_identity::tmux_name_from_session_key;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{ForceKillRow, TurnLifecycleTarget, force_kill_turn_for_row};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[path = "dispatched_sessions/canonical_identity.rs"]
mod canonical_identity;
pub(crate) mod hosted_cleanup;
mod output;
pub use output::tmux_output;

#[path = "dispatched_sessions/tmux_cleanup.rs"]
mod tmux_cleanup;
use tmux_cleanup::kill_tmux_session_impl;

mod force_kill;
#[cfg(unix)]
mod herdr_kill;
mod hook;
use force_kill::{Entry, force_kill_session_impl_with_reason_and_forwarding};
pub use hook::{HookSessionQuery, hook_session_with_query};

fn spawn_auto_queue_activate_for_agent(state: AppState, agent_id: String) {
    tokio::spawn(async move {
        // Let the session/dispatch cleanup commit before queue activation probes.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let _ = crate::services::auto_queue::route::activate(
            State(state),
            Json(crate::services::auto_queue::route::ActivateBody {
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
    /// Explicit semantic owner kind. Omitted by old binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_kind: Option<String>,
    /// Existing SHA-256-derived Discord bot namespace; never a raw token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_token_hash: Option<String>,
    /// Durable turn-start identity. Present only for an authoritative new turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_start_nonce: Option<String>,
    /// Whether the turn start was initiated by a dispatch. Paired with nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_origin: Option<bool>,
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
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
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
                Ok((StatusCode::OK, Json(json!({"sessions": sessions}))))
            }
            Err(error) => Err(AppError::internal(error).with_code(ErrorCode::Database)),
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// DELETE /api/dispatched-sessions/cleanup — manual: delete disconnected sessions
pub async fn cleanup_sessions(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match hosted_cleanup::cleanup_disconnected_sessions_pg(pool).await {
            Ok(result) => Ok((StatusCode::OK, Json(json!({"ok": true, "deleted": result})))),
            Err(error) => {
                Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database))
            }
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// DELETE /api/dispatched-sessions/gc-threads — periodic: delete stale thread sessions
pub async fn gc_thread_sessions(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        let deleted = dispatched_sessions_db::gc_stale_thread_sessions_pg(pool).await;
        return Ok((
            StatusCode::OK,
            Json(json!({"ok": true, "gc_threads": deleted.len()})),
        ));
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// DELETE /api/dispatched-sessions/webhook — delete a session by session_key
pub async fn delete_session(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match hosted_cleanup::delete_session_by_key_pg(pool, &params.session_key).await {
            Ok(result) => {
                if let Some(session_id) = result.session_id {
                    crate::eventbus::emit_event(
                        &state.broadcast_tx,
                        "dispatched_session_disconnect",
                        json!({"id": session_id.to_string()}),
                    );
                }
                Ok((
                    StatusCode::OK,
                    Json(json!({"ok": true, "deleted": result.deleted})),
                ))
            }
            Err(error) => {
                Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database))
            }
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// GET /api/dispatched-sessions/claude-session-id?session_key=...
/// Returns the stored provider session_id for the given session_key.
pub async fn get_claude_session_id(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
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
            Ok(Some(ids)) => {
                let selected_session_id =
                    selected_provider_resume_selector_for_provider_recording_observation(
                        pool, provider, &ids,
                    )
                    .await;
                Ok((
                    StatusCode::OK,
                    Json(json!({
                        "claude_session_id": ids.claude_session_id,
                        "session_id": selected_session_id,
                        "raw_provider_session_id": ids.raw_provider_session_id,
                    })),
                ))
            }
            Ok(None) => Ok((
                StatusCode::OK,
                Json(json!({
                    "claude_session_id": null,
                    "session_id": null,
                    "raw_provider_session_id": null,
                })),
            )),
            Err(error) => Err(AppError::internal(error).with_code(ErrorCode::Database)),
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// POST /api/dispatched-sessions/clear-stale-session-id
/// Clears the stale ID from every legacy row holding it outside the Herdr-configured channels.
pub async fn clear_stale_session_id(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(sid) = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("claude_session_id").and_then(|v| v.as_str()))
    else {
        return Err(AppError::bad_request("session_id required"));
    };
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::clear_stale_session_id_pg(pool, sid).await {
            Ok(result) => Ok((StatusCode::OK, Json(json!({"cleared": result})))),
            Err(error) => {
                Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database))
            }
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// POST /api/dispatched-sessions/clear-session-id
/// Clears claude_session_id for a specific legacy session_key outside the configured channels.
/// Used when /clear is called so the next turn doesn't resume a dead session.
pub async fn clear_session_id_by_key(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(key) = body.get("session_key").and_then(|v| v.as_str()) else {
        return Err(AppError::bad_request("session_key required"));
    };
    if let Some(pool) = state.pg_pool_ref() {
        return match dispatched_sessions_db::clear_session_id_by_key_pg(pool, key).await {
            Ok(result) => Ok((StatusCode::OK, Json(json!({"cleared": result})))),
            Err(error) => {
                Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database))
            }
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
}

/// PATCH /api/dispatched-sessions/:id
pub async fn update_dispatched_session(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDispatchedSessionBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        if body.status.is_none()
            && body.active_dispatch_id.is_none()
            && body.model.is_none()
            && body.tokens.is_none()
            && body.cwd.is_none()
            && body.session_info.is_none()
        {
            return Err(AppError::bad_request("no fields to update"));
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
            Ok(0) => Err(AppError::not_found("session not found")),
            Ok(_) => {
                match dispatched_sessions_db::load_session_update_payload_pg(pool, id).await {
                    Ok(Some(session)) => {
                        crate::eventbus::emit_batched_event(
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
                Ok((StatusCode::OK, Json(json!({"ok": true}))))
            }
            Err(error) => Err(AppError::internal(error).with_code(ErrorCode::Database)),
        };
    }

    Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database))
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

pub(crate) async fn force_kill_session_impl_with_reason(
    state: &AppState,
    session_key: &str,
    retry: bool,
    reason: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let headers = HeaderMap::new();
    let key = session_key;
    force_kill_session_impl_with_reason_and_forwarding(
        state,
        &headers,
        key,
        retry,
        reason,
        Entry::Direct,
    )
    .await
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
    let (key, retry) = (&session_key, body.retry);
    force_kill_session_impl_with_reason_and_forwarding(
        &state,
        &headers,
        key,
        retry,
        reason,
        Entry::Http,
    )
    .await
}

#[derive(Deserialize)]
pub struct KillTmuxOptions {
    /// Human-readable reason for the kill (e.g. "idle 15시간 초과").
    #[serde(default)]
    pub reason: Option<String>,
    /// Policy threshold that selected this idle-cleanup candidate. When present,
    /// the kill-time live-activity guard treats runtime output newer than the
    /// DB heartbeat as the true idle anchor and skips the kill only while that
    /// output is still younger than this threshold.
    #[serde(default)]
    pub minimum_idle_minutes: Option<u64>,
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
    kill_tmux_session_impl(
        &state,
        &headers,
        &session_key,
        reason,
        body.minimum_idle_minutes,
    )
    .await
}

/// #3053: most-recent runtime activity instant for a tmux session, as a
/// unix-epoch nanosecond count. Considers the relay output (`jsonl`) file mtime,
/// the `.generation` marker mtime, and (best-effort) the provider transcript
/// mtime. These files are touched by the live wrapper/relay even when the
/// session-key heartbeat path silently misses the idle-kill row, so they are a
/// reliable "is this tmux actually doing work?" signal at kill time. Returns 0
/// when nothing is observable.
///
/// #3169: also reused by the stall-watchdog (`recovery.rs`) as a liveness probe
/// before it force-cleans a `desynced` channel — a self-paced loop session that
/// is mid-write touches its jsonl even while the inflight row reads stale, so a
/// fresh probe here means the "desync" is a loop mid-write, not a hang.
pub(crate) fn latest_runtime_activity_unix_nanos(tmux_session_name: &str) -> i64 {
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
    // Codex TUI direct mode writes provider-native rollout JSONL under
    // ~/.codex/sessions/... and only stores that path in a small AgentDesk
    // marker. Use the marker and the rollout file as liveness anchors; the
    // AgentDesk relay jsonl may remain quiet while Codex keeps appending output.
    if let Some(marker_path) = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ) {
        latest = latest.max(mtime_nanos(&marker_path));
    }
    if let Some(marker) =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
    {
        latest = latest.max(marker.rollout_path.to_str().map(mtime_nanos).unwrap_or(0));
    }
    // Claude TUI direct mode relays from the provider-native transcript under
    // ~/.claude/projects. Treat the bound transcript mtime as runtime activity
    // so manual recovery does not keep a stale wrapper jsonl while Claude is
    // actively appending to the real transcript.
    if let Some(binding) =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
        && binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
    {
        latest = latest.max(mtime_nanos(&binding.output_path));
    }

    latest
}

/// Provider-output clock for the idle-kill guard: newest conversation record time plus an open-turn
/// flag. Spawn markers and touched mtimes (Claude rewrites idle transcripts hourly) do not count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProviderOutputEvidence {
    pub output_unix_nanos: i64,
    pub turn_open: bool,
}

pub(crate) fn provider_output_evidence(tmux_session_name: &str) -> ProviderOutputEvidence {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    let mut evidence = ProviderOutputEvidence::default();
    let Some((provider, _)) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)
    else {
        return evidence;
    };
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    if let Some(path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl")
    {
        sources.push(path.into());
    }
    if let Some(marker) =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
    {
        sources.push(marker.rollout_path);
    }
    if let Some(binding) =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
        && matches!(
            binding.runtime_kind,
            RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui
        )
    {
        sources.push(binding.output_path.into());
    }
    for path in sources {
        if let Some(nanos) =
            crate::services::tui_turn_state::latest_provider_record_unix_nanos(&provider, &path)
        {
            evidence.output_unix_nanos = evidence.output_unix_nanos.max(nanos);
        }
        evidence.turn_open |=
            crate::services::tui_turn_state::observe_provider_jsonl_turn_state(&provider, &path)
                .is_busy();
    }
    evidence
}

fn now_unix_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or(0)
}

pub(crate) fn selected_provider_resume_selector_for_provider<'a>(
    provider_name: Option<&str>,
    ids: &'a dispatched_sessions_db::ProviderSessionIds,
) -> Option<&'a str> {
    if provider_is_claude(provider_name) {
        selected_provider_resume_selector_with_claude_home(ids, None)
    } else {
        selected_provider_resume_selector(ids)
    }
}

pub(crate) async fn selected_provider_resume_selector_for_provider_recording_observation(
    pool: &sqlx::PgPool,
    provider_name: Option<&str>,
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> Option<String> {
    let selected =
        selected_provider_resume_selector_for_provider(provider_name, ids).map(str::to_string);
    record_raw_provider_transcript_len_watermark_if_observed(
        pool,
        &ids.resolved_session_key,
        provider_name,
        ids,
        None,
        RawProviderTranscriptObservationMode::AdvanceWatermark,
    )
    .await;
    selected
}

fn selected_provider_resume_selector(
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> Option<&str> {
    ids.claude_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| raw_provider_resume_selector(ids))
}

fn raw_provider_resume_selector(ids: &dispatched_sessions_db::ProviderSessionIds) -> Option<&str> {
    ids.raw_provider_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn selected_provider_resume_selector_with_claude_home<'a>(
    ids: &'a dispatched_sessions_db::ProviderSessionIds,
    claude_home: Option<&std::path::Path>,
) -> Option<&'a str> {
    let cached = ids
        .claude_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let raw = raw_provider_resume_selector(ids);
    let cwd = ids
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let cached_activity = cwd.zip(cached).and_then(|(cwd, selector)| {
        claude_selector_file_activity(cwd, selector, claude_home, None, false)
    });
    let raw_activity = cwd.zip(raw).and_then(|(cwd, selector)| {
        let evidence = raw_transcript_persisted_growth_evidence(ids, selector);
        claude_selector_file_activity(
            cwd,
            selector,
            claude_home,
            evidence.len_watermark,
            evidence.growth_proven,
        )
    });

    crate::services::session_selector_validity::choose_provider_session_selector(
        cached,
        raw,
        cached_activity,
        raw_activity,
        ids.cache_entry_age_secs,
        crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS,
    )
}

fn claude_selector_file_activity(
    cwd: &str,
    selector: &str,
    claude_home: Option<&std::path::Path>,
    persisted_len_watermark: Option<u64>,
    persisted_growth_proven: bool,
) -> Option<crate::services::session_selector_validity::SelectorFileActivity> {
    claude_selector_file_activity_sample(cwd, selector, claude_home).map(|activity| {
        crate::services::session_selector_validity::activity_with_observed_growth(
            selector,
            activity,
            persisted_len_watermark,
            persisted_growth_proven,
        )
    })
}

fn claude_selector_file_activity_sample(
    cwd: &str,
    selector: &str,
    claude_home: Option<&std::path::Path>,
) -> Option<crate::services::session_selector_validity::SelectorFileActivity> {
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        std::path::Path::new(cwd),
        selector,
        claude_home,
    )
    .ok()?;
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Some(
            crate::services::session_selector_validity::SelectorFileActivity {
                exists: false,
                len: 0,
                mtime_age_secs: None,
                observed_growth_since_previous_sample: false,
            },
        );
    };
    Some(
        crate::services::session_selector_validity::SelectorFileActivity {
            exists: true,
            len: metadata.len(),
            mtime_age_secs: file_mtime_age_secs(&metadata),
            observed_growth_since_previous_sample: false,
        },
    )
}

fn positive_raw_transcript_len_watermark(
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> Option<u64> {
    ids.raw_provider_transcript_len_watermark
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
}

struct RawTranscriptPersistedGrowthEvidence {
    len_watermark: Option<u64>,
    growth_proven: bool,
}

fn raw_transcript_persisted_growth_evidence(
    ids: &dispatched_sessions_db::ProviderSessionIds,
    raw_provider_session_id: &str,
) -> RawTranscriptPersistedGrowthEvidence {
    let raw_provider_session_id = raw_provider_session_id.trim();
    let watermark_matches_raw_id = !raw_provider_session_id.is_empty()
        && ids
            .raw_provider_transcript_watermark_session_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|watermark_session_id| watermark_session_id == raw_provider_session_id);
    if !watermark_matches_raw_id {
        return RawTranscriptPersistedGrowthEvidence {
            len_watermark: None,
            growth_proven: false,
        };
    }
    RawTranscriptPersistedGrowthEvidence {
        len_watermark: positive_raw_transcript_len_watermark(ids),
        growth_proven: ids.raw_provider_transcript_growth_proven,
    }
}

#[derive(Clone, Copy)]
enum RawProviderTranscriptObservationMode {
    AdvanceWatermark,
    GrowthFlagOnly,
}

async fn record_raw_provider_transcript_len_watermark_if_observed(
    pool: &sqlx::PgPool,
    session_key: &str,
    provider_name: Option<&str>,
    ids: &dispatched_sessions_db::ProviderSessionIds,
    claude_home: Option<&std::path::Path>,
    mode: RawProviderTranscriptObservationMode,
) {
    if !provider_is_claude(provider_name) {
        return;
    }
    let Some(cwd) = ids
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(raw) = raw_provider_resume_selector(ids) else {
        return;
    };
    let Some(activity) = claude_selector_file_activity_sample(cwd, raw, claude_home) else {
        return;
    };
    if !activity.exists || activity.len == 0 {
        return;
    }
    let update_result = match mode {
        RawProviderTranscriptObservationMode::AdvanceWatermark => {
            dispatched_sessions_db::update_raw_provider_transcript_len_watermark_pg(
                pool,
                session_key,
                provider_name,
                raw,
                activity.len,
            )
            .await
        }
        RawProviderTranscriptObservationMode::GrowthFlagOnly => {
            dispatched_sessions_db::mark_raw_provider_transcript_growth_if_observed_pg(
                pool,
                session_key,
                provider_name,
                raw,
                activity.len,
            )
            .await
        }
    };
    if let Err(error) = update_result {
        tracing::warn!(
            session_key,
            provider = provider_name.unwrap_or(""),
            raw_provider_session_id = raw,
            observed_len = activity.len,
            error,
            mode = match mode {
                RawProviderTranscriptObservationMode::AdvanceWatermark => "advance_watermark",
                RawProviderTranscriptObservationMode::GrowthFlagOnly => "growth_flag_only",
            },
            "failed to record raw provider transcript length observation"
        );
    }
}

fn file_mtime_age_secs(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    i64::try_from(age.as_secs()).ok()
}

fn provider_is_claude(provider_name: Option<&str>) -> bool {
    provider_name.is_some_and(|provider| provider.eq_ignore_ascii_case("claude"))
}

fn provider_resume_selector_is_effective(
    provider_name: Option<&str>,
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> bool {
    provider_resume_selector_is_effective_with_claude_home(provider_name, ids, None)
}

fn provider_resume_selector_is_effective_with_claude_home(
    provider_name: Option<&str>,
    ids: &dispatched_sessions_db::ProviderSessionIds,
    claude_home: Option<&std::path::Path>,
) -> bool {
    if !provider_is_claude(provider_name) {
        return selected_provider_resume_selector(ids).is_some();
    }

    let Some(selector) = selected_provider_resume_selector_with_claude_home(ids, claude_home)
    else {
        return false;
    };

    let Some(cwd) = ids
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };

    crate::services::claude_tui::transcript_tail::claude_transcript_path(
        std::path::Path::new(cwd),
        selector,
        claude_home,
    )
    .is_ok_and(|path| path.exists())
}

#[cfg(test)]
mod kill_tmux_resume_tests {
    use super::tmux_cleanup::{
        idle_kill_skip_decision, runtime_activity_age_minutes,
        should_skip_idle_cleanup_for_active_dispatch,
        should_skip_idle_kill_for_live_runtime_activity,
    };
    use super::{
        latest_runtime_activity_unix_nanos, provider_output_evidence,
        provider_resume_selector_is_effective_with_claude_home,
        selected_provider_resume_selector_with_claude_home,
    };
    use crate::db::dispatched_sessions::ProviderSessionIds;

    fn ids(
        claude_session_id: Option<&str>,
        raw_provider_session_id: Option<&str>,
        cwd: Option<&std::path::Path>,
        raw_provider_transcript_len_watermark: Option<i64>,
        raw_provider_transcript_growth_proven: bool,
    ) -> ProviderSessionIds {
        ProviderSessionIds {
            resolved_session_key: "test-session".to_string(),
            claude_session_id: claude_session_id.map(str::to_string),
            raw_provider_session_id: raw_provider_session_id.map(str::to_string),
            cwd: cwd.map(|path| path.display().to_string()),
            cache_entry_age_secs: Some(3_600),
            raw_provider_transcript_len_watermark,
            raw_provider_transcript_watermark_session_id: raw_provider_session_id
                .map(str::to_string),
            raw_provider_transcript_growth_proven,
        }
    }

    fn mtime_nanos(path: &std::path::Path) -> i64 {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
            .unwrap_or(0)
    }

    fn selector_observation_test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn latest_runtime_activity_uses_codex_tui_rollout_marker_target() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmux = format!("AgentDesk-codex-runtime-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().expect("tempdir");
        let rollout = dir.path().join("rollout.jsonl");
        std::fs::write(&rollout, "{\"type\":\"session_meta\"}\n").expect("write rollout");
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            &tmux,
            &rollout,
            Some("session-runtime-activity"),
        )
        .expect("write marker");
        let marker = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
            &tmux,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ));
        let old = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let new = filetime::FileTime::from_unix_time(1_700_000_600, 0);
        filetime::set_file_mtime(&marker, old).expect("set marker mtime");
        filetime::set_file_mtime(&rollout, new).expect("set rollout mtime");

        let latest = latest_runtime_activity_unix_nanos(&tmux);

        assert_eq!(latest, mtime_nanos(&rollout));
        assert!(latest > mtime_nanos(&marker));
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn latest_runtime_activity_uses_claude_tui_bound_transcript() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmux = format!("AgentDesk-claude-runtime-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("claude-transcript.jsonl");
        std::fs::write(&transcript, "{\"type\":\"assistant\"}\n").expect("write transcript");
        let transcript_time = filetime::FileTime::from_unix_time(1_700_001_000, 0);
        filetime::set_file_mtime(&transcript, transcript_time).expect("set transcript mtime");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );

        let latest = latest_runtime_activity_unix_nanos(&tmux);

        assert_eq!(latest, mtime_nanos(&transcript));
        assert!(crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux));
    }

    #[test]
    fn runtime_activity_age_minutes_uses_last_output_age() {
        let minute = 60_000_000_000i64;
        assert_eq!(
            runtime_activity_age_minutes(100 * minute, 465 * minute),
            Some(365)
        );
        assert_eq!(runtime_activity_age_minutes(0, 465 * minute), None);
        assert_eq!(
            runtime_activity_age_minutes(465 * minute, 465 * minute),
            Some(0)
        );
    }

    #[test]
    fn idle_kill_guard_uses_runtime_output_age_not_turn_age() {
        let minute = 60_000_000_000i64;
        let now = 1_000 * minute;
        let old_db_last_seen = 10 * minute;
        let recent_runtime_output = now - 5 * minute;
        let stale_runtime_output = now - 400 * minute;

        assert!(should_skip_idle_kill_for_live_runtime_activity(
            old_db_last_seen,
            recent_runtime_output,
            now,
            Some(360),
        ));
        assert!(!should_skip_idle_kill_for_live_runtime_activity(
            old_db_last_seen,
            stale_runtime_output,
            now,
            Some(360),
        ));
        assert!(!should_skip_idle_kill_for_live_runtime_activity(
            recent_runtime_output,
            recent_runtime_output,
            now,
            Some(360),
        ));
        assert!(!should_skip_idle_kill_for_live_runtime_activity(
            old_db_last_seen,
            0,
            now,
            Some(360),
        ));
    }

    fn rfc3339_at(unix_nanos: i64) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(unix_nanos)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn millis_floor(unix_nanos: i64) -> i64 {
        unix_nanos / 1_000_000 * 1_000_000
    }

    fn claude_record(kind: &str, body: &str, unix_nanos: i64) -> String {
        format!(
            "{{\"type\":\"{kind}\",{body},\"timestamp\":\"{}\"}}",
            rfc3339_at(unix_nanos)
        )
    }

    fn bind_claude_tui_transcript(
        tmux: &str,
        dir: &std::path::Path,
        records: &[String],
    ) -> std::path::PathBuf {
        let transcript = dir.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&transcript, records.join("\n") + "\n").expect("write transcript");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        transcript
    }

    const USER_HI: &str = "\"message\":{\"role\":\"user\",\"content\":\"hi\"}";
    const ASSISTANT_TEXT: &str =
        "\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}";
    const ASSISTANT_TOOL_USE: &str = "\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}]}";
    const TURN_DURATION: &str = "\"subtype\":\"turn_duration\",\"durationMs\":1";

    /// Claude's hourly transcript rewrite and a respawn `.generation` marker refresh mtimes
    /// without a new conversation record; the guard must not read that as activity.
    #[test]
    fn idle_kill_guard_ignores_marker_and_transcript_touch_without_provider_records() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let minute = 60_000_000_000i64;
        let now = super::now_unix_nanos();
        let last_output = now - 15 * 60 * minute;
        let last_seen = now - 14 * 60 * minute;
        let tmux = format!("AgentDesk-claude-runtime-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = bind_claude_tui_transcript(
            &tmux,
            dir.path(),
            &[
                claude_record("user", USER_HI, last_output - minute),
                claude_record("assistant", ASSISTANT_TEXT, last_output - minute / 2),
                claude_record("system", TURN_DURATION, last_output),
                "{\"type\":\"cost-state\",\"totalCostUSD\":1.0}".to_string(),
            ],
        );
        let touched = filetime::FileTime::from_unix_time(now / 1_000_000_000, 0);
        filetime::set_file_mtime(&transcript, touched).expect("touch transcript");
        let generation = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
            &tmux,
            "generation",
        ));
        std::fs::create_dir_all(generation.parent().expect("marker parent")).expect("marker dir");
        std::fs::write(&generation, "7").expect("write generation marker");

        let evidence = provider_output_evidence(&tmux);

        assert_eq!(evidence.output_unix_nanos, millis_floor(last_output));
        assert!(!evidence.turn_open);
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            None
        );
        // The mtime clock still reads the touch and the marker as activity.
        assert!(latest_runtime_activity_unix_nanos(&tmux) > last_seen);
        assert!(crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux));
    }

    /// A recent closed turn behind an old mtime is still protected, and an open turn
    /// (assistant `tool_use` without its result) is protected past the idle threshold.
    #[test]
    fn idle_kill_guard_protects_recent_provider_output_and_open_turn() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let minute = 60_000_000_000i64;
        let now = super::now_unix_nanos();
        let last_seen = now - 14 * 60 * minute;
        let stale_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let dir = tempfile::tempdir().expect("tempdir");

        let tmux_recent = format!("AgentDesk-claude-runtime-{}", uuid::Uuid::new_v4());
        let recent = bind_claude_tui_transcript(
            &tmux_recent,
            dir.path(),
            &[
                claude_record("user", USER_HI, now - 6 * minute),
                claude_record("assistant", ASSISTANT_TEXT, now - 5 * minute - minute / 2),
                claude_record("system", TURN_DURATION, now - 5 * minute),
            ],
        );
        filetime::set_file_mtime(&recent, stale_mtime).expect("stale mtime");
        let evidence = provider_output_evidence(&tmux_recent);
        assert_eq!(evidence.output_unix_nanos, millis_floor(now - 5 * minute));
        assert!(!evidence.turn_open);
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            Some("skip_live_output")
        );

        let tmux_open = format!("AgentDesk-claude-runtime-{}", uuid::Uuid::new_v4());
        let open = bind_claude_tui_transcript(
            &tmux_open,
            dir.path(),
            &[
                claude_record("user", USER_HI, now - 401 * minute),
                claude_record("assistant", ASSISTANT_TOOL_USE, now - 400 * minute),
            ],
        );
        filetime::set_file_mtime(&open, stale_mtime).expect("stale mtime");
        let evidence = provider_output_evidence(&tmux_open);
        assert_eq!(evidence.output_unix_nanos, millis_floor(now - 400 * minute));
        assert!(evidence.turn_open);
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            Some("skip_open_turn")
        );

        assert!(crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_recent));
        assert!(crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_open));
    }

    /// A legacy relay carries no record timestamps; a wrapper sentinel appended after an
    /// old result moves only the mtime, which is not output. A stamped result still is.
    #[test]
    fn idle_kill_guard_treats_unstamped_relay_with_meta_sentinel_as_no_output() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let minute = 60_000_000_000i64;
        let now = super::now_unix_nanos();
        let last_seen = now - 14 * 60 * minute;
        let tmux = format!("AgentDesk-claude-relay-{}", uuid::Uuid::new_v4());
        let relay = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
            &tmux, "jsonl",
        ));
        std::fs::create_dir_all(relay.parent().expect("relay parent")).expect("relay dir");
        std::fs::write(&relay, "{\"type\":\"result\",\"subtype\":\"success\"}\n")
            .expect("write relay");
        crate::services::tmux_common::emit_wrapper_sentinel(
            relay.to_str().expect("relay path"),
            crate::services::tmux_common::WrapperSentinel::ReadyForInput { provider: "claude" },
        );
        assert!(mtime_nanos(&relay) > last_seen);

        let evidence = provider_output_evidence(&tmux);

        assert_eq!(evidence.output_unix_nanos, 0);
        assert!(!evidence.turn_open);
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            None
        );

        std::fs::write(
            &relay,
            claude_record("result", "\"subtype\":\"success\"", now - 5 * minute) + "\n",
        )
        .expect("write stamped relay");
        let evidence = provider_output_evidence(&tmux);
        assert_eq!(evidence.output_unix_nanos, millis_floor(now - 5 * minute));
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            Some("skip_live_output")
        );
    }

    fn codex_event(payload: &str, unix_nanos: i64) -> String {
        format!(
            "{{\"timestamp\":\"{}\",\"type\":\"event_msg\",\"payload\":{payload}}}",
            rfc3339_at(unix_nanos)
        )
    }

    /// Codex settings/token records after a completed turn are housekeeping with fresh
    /// timestamps; the turn observer skips them and so must the output clock.
    #[test]
    fn idle_kill_guard_ignores_codex_housekeeping_records() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let minute = 60_000_000_000i64;
        let now = super::now_unix_nanos();
        let last_output = now - 15 * 60 * minute;
        let last_seen = now - 14 * 60 * minute;
        let tmux = format!("AgentDesk-codex-runtime-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().expect("tempdir");
        let rollout = dir.path().join("rollout.jsonl");
        let records = [
            format!(
                "{{\"timestamp\":\"{}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"s1\"}}}}",
                rfc3339_at(last_output - 60 * minute)
            ),
            codex_event(
                "{\"type\":\"task_started\",\"turn_id\":\"t1\"}",
                last_output - minute,
            ),
            codex_event(
                "{\"type\":\"task_complete\",\"turn_id\":\"t1\"}",
                last_output,
            ),
            codex_event("{\"type\":\"thread_settings_applied\"}", now - minute),
            codex_event("{\"type\":\"token_count\"}", now),
        ];
        std::fs::write(&rollout, records.join("\n") + "\n").expect("write rollout");
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            &tmux,
            &rollout,
            Some("session-housekeeping"),
        )
        .expect("write marker");

        let evidence = provider_output_evidence(&tmux);

        assert_eq!(evidence.output_unix_nanos, millis_floor(last_output));
        assert!(!evidence.turn_open);
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            None
        );

        let recent = codex_event(
            "{\"type\":\"task_complete\",\"turn_id\":\"t2\"}",
            now - 5 * minute,
        );
        std::fs::write(&rollout, records.join("\n") + "\n" + &recent + "\n").expect("append");
        let evidence = provider_output_evidence(&tmux);
        assert_eq!(evidence.output_unix_nanos, millis_floor(now - 5 * minute));
        assert_eq!(
            idle_kill_skip_decision(last_seen, evidence, now, Some(360)),
            Some("skip_live_output")
        );
    }

    #[test]
    fn idle_cleanup_reason_does_not_kill_active_dispatch_tmux() {
        assert!(should_skip_idle_cleanup_for_active_dispatch(
            Some("dispatch-123"),
            "idle 24시간 초과 — 자동 정리",
        ));
        assert!(!should_skip_idle_cleanup_for_active_dispatch(
            Some("dispatch-123"),
            "operator requested tmux reset",
        ));
        assert!(!should_skip_idle_cleanup_for_active_dispatch(
            None,
            "idle 24시간 초과 — 자동 정리",
        ));
    }

    #[test]
    fn claude_selector_switches_to_raw_only_after_observed_growth() {
        let _selector_guard = selector_observation_test_lock();
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let cached_session_id = uuid::Uuid::new_v4().to_string();
        let raw_session_id = uuid::Uuid::new_v4().to_string();
        let cached_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &cached_session_id,
            Some(claude_home.path()),
        )
        .expect("cached transcript path");
        let raw_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &raw_session_id,
            Some(claude_home.path()),
        )
        .expect("raw transcript path");
        std::fs::create_dir_all(cached_path.parent().expect("cached parent"))
            .expect("create cached parent");
        std::fs::create_dir_all(raw_path.parent().expect("raw parent")).expect("create raw parent");
        std::fs::write(&cached_path, b"cached\n").expect("write cached transcript");
        std::fs::write(&raw_path, b"raw\n").expect("write raw transcript");
        let now = std::time::SystemTime::now();
        filetime::set_file_mtime(
            &cached_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(700)),
        )
        .expect("set cached mtime");
        filetime::set_file_mtime(
            &raw_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(5)),
        )
        .expect("set raw mtime");
        let ids = ids(
            Some(&cached_session_id),
            Some(&raw_session_id),
            Some(cwd.path()),
            None,
            false,
        );

        assert_eq!(
            selected_provider_resume_selector_with_claude_home(&ids, Some(claude_home.path())),
            Some(cached_session_id.as_str()),
            "recent raw mtime alone is not growth evidence"
        );

        std::fs::write(&raw_path, b"raw\ngrown\n").expect("grow raw transcript");
        filetime::set_file_mtime(
            &raw_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(4)),
        )
        .expect("refresh raw mtime");

        assert_eq!(
            selected_provider_resume_selector_with_claude_home(&ids, Some(claude_home.path())),
            Some(raw_session_id.as_str()),
            "the second raw sample with a larger length is required before flipping"
        );
    }

    #[test]
    fn claude_selector_uses_raw_growth_from_persisted_watermark_after_restart() {
        let _selector_guard = selector_observation_test_lock();
        crate::services::session_selector_validity::clear_selector_observations_for_tests();
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let cached_session_id = uuid::Uuid::new_v4().to_string();
        let raw_session_id = uuid::Uuid::new_v4().to_string();
        let cached_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &cached_session_id,
            Some(claude_home.path()),
        )
        .expect("cached transcript path");
        let raw_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &raw_session_id,
            Some(claude_home.path()),
        )
        .expect("raw transcript path");
        std::fs::create_dir_all(cached_path.parent().expect("cached parent"))
            .expect("create cached parent");
        std::fs::create_dir_all(raw_path.parent().expect("raw parent")).expect("create raw parent");
        std::fs::write(&cached_path, b"cached\n").expect("write cached transcript");
        std::fs::write(&raw_path, b"raw\ngrown\n").expect("write raw transcript");
        let now = std::time::SystemTime::now();
        filetime::set_file_mtime(
            &cached_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(700)),
        )
        .expect("set cached mtime");
        filetime::set_file_mtime(
            &raw_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(5)),
        )
        .expect("set raw mtime");
        let ids = ids(
            Some(&cached_session_id),
            Some(&raw_session_id),
            Some(cwd.path()),
            Some(4),
            false,
        );

        assert_eq!(
            selected_provider_resume_selector_with_claude_home(&ids, Some(claude_home.path())),
            Some(raw_session_id.as_str()),
            "a persisted raw length watermark below the current length is durable growth evidence"
        );
    }

    #[test]
    fn claude_selector_keeps_cached_when_raw_len_equals_persisted_watermark() {
        let _selector_guard = selector_observation_test_lock();
        crate::services::session_selector_validity::clear_selector_observations_for_tests();
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let cached_session_id = uuid::Uuid::new_v4().to_string();
        let raw_session_id = uuid::Uuid::new_v4().to_string();
        let cached_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &cached_session_id,
            Some(claude_home.path()),
        )
        .expect("cached transcript path");
        let raw_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &raw_session_id,
            Some(claude_home.path()),
        )
        .expect("raw transcript path");
        std::fs::create_dir_all(cached_path.parent().expect("cached parent"))
            .expect("create cached parent");
        std::fs::create_dir_all(raw_path.parent().expect("raw parent")).expect("create raw parent");
        std::fs::write(&cached_path, b"cached\n").expect("write cached transcript");
        std::fs::write(&raw_path, b"raw\n").expect("write raw transcript");
        let raw_len = std::fs::metadata(&raw_path).expect("raw metadata").len() as i64;
        let now = std::time::SystemTime::now();
        filetime::set_file_mtime(
            &cached_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(700)),
        )
        .expect("set cached mtime");
        filetime::set_file_mtime(
            &raw_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(5)),
        )
        .expect("set raw mtime");
        let ids = ids(
            Some(&cached_session_id),
            Some(&raw_session_id),
            Some(cwd.path()),
            Some(raw_len),
            false,
        );

        assert_eq!(
            selected_provider_resume_selector_with_claude_home(&ids, Some(claude_home.path())),
            Some(cached_session_id.as_str()),
            "an equal persisted watermark means the raw transcript has not grown"
        );
    }

    #[test]
    fn claude_selector_ignores_raw_growth_evidence_for_different_raw_id() {
        let _selector_guard = selector_observation_test_lock();
        crate::services::session_selector_validity::clear_selector_observations_for_tests();
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let cached_session_id = uuid::Uuid::new_v4().to_string();
        let raw_session_id = uuid::Uuid::new_v4().to_string();
        let stale_watermark_session_id = uuid::Uuid::new_v4().to_string();
        let cached_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &cached_session_id,
            Some(claude_home.path()),
        )
        .expect("cached transcript path");
        let raw_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &raw_session_id,
            Some(claude_home.path()),
        )
        .expect("raw transcript path");
        std::fs::create_dir_all(cached_path.parent().expect("cached parent"))
            .expect("create cached parent");
        std::fs::create_dir_all(raw_path.parent().expect("raw parent")).expect("create raw parent");
        std::fs::write(&cached_path, b"cached\n").expect("write cached transcript");
        std::fs::write(&raw_path, b"raw\ngrown\n").expect("write raw transcript");
        let now = std::time::SystemTime::now();
        filetime::set_file_mtime(
            &cached_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(700)),
        )
        .expect("set cached mtime");
        filetime::set_file_mtime(
            &raw_path,
            filetime::FileTime::from_system_time(now - std::time::Duration::from_secs(5)),
        )
        .expect("set raw mtime");
        let mut ids = ids(
            Some(&cached_session_id),
            Some(&raw_session_id),
            Some(cwd.path()),
            Some(1),
            true,
        );
        ids.raw_provider_transcript_watermark_session_id = Some(stale_watermark_session_id);

        assert_eq!(
            selected_provider_resume_selector_with_claude_home(&ids, Some(claude_home.path())),
            Some(cached_session_id.as_str()),
            "watermark and sticky growth proof belong only to their recorded raw id"
        );
    }

    #[test]
    fn claude_resumable_requires_existing_transcript_for_selected_selector() {
        let _selector_guard = selector_observation_test_lock();
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let session_id = uuid::Uuid::new_v4().to_string();
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &session_id,
            Some(claude_home.path()),
        )
        .expect("transcript path");

        std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .expect("create transcript parent");
        std::fs::write(&transcript_path, b"{}\n").expect("write transcript");

        let ids = ids(Some(&session_id), None, Some(cwd.path()), None, false);
        assert!(provider_resume_selector_is_effective_with_claude_home(
            Some("claude"),
            &ids,
            Some(claude_home.path()),
        ));
    }

    #[test]
    fn claude_resumable_rejects_missing_transcript_or_cwd() {
        let _selector_guard = selector_observation_test_lock();
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let session_id = uuid::Uuid::new_v4().to_string();

        let missing_transcript = ids(Some(&session_id), None, Some(cwd.path()), None, false);
        assert!(!provider_resume_selector_is_effective_with_claude_home(
            Some("claude"),
            &missing_transcript,
            Some(claude_home.path()),
        ));

        let missing_cwd = ids(Some(&session_id), None, None, None, false);
        assert!(!provider_resume_selector_is_effective_with_claude_home(
            Some("claude"),
            &missing_cwd,
            Some(claude_home.path()),
        ));
    }

    #[test]
    fn non_claude_resumable_uses_existing_selector_presence_contract() {
        let codex_ids = ids(None, Some("codex-selector"), None, None, false);
        assert!(provider_resume_selector_is_effective_with_claude_home(
            Some("codex"),
            &codex_ids,
            None,
        ));

        let no_selector = ids(None, Some("   "), None, None, false);
        assert!(!provider_resume_selector_is_effective_with_claude_home(
            Some("codex"),
            &no_selector,
            None,
        ));
    }
}
