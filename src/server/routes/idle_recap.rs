//! Idle-recap endpoint, called by `policies/timeouts/idle-recap.js` for each main-channel
//! session that has been ready for input for 5+ minutes. The route stamps the idle cycle
//! and returns; a detached job summarizes the tmux or transcript scrollback with Haiku
//! (best effort) and posts the card through the provider bot.
//!
//! A recap card must never sit over a live turn. The job re-checks for an active turn
//! before and after the POST and persists the pointer with a compare-and-swap on the turn
//! generation, deleting the just-posted card whenever a turn wins. Turn claims clear only
//! the recap id they captured at claim time: the policy posts once per idle period, so a
//! newer card removed by a stale clear would not come back.

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
use crate::error::AppResult;
use crate::services::discord::idle_recap;
use crate::services::discord::idle_recap::RecapSnapshot;
use crate::services::provider::ProviderKind;

/// POST /api/sessions/{session_key}/idle-recap
pub async fn post_idle_recap(
    State(state): State<AppState>,
    Path(session_key): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
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
    if snapshot.is_routine_session {
        return skip("routine session");
    }

    let Some(channel_id) = idle_recap::resolve_post_channel(&snapshot) else {
        return skip("no discord channel bound to agent");
    };

    let Some(registry) = state.health_registry.clone() else {
        return skip("health registry unavailable (standalone mode)");
    };
    // Post via the provider bot, not notify-bot: Discord routes the `[새 세션 시작]` button
    // interaction to the author's gateway, and notify-bot is HTTP-only.
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

    // Stamp only once a post will be attempted: the early skips above stay eligible,
    // while failures in the detached job still dedupe this idle cycle.
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

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "accepted": true,
            "posted": false,
            "channel_id": channel_id.to_string(),
        })),
    ))
}

async fn run_idle_recap_post_job(
    pool: PgPool,
    session_key: String,
    snapshot: RecapSnapshot,
    channel_id: u64,
    http: Arc<serenity::Http>,
) -> Result<(), String> {
    // Scrollback and Haiku summary are both best effort; without them the card still
    // ships its token/idle header.
    let scrollback = match idle_recap::tmux_session_name_from_key(&session_key) {
        Some(name) => idle_recap::capture_tmux_scrollback(&name).await,
        None => None,
    };
    // The transcript file outlives the pane, so it covers runtimes without a live tmux
    // pane (e.g. `claude-e`) and sessions already torn down.
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
    let composer = match scrollback.as_deref() {
        Some(text) => idle_recap::compose_with_haiku(text).await,
        None => None,
    };
    let relay_probe = match ProviderKind::from_str(&snapshot.provider) {
        Some(provider) => idle_recap::probe_relay_integrity(&snapshot, &provider, channel_id, None),
        None => idle_recap::decide_relay_integrity(idle_recap::RelayIntegrityInput {
            provider: snapshot.provider.clone(),
            session_key: snapshot.session_key.clone(),
            provider_session_id: snapshot
                .claude_session_id
                .as_deref()
                .or(snapshot.raw_provider_session_id.as_deref())
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            channel_id,
            recap_message_id: None,
            output_path: None,
            output_end: None,
            committed_end: None,
            committed_source: None,
            committed_range: None,
            anchor_message_id: None,
            anchor_channel_id: None,
            unknown_reason: Some("provider kind unsupported".to_string()),
        }),
    };
    let content = idle_recap::compose_recap_text(&snapshot, composer.as_ref(), &relay_probe);
    let actions =
        idle_recap::RecapCardActions::for_probe_and_composer(&relay_probe, composer.as_ref());

    // Composing can take seconds and a turn may have started meanwhile; posting now would
    // put a stale idle card over it. The cycle is already stamped, so skipping is safe.
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

    match idle_recap::post_recap_card(http.as_ref(), channel_id, &content, actions).await {
        Ok(message_id) => {
            // The check above and the POST are not atomic, and a claim in between captured the
            // old pointer, so it cannot clear this card. Undo the post if a turn is now active.
            let active_turn_after_post = match ProviderKind::from_str(&snapshot.provider) {
                Some(provider) => idle_recap::channel_has_active_turn(&provider, channel_id).await,
                None => false,
            };
            if idle_recap::post_recheck_action(active_turn_after_post)
                == idle_recap::PostRecheckAction::DeleteAndSkipPersist
            {
                idle_recap::delete_previous_card(http.as_ref(), channel_id, message_id).await;
                tracing::info!(
                    session_key = %session_key,
                    channel_id = channel_id,
                    message_id = message_id,
                    "idle_recap: turn became active during post; deleted just-posted card"
                );
                return Ok(());
            }

            // CAS on the turn generation captured at load: losing to a claim or a newer recap
            // deletes the card, and a claim that commits after us clears it via the pointer.
            let persist_result = match idle_recap::persist_recap_message_id(
                &pool,
                &session_key,
                channel_id,
                message_id,
                snapshot.idle_recap_turn_generation,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    // Delete the now-orphan card; the stamp still dedupes this cycle.
                    idle_recap::delete_previous_card(http.as_ref(), channel_id, message_id).await;
                    return Err(format!("persist: {e}"));
                }
            };
            let previous_card = match persist_result {
                idle_recap::PersistRecapMessageIdResult::Persisted { previous_card } => {
                    previous_card
                }
                idle_recap::PersistRecapMessageIdResult::LostDeleteAndSkip => {
                    idle_recap::delete_previous_card(http.as_ref(), channel_id, message_id).await;
                    tracing::info!(
                        session_key = %session_key,
                        channel_id = channel_id,
                        message_id = message_id,
                        "idle_recap: persist lost to a turn claim or newer recap; deleted just-posted card"
                    );
                    return Ok(());
                }
            };
            if let Some(previous_card) = previous_card {
                idle_recap::delete_previous_card(
                    http.as_ref(),
                    previous_card.channel_id,
                    previous_card.message_id,
                )
                .await;
            }
            match idle_recap::recap_channel_has_newer_card(&pool, channel_id, message_id).await {
                Ok(true) => {
                    let _ = idle_recap::clear_recap_pointer(&pool, &session_key, message_id).await;
                    idle_recap::delete_previous_card(http.as_ref(), channel_id, message_id).await;
                    tracing::info!(
                        session_key = %session_key,
                        channel_id = channel_id,
                        message_id = message_id,
                        "idle_recap: deleted just-posted card because a newer recap already owns the channel"
                    );
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        session_key = %session_key,
                        channel_id = channel_id,
                        message_id = message_id,
                        error = %error,
                        "idle_recap: newer-card check failed; keeping just-posted recap"
                    );
                }
            }
            if let Err(error) = idle_recap::delete_older_recorded_recaps_for_channel(
                http.as_ref(),
                &pool,
                channel_id,
                message_id,
            )
            .await
            {
                tracing::warn!(
                    session_key = %session_key,
                    channel_id = channel_id,
                    message_id = message_id,
                    error = %error,
                    "idle_recap: older channel recap cleanup failed"
                );
            }
            tracing::info!(
                session_key = %session_key,
                channel_id = channel_id,
                message_id = message_id,
                summary_present = composer
                    .as_ref()
                    .and_then(|output| output.summary.as_ref())
                    .is_some(),
                relay_status = relay_probe.status.label(),
                "idle_recap detached post job completed"
            );
            Ok(())
        }
        Err(e) => Err(format!("post: {e}")),
    }
}

fn skip(reason: &str) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::OK,
        Json(json!({"ok": true, "posted": false, "skipped": true, "reason": reason})),
    ))
}

fn error(status: StatusCode, message: &str) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((status, Json(json!({"ok": false, "error": message}))))
}
