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
//!   3. Capture the last ~100 lines of the tmux scrollback (best effort).
//!   4. Ask Claude Haiku for a 1-2 sentence Korean summary (best effort,
//!      20 s timeout; fall back to a header-only card if it fails).
//!   5. Re-check whether a turn became active during the (slow) compose
//!      window (#3146 Part 1, codex clear/post race); if so, SKIP posting.
//!   6. Otherwise delete the previous recap card for this channel (best
//!      effort), post the new one via the notify bot, and persist its id.
//!
//! Lifecycle hooks live in two places:
//!   - `router::message_handler::clear_idle_recap_for_channel` — fires the
//!     moment the user sends the next message in that channel.
//!   - The next 5-min cycle deletes the previous card before posting fresh.
//!
//! Reverse race (codex R2 P2 — now closed by capture-at-claim + CAS): a
//! delayed clear spawned by an *old* turn could, in principle, land after a
//! later legitimately-posted card. This is NOT self-healing: the policy posts
//! at most once per idle period (`idle_recap_posted_at < last_heartbeat`), so
//! a card lost to a stale clear stays gone until new activity re-arms the
//! session. The post-side active-turn recheck (step 5) closes the harmful
//! direction (stale `idle` card over a live turn). The reverse is closed by
//! the claim path capturing the recap card id that exists when the turn is
//! claimed and clearing ONLY that captured id — `delete_previous_card` probes
//! the captured message and `clear_recap_pointer` is a compare-and-clear, so a
//! delayed clear cannot delete or unlink a NEWER card (see
//! `idle_recap::spawn_clear_captured_idle_recap_for_channel`).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

use super::AppState;
use crate::services::discord::idle_recap;
use crate::services::discord::idle_recap::RecapSnapshot;
use crate::services::provider::ProviderKind;

/// POST /api/sessions/{session_key}/idle-recap
pub async fn post_idle_recap(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Some(pool) = state.pg_pool.as_ref().cloned() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "pg pool unavailable");
    };

    let mut snapshot = match idle_recap::load_recap_snapshot(&pool, &session_key).await {
        Ok(Some(snap)) => snap,
        Ok(None) => return error(StatusCode::NOT_FOUND, "session not found"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("load: {e}")),
    };

    if !snapshot.has_resumable_provider_session() {
        return skip("no resumable provider session");
    }

    let Some(channel_id) = idle_recap::resolve_post_channel(&snapshot) else {
        return skip("no discord channel bound to agent");
    };

    let Some(registry) = state.health_registry.clone() else {
        return skip("health registry unavailable (standalone mode)");
    };
    // Post via the provider bot (claude/codex), not notify-bot. The recap
    // card carries an interactive `[새 세션 시작]` button — Discord routes the
    // InteractionCreate to the application_id of the message author. The
    // notify-bot is HTTP-only (no gateway), so its interactions are never
    // received and users see "상호작용에 실패했습니다" after the 3s ACK timeout.
    // The provider bots have gateways wired into `intake_gate::handle_event`,
    // which already dispatches `idle_recap_interaction::handle_*`.
    let http = match crate::services::discord::health::resolve_bot_http(
        registry.as_ref(),
        &snapshot.provider,
    )
    .await
    {
        Ok(http) => http,
        Err(_) => return skip("provider bot not registered for recap interaction"),
    };
    idle_recap::attach_live_context_usage(registry.as_ref(), &mut snapshot, channel_id).await;

    // Stamp only after the recap is known to be meaningful and postable. No
    // resumable-session skips should stay eligible until a real session id is
    // recorded; transient delivery skips still dedupe this idle cycle.
    if let Err(e) = idle_recap::stamp_recap_cycle(&pool, &session_key).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("stamp: {e}"));
    }

    let session_key_for_job = session_key.clone();
    tokio::spawn(async move {
        if let Err(error) = run_idle_recap_post_job(
            pool,
            session_key_for_job.clone(),
            snapshot,
            channel_id,
            http,
        )
        .await
        {
            tracing::warn!(
                session_key = %session_key_for_job,
                channel_id = channel_id,
                error = %error,
                "idle_recap detached post job failed"
            );
        }
    });

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "accepted": true,
            "posted": false,
            "channel_id": channel_id.to_string(),
        })),
    )
}

async fn run_idle_recap_post_job(
    pool: PgPool,
    session_key: String,
    snapshot: RecapSnapshot,
    channel_id: u64,
    http: Arc<serenity::Http>,
) -> Result<(), String> {
    // PR #3c: capture the live tmux scrollback (best-effort) and ask
    // Claude Haiku for a 1-2 sentence Korean summary (also best-effort, 20s
    // timeout). Both legs degrade gracefully to "no summary" — the card
    // still ships its token / idle header in that case.
    let scrollback = match idle_recap::tmux_session_name_from_key(&session_key) {
        Some(name) => idle_recap::capture_tmux_scrollback(name).await,
        None => None,
    };
    // Fallback for runtimes without a live tmux pane (notably `claude-e`,
    // which spawns per-turn and exits). Also rescues recap summaries when a
    // long-lived tmux session has already been torn down: the transcript
    // file outlives the pane, so we get a usable scrollback either way.
    let scrollback = match (
        scrollback,
        snapshot.cwd.as_deref(),
        snapshot.claude_session_id.as_deref(),
    ) {
        (Some(text), _, _) => Some(text),
        (None, Some(cwd), Some(session_id)) if !cwd.is_empty() && !session_id.is_empty() => {
            idle_recap::capture_transcript_scrollback(std::path::Path::new(cwd), session_id).await
        }
        _ => None,
    };
    let summary = match scrollback.as_deref() {
        Some(text) => idle_recap::summarize_with_haiku(text).await,
        None => None,
    };
    let content = idle_recap::compose_recap_text(&snapshot, summary.as_deref());

    // #3146 Part 1 (codex clear/post race): the scrollback + Haiku compose
    // above can take seconds. During that window a TUI-driven turn may have
    // become active and the claim path (`claim_tui_direct_synthetic_turn`)
    // may have already cleared the previous recap card. Re-check turn
    // activity RIGHT before we commit, and skip posting+persisting entirely
    // if a turn is now in progress — otherwise we would post a FRESH stale
    // `📦 … idle` card OVER the live turn (and persist its pointer),
    // re-opening the exact bug #3146 closes. The idle-cycle stamp at the top
    // of the route already deduped this cycle, so skipping is safe (we do NOT
    // post-then-delete; we just don't post).
    //
    // codex R2 P1: the recheck consults the MAILBOX active-turn FIRST (the
    // authoritative signal `mailbox_try_start_turn` sets at the very start of
    // the claim), ORed with the inflight sidecar for defense-in-depth. The
    // claim path makes the mailbox turn active BEFORE it writes the inflight
    // sidecar, so reading only the sidecar left a lag window in which this
    // recheck saw "idle" and could post a stale card over a now-active turn.
    // Reading the mailbox closes that window. Genuinely-idle channels have no
    // active mailbox turn and no (non-stale) inflight, so they still post the
    // recap as before.
    let active_turn = match ProviderKind::from_str(&snapshot.provider) {
        Some(provider) => idle_recap::channel_has_active_turn(&provider, channel_id).await,
        None => false,
    };
    if !idle_recap::should_post_recap(active_turn) {
        tracing::info!(
            session_key = %session_key,
            channel_id = channel_id,
            "idle_recap post skipped: turn became active during recap compose"
        );
        return Ok(());
    }

    if let (Some(prev_msg), Some(prev_chan)) =
        (snapshot.previous_message_id, snapshot.previous_channel_id)
    {
        idle_recap::delete_previous_card(http.as_ref(), prev_chan as u64, prev_msg as u64).await;
    }

    match idle_recap::post_recap_card(http.as_ref(), channel_id, &content).await {
        Ok(message_id) => {
            if let Err(e) =
                idle_recap::persist_recap_message_id(&pool, &session_key, channel_id, message_id)
                    .await
            {
                // Best-effort: clear the now-orphan card and report. The
                // stamp at the top still dedupes this cycle.
                idle_recap::delete_previous_card(http.as_ref(), channel_id, message_id).await;
                return Err(format!("persist: {e}"));
            }
            tracing::info!(
                session_key = %session_key,
                channel_id = channel_id,
                message_id = message_id,
                summary_present = summary.is_some(),
                "idle_recap detached post job completed"
            );
            Ok(())
        }
        Err(e) => Err(format!("post: {e}")),
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
