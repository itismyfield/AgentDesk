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
//!   6. Otherwise post the new recap card via the provider bot.
//!   7. Re-check active-turn ONCE MORE after the POST returns (#3146 Part 1,
//!      codex R3 P1 — check-then-post TOCTOU). If a turn raced into the
//!      (step-5 check → step-6 POST) window, UNDO the post: delete the
//!      just-posted card and do NOT persist its pointer. Otherwise persist with
//!      a newest-wins conditional swap, then clear any older recap pointer for
//!      the channel so only the newest card survives overlapping recap jobs.
//!
//! Lifecycle hooks live in two places, both now using capture-at-claim +
//! compare-and-clear (codex R3 P2):
//!   - `router::intake_gate` — the moment a Discord message is accepted as a
//!     real turn, it captures the recap pointer synchronously and clears ONLY
//!     that captured id via `idle_recap::spawn_clear_captured_idle_recap_for_channel`.
//!   - `tui_prompt_relay` — same capture-at-claim variant when a TUI-driven
//!     turn becomes active.
//!   - The next 5-min cycle deletes the previous card before posting fresh.
//!
//! Reverse race (codex R2 P2 — closed by capture-at-claim + CAS): a delayed
//! clear spawned by an *old* turn could, in principle, land after a later
//! legitimately-posted card. This is NOT self-healing: the policy posts at most
//! once per idle period (`idle_recap_posted_at < last_heartbeat`), so a card
//! lost to a stale clear stays gone until new activity re-arms the session.
//! Both clear call sites now capture the recap card id that exists when the
//! turn is claimed and clear ONLY that captured id — `delete_previous_card`
//! probes the captured message and `clear_recap_pointer` is a compare-and-clear,
//! so a delayed clear cannot delete or unlink a NEWER card (see
//! `idle_recap::spawn_clear_captured_idle_recap_for_channel`).
//!
//! Forward race (codex R3 P1 — closed by the step-7 post-recheck): a turn that
//! starts AFTER step 5 but BEFORE persist captures the OLD pointer at claim
//! time, so its clear cannot remove THIS card; the step-7 recheck deletes the
//! just-posted card and skips persist. RESIDUAL (inherent, acceptable): a turn
//! starting in the few-ms between the Discord POST returning (step 6) and the
//! recheck (step 7) can still momentarily show the card — but it is NEVER
//! persisted as a live pointer, and the NEXT turn's claim-clear or the user's
//! `[새 세션 시작]` button removes it.

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
    // PR #3c: capture the live tmux scrollback (best-effort) and ask
    // Claude Haiku for a 1-2 sentence Korean summary (also best-effort, 20s
    // timeout). Both legs degrade gracefully to "no summary" — the card
    // still ships its token / idle header in that case.
    let scrollback = match idle_recap::tmux_session_name_from_key(&session_key) {
        Some(name) => idle_recap::capture_tmux_scrollback(&name).await,
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

    match idle_recap::post_recap_card(http.as_ref(), channel_id, &content, actions).await {
        Ok(message_id) => {
            // #3146 Part 1 (codex R3 P1 — check-then-post TOCTOU): the pre-post
            // `should_post_recap` check above and the Discord POST are not
            // atomic. A TUI claim that starts AFTER that check but BEFORE we
            // persist captures the OLD recap pointer (capture-at-claim sees
            // whatever id existed at claim time, not this not-yet-persisted
            // one), so its clear cannot remove THIS card. If we then persisted,
            // a fresh stale `📦 … idle` card would sit over the live turn.
            //
            // So we re-check active-turn AFTER the POST returns and BEFORE we
            // persist. If a turn raced in, UNDO the post: delete the
            // just-posted card and do NOT persist its pointer (return Ok). The
            // idle-cycle stamp at the top already deduped this cycle, so not
            // persisting is safe. The pre-post check is kept as
            // defense-in-depth — it avoids the wasted POST in the common case.
            //
            // Residual window (inherent, acceptable): a turn that starts in the
            // few-ms gap between the Discord POST returning and this recheck can
            // still momentarily show the card. It is then cleared by the NEXT
            // turn's claim-clear or by the user's `[새 세션 시작]` button — it is
            // never persisted as a live pointer here, so it cannot be the target
            // of a future stale-clear race.
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

            // #3148: the persist folds a compare-and-swap on the per-channel
            // turn generation captured at snapshot load. A turn claimed in the
            // (post-recheck → persist) gap — the residual TOCTOU Window 1 — has
            // bumped the generation, so the conditional UPDATE matches 0 rows.
            // The claim's increment and this persist serialize on the same
            // Postgres row, so this CAS is the ATOMIC close of Window 1: if a
            // claim committed first, or a newer recap card already owns this
            // session row, we delete the just-posted card and skip persisting
            // (an expected race outcome, NOT an error — the idle-cycle stamp
            // already deduped this cycle so it does not re-fire). If the
            // persist committed first, the claim's relocated post-claim clear
            // sees the just-committed pointer and removes the card.
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
                    // Best-effort: clear the now-orphan card and report. The
                    // stamp at the top still dedupes this cycle.
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
