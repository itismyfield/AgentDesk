//! #138: Channel queue management + dispatch cancel API.
//!
//! Provides operational endpoints for pipeline incident recovery.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;

// ── GET /api/channels/:id/queue ─────────────────────────────────

/// List intervention queue for a channel.
pub async fn list_channel_queue(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> Json<serde_json::Value> {
    if channel_id.parse::<u64>().is_err() {
        return Json(json!({"error": "invalid channel_id", "queue": []}));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({"error": "postgres pool unavailable", "queue": []}));
    };
    let dispatches = sqlx::query(
        "SELECT
            td.id,
            td.dispatch_type,
            td.status,
            td.title,
            td.created_at::TEXT AS created_at,
            kc.github_issue_number::BIGINT AS github_issue_number
         FROM task_dispatches td
         JOIN kanban_cards kc ON td.kanban_card_id = kc.id
         JOIN agents a ON td.to_agent_id = a.id
         WHERE (
             a.discord_channel_id = $1 OR a.discord_channel_alt = $1 OR
             a.discord_channel_cc = $1 OR a.discord_channel_cdx = $1
         )
           AND td.status IN ('pending', 'dispatched')
         ORDER BY td.created_at DESC",
    )
    .bind(&channel_id)
    .fetch_all(pool)
    .await
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| queue_channel_dispatch_row_to_json_pg(&row).ok())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    Json(json!({"channel_id": channel_id, "dispatches": dispatches}))
}

// ── GET /api/dispatches/pending ─────────────────────────────────

/// List all pending dispatches across all agents.
pub async fn list_pending_dispatches(State(state): State<AppState>) -> Json<serde_json::Value> {
    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({"error": "postgres pool unavailable", "dispatches": [], "count": 0}));
    };
    let dispatches = sqlx::query(
        "SELECT
            td.id,
            td.kanban_card_id,
            td.to_agent_id,
            td.dispatch_type,
            td.status,
            td.title,
            td.thread_id,
            td.created_at::TEXT AS created_at,
            td.retry_count::BIGINT AS retry_count,
            kc.github_issue_number::BIGINT AS github_issue_number,
            kc.status AS card_status
         FROM task_dispatches td
         JOIN kanban_cards kc ON td.kanban_card_id = kc.id
         WHERE td.status IN ('pending', 'dispatched')
         ORDER BY td.created_at DESC",
    )
    .fetch_all(pool)
    .await
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| pending_dispatch_row_to_json_pg(&row).ok())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    Json(json!({"dispatches": dispatches, "count": dispatches.len()}))
}

// ── POST /api/dispatches/:id/cancel ─────────────────────────────

/// Cancel a specific dispatch.
pub async fn cancel_dispatch(
    State(state): State<AppState>,
    Path(dispatch_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.queue_service().cancel_dispatch(&dispatch_id).await {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(error) => error.into_json_response(),
    }
}

// ── POST /api/dispatches/cancel-all ─────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CancelAllBody {
    pub kanban_card_id: Option<String>,
    pub agent_id: Option<String>,
}

/// Cancel all pending/dispatched dispatches matching filters.
pub async fn cancel_all_dispatches(
    State(state): State<AppState>,
    Json(body): Json<CancelAllBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .queue_service()
        .cancel_all_dispatches(body.kanban_card_id.as_deref(), body.agent_id.as_deref())
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => error.into_json_response(),
    }
}

// ── POST /api/turns/:channel_id/cancel ──────────────────────────

/// Query parameters for `POST /api/turns/:channel_id/cancel`.
///
/// `force=true` requests the historical hard-kill path: the live turn's tmux
/// session and the entire child PID tree (cargo, claude CLI, …) get SIGKILLed
/// via `kill_pid_tree`. Reserve for explicit recovery — operators reaching
/// for "remove queued message" almost never want this (#1196).
///
/// Default (`force=false`): preserve the live tmux session and watcher; only
/// drain the channel mailbox. Tool subprocesses keep running.
#[derive(Debug, Default, Deserialize)]
pub struct CancelTurnQuery {
    #[serde(default)]
    pub force: bool,
}

/// Cancel the active turn in a channel.
///
/// Default (`force=false`): preserves the live provider session and watcher;
/// drains the channel mailbox. The currently running tool subtree is NOT
/// SIGKILLed — what an operator usually means by "queue 정리".
///
/// `force=true`: tear the tmux session down, SIGKILL the PID tree, clear
/// inflight state. The turn will not complete gracefully; in-flight
/// `cargo`/`claude` subprocesses get terminated.
pub async fn cancel_turn(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    Query(query): Query<CancelTurnQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .queue_service()
        .cancel_turn(state.health_registry.as_ref(), &channel_id, query.force)
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => error.into_json_response(),
    }
}

// ── GET /api/channels/:id/watcher-state ─────────────────────────

/// #964 / #1133: snapshot the tmux-watcher lifecycle state for a channel.
///
/// Core fields (#964): `{ provider, attached, tmux_session,
/// last_relay_offset, inflight_state_present, last_relay_ts_ms,
/// last_capture_offset, unread_bytes, desynced, reconnect_count,
/// has_pending_queue }`.
///
/// #1133 enriched read-only diagnostics (omitted when their source is
/// absent): `inflight_started_at`, `inflight_updated_at`,
/// `inflight_user_msg_id`, `inflight_current_msg_id`,
/// `watcher_owner_channel_id`, `tmux_session_alive` (PID check via
/// `tmux has-session`), and `mailbox_active_user_msg_id`. All fields are
/// PII-free scalars so the response is safe for non-privileged operator
/// dashboards.
///
/// Used by operators to diagnose "watcher detached silently while tmux
/// still producing output" incidents and pre-watcher mailbox queueing.
/// `desynced=true` means a live tmux-backed inflight appears orphaned,
/// is owned by another channel, or its capture file diverges from relay
/// telemetry while stale for at least 30 seconds.
///
/// 404 is returned when no watcher, no inflight state, and no mailbox
/// engagement (active turn or queued intervention) is known for the
/// channel across all registered providers.
pub async fn get_watcher_state(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "channel_id must be a numeric Discord channel ID"})),
            );
        }
    };

    let Some(registry) = state.health_registry.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "health registry unavailable in this runtime"})),
        );
    };

    match registry.snapshot_watcher_state(channel_num).await {
        Some(snapshot) => {
            let body = serde_json::to_value(&snapshot)
                .unwrap_or_else(|_| json!({"error": "failed to serialize watcher snapshot"}));
            (StatusCode::OK, Json(body))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "no watcher, relay-coord, or inflight state for this channel",
                "channel_id": channel_id,
            })),
        ),
    }
}

// ── POST /api/turns/:channel_id/extend-timeout ───────────────────

#[derive(Deserialize)]
pub struct ExtendTimeoutBody {
    /// Seconds to extend. Default: 1800 (30 min).
    #[serde(default = "default_extend_secs")]
    pub extend_secs: u64,
}

fn default_extend_secs() -> u64 {
    1800
}

/// Extend the watchdog timeout for an active turn in a channel.
///
/// The per-turn hard cap moves with accepted operator extensions and is bounded
/// by `AGENTDESK_TURN_TIMEOUT_EXTEND_MAX_COUNT` and
/// `AGENTDESK_TURN_TIMEOUT_EXTEND_MAX_TOTAL_SECS`.
pub async fn extend_turn_timeout(
    Path(channel_id): Path<String>,
    Json(body): Json<ExtendTimeoutBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "channel_id must be a numeric Discord channel ID"})),
            );
        }
    };

    match crate::services::discord::extend_watchdog_deadline(channel_num, body.extend_secs).await {
        Ok(extension) => {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let remaining_min = (extension.new_deadline_ms - now_ms) / 1000 / 60;
            let max_remaining_min = (extension.max_deadline_ms - now_ms) / 1000 / 60;
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "channel_id": channel_id,
                    "requested_deadline_ms": extension.requested_deadline_ms,
                    "new_deadline_ms": extension.new_deadline_ms,
                    "effective_deadline_ms": extension.new_deadline_ms,
                    "max_deadline_ms": extension.max_deadline_ms,
                    "remaining_minutes": remaining_min,
                    "effective_remaining_minutes": remaining_min,
                    "max_remaining_minutes": max_remaining_min,
                    "requested_extend_secs": extension.requested_extend_secs,
                    "applied_extend_secs": extension.applied_extend_secs,
                    "extension_count": extension.extension_count,
                    "extension_count_limit": extension.extension_count_limit,
                    "extension_total_secs": extension.extension_total_secs,
                    "extension_total_secs_limit": extension.extension_total_secs_limit,
                    "clamped": extension.clamped,
                })),
            )
        }
        Err(crate::services::turn_orchestrator::WatchdogDeadlineExtensionError::MailboxUnavailable) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no mailbox for channel", "channel_id": channel_id})),
        ),
        Err(crate::services::turn_orchestrator::WatchdogDeadlineExtensionError::NoActiveTurn) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "no active turn for channel", "channel_id": channel_id})),
        ),
        Err(crate::services::turn_orchestrator::WatchdogDeadlineExtensionError::ExtensionLimitReached {
            extension_count,
            extension_count_limit,
            extension_total_secs,
            extension_total_secs_limit,
        }) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "watchdog extension limit reached",
                "channel_id": channel_id,
                "extension_count": extension_count,
                "extension_count_limit": extension_count_limit,
                "extension_total_secs": extension_total_secs,
                "extension_total_secs_limit": extension_total_secs_limit,
                "clamped": true,
            })),
        ),
    }
}

fn queue_channel_dispatch_row_to_json_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<serde_json::Value, String> {
    Ok(json!({
        "dispatch_id": row.try_get::<String, _>("id").map_err(|error| format!("decode queue dispatch id: {error}"))?,
        "dispatch_type": row.try_get::<String, _>("dispatch_type").map_err(|error| format!("decode queue dispatch_type: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode queue status: {error}"))?,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode queue title: {error}"))?,
        "created_at": row.try_get::<Option<String>, _>("created_at").map_err(|error| format!("decode queue created_at: {error}"))?,
        "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").map_err(|error| format!("decode queue github_issue_number: {error}"))?,
    }))
}

fn pending_dispatch_row_to_json_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<serde_json::Value, String> {
    Ok(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode pending dispatch id: {error}"))?,
        "kanban_card_id": row.try_get::<String, _>("kanban_card_id").map_err(|error| format!("decode pending kanban_card_id: {error}"))?,
        "to_agent_id": row.try_get::<String, _>("to_agent_id").map_err(|error| format!("decode pending to_agent_id: {error}"))?,
        "dispatch_type": row.try_get::<String, _>("dispatch_type").map_err(|error| format!("decode pending dispatch_type: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode pending status: {error}"))?,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode pending title: {error}"))?,
        "thread_id": row.try_get::<Option<String>, _>("thread_id").map_err(|error| format!("decode pending thread_id: {error}"))?,
        "created_at": row.try_get::<Option<String>, _>("created_at").map_err(|error| format!("decode pending created_at: {error}"))?,
        "retry_count": row.try_get::<i64, _>("retry_count").map_err(|error| format!("decode pending retry_count: {error}"))?,
        "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").map_err(|error| format!("decode pending github_issue_number: {error}"))?,
        "card_status": row.try_get::<String, _>("card_status").map_err(|error| format!("decode pending card_status: {error}"))?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn extend_turn_timeout_reports_effective_deadline_and_cap() {
        let channel_id = ChannelId::new(1_417_000_001);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let token = Arc::new(CancelToken::new());
        let now_ms = chrono::Utc::now().timestamp_millis();
        token
            .watchdog_deadline_ms
            .store(now_ms + 60_000, Ordering::Relaxed);
        token
            .watchdog_max_deadline_ms
            .store(now_ms + 120_000, Ordering::Relaxed);
        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), MessageId::new(11))
                .await
        );

        let (status, Json(body)) = extend_turn_timeout(
            Path(channel_id.get().to_string()),
            Json(ExtendTimeoutBody { extend_secs: 30 }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["clamped"], false);
        assert_eq!(body["requested_extend_secs"], 30);
        assert_eq!(body["applied_extend_secs"], 30);
        assert_eq!(body["new_deadline_ms"], body["effective_deadline_ms"]);
        assert!(body["effective_remaining_minutes"].as_i64().unwrap() >= 1);
        assert!(
            body["max_deadline_ms"].as_i64().unwrap() >= body["new_deadline_ms"].as_i64().unwrap()
        );
    }
}
