//! Idle-recap notification endpoint (#PR3) — called once per 5-min policy
//! cycle by `policies/timeouts/idle-recap.js` for each main-channel session
//! that has been ready-for-input for ≥5 minutes.
//!
//! This first cut intentionally lands the plumbing only:
//!   * SQL: stamp `sessions.idle_recap_posted_at = NOW()` so the policy
//!     dedupe (`idle_recap_posted_at < NOW() - INTERVAL '5 minutes'`) works
//!     without spamming Discord while the rendering layer is in progress.
//!   * Response carries `posted: false, skipped: true, reason: "renderer pending"`
//!     so operators can confirm the cycle from logs without surfacing a card
//!     to the user.
//!
//! Follow-up PR will replace the stub body with:
//!   1. `tmux capture-pane -p -S -500` on the session's tmux pane,
//!   2. opencode/Haiku summarisation,
//!   3. Discord channel posting (with token-occupancy panel and a future
//!      `[새 세션 시작]` button — kept out of scope here to keep the
//!      reviewable diff small and the policy → API contract stable),
//!   4. previous-message deletion via `sessions.idle_recap_message_id`,
//!   5. clearing the message id from `message_handler` when the user sends
//!      the next turn.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use super::AppState;

/// POST /api/sessions/{session_key}/idle-recap
pub async fn post_idle_recap(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Some(pool) = state.pg_pool.as_ref().cloned() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": "pg pool unavailable"})),
        );
    };

    // Stamp the dedupe column so the policy treats this session as "handled
    // for this 5-min cycle" even before the renderer is wired up. Once the
    // renderer lands, this UPDATE will be expanded to also write
    // `idle_recap_message_id` and `idle_recap_channel_id` for the posted card.
    let updated = sqlx::query(
        "UPDATE sessions
         SET idle_recap_posted_at = NOW()
         WHERE session_key = $1",
    )
    .bind(&session_key)
    .execute(&pool)
    .await;

    match updated {
        Ok(result) if result.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "session not found"})),
        ),
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "posted": false,
                "skipped": true,
                "reason": "renderer pending",
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("{e}")})),
        ),
    }
}
