//! Idle-recap notification endpoint — called once per 5-min policy cycle by
//! `policies/timeouts/idle-recap.js` for each main-channel session that has
//! been ready-for-input for ≥5 minutes.
//!
//! Flow:
//!   1. Pull session + agent channel bindings + last token/heartbeat in one
//!      SQL hit (`idle_recap::load_recap_snapshot`).
//!   2. Resolve the Discord channel id for this provider via the
//!      agent's bindings (claude → `discord_channel_cc`,
//!      codex → `discord_channel_cdx`, fallback → `discord_channel_id`).
//!   3. Capture the last ~500 lines of the tmux scrollback (best effort).
//!   4. Ask opencode for a 1-2 sentence Korean summary (best effort,
//!      20 s timeout; fall back to a header-only card if it fails).
//!   5. Delete the previous recap card for this channel (best effort), post
//!      the new one via the notify bot, and persist its message id.
//!
//! Lifecycle hooks live in two places:
//!   - `router::message_handler::clear_idle_recap_for_channel` — fires the
//!     moment the user sends the next message in that channel.
//!   - The next 5-min cycle deletes the previous card before posting fresh.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use super::AppState;
use crate::services::discord::idle_recap;

/// POST /api/sessions/{session_key}/idle-recap
pub async fn post_idle_recap(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Some(pool) = state.pg_pool.as_ref().cloned() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "pg pool unavailable");
    };

    // Always stamp `idle_recap_posted_at` first so the policy dedupes this
    // cycle even if the renderer below decides to skip (no channel binding,
    // notify bot offline, …). Without this, a transient renderer failure
    // would cause the policy to retry on every tick. SQL lives in the
    // service module to keep this route handler SRP-clean (no raw `sqlx::query`
    // alongside the `json!` response shaping).
    if let Err(e) = idle_recap::stamp_recap_cycle(&pool, &session_key).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("stamp: {e}"));
    }

    let snapshot = match idle_recap::load_recap_snapshot(&pool, &session_key).await {
        Ok(Some(snap)) => snap,
        Ok(None) => return error(StatusCode::NOT_FOUND, "session not found"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("load: {e}")),
    };

    let Some(channel_id) = idle_recap::resolve_post_channel(&snapshot) else {
        return skip("no discord channel bound to agent");
    };

    let Some(registry) = state.health_registry.clone() else {
        return skip("health registry unavailable (standalone mode)");
    };
    let Some(http) = registry.notify_http_clone().await else {
        return skip("notify bot not registered");
    };

    // PR #3c: capture the live tmux scrollback (best-effort) and ask
    // opencode for a 1-2 sentence Korean summary (also best-effort, 20s
    // timeout). Both legs degrade gracefully to "no summary" — the card
    // still ships its token / idle header in that case.
    let scrollback = match idle_recap::tmux_session_name_from_key(&session_key) {
        Some(name) => idle_recap::capture_tmux_scrollback(name).await,
        None => None,
    };
    let summary = match scrollback.as_deref() {
        Some(text) => idle_recap::summarize_with_opencode(text).await,
        None => None,
    };
    let content = idle_recap::compose_recap_text(&snapshot, summary.as_deref());

    if let (Some(prev_msg), Some(prev_chan)) =
        (snapshot.previous_message_id, snapshot.previous_channel_id)
    {
        idle_recap::delete_previous_card(&http, prev_chan as u64, prev_msg as u64).await;
    }

    match idle_recap::post_recap_card(&http, channel_id, &content).await {
        Ok(message_id) => {
            if let Err(e) =
                idle_recap::persist_recap_message_id(&pool, &session_key, channel_id, message_id)
                    .await
            {
                // Best-effort: clear the now-orphan card and report. The
                // stamp at the top still dedupes this cycle.
                idle_recap::delete_previous_card(&http, channel_id, message_id).await;
                return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("persist: {e}"));
            }
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "posted": true,
                    "channel_id": channel_id.to_string(),
                    "message_id": message_id.to_string(),
                    "summary_present": summary.is_some(),
                })),
            )
        }
        Err(e) => error(StatusCode::BAD_GATEWAY, &format!("post: {e}")),
    }
}

fn skip(reason: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({"ok": true, "posted": false, "skipped": true, "reason": reason})),
    )
}

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"ok": false, "error": message})))
}
