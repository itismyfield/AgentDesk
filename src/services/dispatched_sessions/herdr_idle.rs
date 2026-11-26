//! Kill-tmux's idle answer for a row recording a Herdr execution: the idle end closes its pane and
//! retires it, or says once per session and reason why the session stays; no preserve alarm.

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::app_state::AppState;
use crate::services::discord::herdr_terminate::{IdleEnd, expire_idle_herdr};

/// `None` when the row records no Herdr execution, so the caller keeps its host-guard answer.
pub(super) async fn herdr_idle_kill(
    state: &AppState,
    pool: &sqlx::PgPool,
    session_key: &str,
    tmux_name: &str,
) -> Option<(StatusCode, Json<Value>)> {
    let registry = state.health_registry.as_deref();
    let reply = match expire_idle_herdr(registry, pool, session_key).await? {
        Ok(IdleEnd {
            closed,
            disconnected,
        }) => {
            if closed {
                crate::services::termination_audit::record_termination_with_handles(
                    Some(pool),
                    session_key,
                    None,
                    "kill_tmux_api",
                    "idle_session_expiry",
                    Some("herdr pane closed after idle timeout; provider session kept"),
                    None,
                    None,
                    Some(true),
                );
            }
            tracing::info!(
                session_key,
                closed,
                disconnected,
                "kill-tmux: herdr idle end retired the execution (provider session kept)"
            );
            json!({
                "ok": true,
                "host": "herdr",
                "tmux_killed": closed,
                "tmux_was_alive": closed,
                "tmux_session_name": tmux_name,
                "session_row_preserved": true,
                "session_row_disconnected": disconnected,
            })
        }
        Err(kept) => {
            let (result, detail) = kept.outcome();
            if super::tmux_cleanup::host_refusal_first_sighting(&format!("{session_key}|{result}"))
            {
                tracing::warn!(
                    session_key,
                    result,
                    detail,
                    "kill-tmux: herdr idle end kept the session"
                );
            }
            json!({
                "ok": true,
                "host": "herdr",
                "tmux_killed": false,
                "tmux_was_alive": null,
                "tmux_session_name": tmux_name,
                "session_row_preserved": true,
                "skipped_provider_activity_guard": true,
                "preserved_reason": format!("herdr_{result}"),
                "detail": detail,
            })
        }
    };
    Some((StatusCode::OK, Json(reply)))
}
