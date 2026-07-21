use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use sqlx::Row;

use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

use super::SharedData;

fn preserve_dispatch_on_watcher_death(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("review" | "phase-gate"))
}

async fn load_dispatch_type_for_restart_handoff(
    shared: &SharedData,
    dispatch_id: &str,
) -> Option<String> {
    let pool = shared.pg_pool.as_ref()?;
    sqlx::query("SELECT dispatch_type FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| {
            row.try_get::<Option<String>, _>("dispatch_type")
                .ok()
                .flatten()
        })
}

fn seed_restart_handoff_session_metadata(
    sessions: &mut std::collections::HashMap<ChannelId, super::DiscordSession>,
    channel_id: ChannelId,
    state: &super::inflight::InflightTurnState,
) -> bool {
    let Some(channel_name) = state
        .channel_name
        .as_ref()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
    else {
        return false;
    };

    let session = sessions
        .entry(channel_id)
        .or_insert_with(|| super::DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel_id.get()),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: super::runtime_store::load_generation(),
        });

    let mut changed = false;
    if session
        .channel_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_none()
    {
        session.channel_name = Some(channel_name);
        changed = true;
    }
    if session.channel_id.is_none() {
        session.channel_id = Some(channel_id.get());
        changed = true;
    }
    session.last_active = tokio::time::Instant::now();
    changed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartHandoffScope {
    ExactMetadata,
    ProviderChannelScopedFallback,
}

pub(super) fn resolve_restart_handoff_scope(
    state: &super::inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: &str,
) -> RestartHandoffScope {
    let tmux_matches = state.tmux_session_name.as_deref() == Some(tmux_session_name);
    let output_matches = state.output_path.as_deref() == Some(output_path);
    if tmux_matches || output_matches {
        RestartHandoffScope::ExactMetadata
    } else {
        RestartHandoffScope::ProviderChannelScopedFallback
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartHandoffNoticeTarget {
    Edit(u64),
    SkipRebindOrigin,
    MissingCurrentMessage,
}

fn restart_handoff_notice_target(
    state: &super::inflight::InflightTurnState,
) -> RestartHandoffNoticeTarget {
    if state.rebind_origin {
        return RestartHandoffNoticeTarget::SkipRebindOrigin;
    }
    if state.current_msg_id == 0 {
        return RestartHandoffNoticeTarget::MissingCurrentMessage;
    }
    RestartHandoffNoticeTarget::Edit(state.current_msg_id)
}

fn forget_completion_footer_for_restart_handoff(
    channel_id: ChannelId,
    message_id: serenity::MessageId,
) -> bool {
    super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
        channel_id, message_id,
    )
}

pub(super) fn resolve_dispatched_thread_dispatch_from_db(
    pg_pool: Option<&sqlx::PgPool>,
    thread_channel_id: u64,
) -> Option<String> {
    let pg_pool = pg_pool?;
    let thread_channel_id = thread_channel_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            if let Some(dispatch_id) = sqlx::query_scalar::<_, String>(
                "SELECT id FROM task_dispatches
                 WHERE status = 'dispatched' AND thread_id = $1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
            )
            .bind(&thread_channel_id)
            .fetch_optional(&pool)
            .await
            .map_err(|error| format!("load pg dispatched thread dispatch: {error}"))?
            {
                return Ok(Some(dispatch_id));
            }

            sqlx::query_scalar::<_, String>(
                "SELECT active_dispatch_id FROM sessions
                 WHERE thread_channel_id = $1
                   AND status IN ('turn_active', 'working')
                   AND active_dispatch_id IS NOT NULL
                 ORDER BY COALESCE(last_heartbeat, created_at) DESC, id DESC
                 LIMIT 1",
            )
            .bind(&thread_channel_id)
            .fetch_optional(&pool)
            .await
            .map_err(|error| format!("load pg session dispatch fallback: {error}"))
        },
        |message| message,
    )
    .ok()
    .flatten()
}

fn build_restart_handoff_session_key(
    state: &super::inflight::InflightTurnState,
    token_hash: &str,
    provider_kind: &ProviderKind,
) -> Option<String> {
    state
        .session_key
        .as_ref()
        .filter(|key| !key.trim().is_empty())
        .cloned()
        .or_else(|| {
            state
                .tmux_session_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(|tmux_name| {
                    super::adk_session::build_namespaced_session_key(
                        token_hash,
                        provider_kind,
                        tmux_name,
                    )
                })
        })
        .or_else(|| {
            state
                .channel_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(|channel_name| {
                    let tmux_name = provider_kind.build_tmux_session_name(channel_name);
                    super::adk_session::build_namespaced_session_key(
                        token_hash,
                        provider_kind,
                        &tmux_name,
                    )
                })
        })
}

async fn clear_restart_handoff_provider_session(
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider_kind: &ProviderKind,
    state: &super::inflight::InflightTurnState,
) {
    // #1085 (908-3): preserve provider session_id across watcher-death recovery.
    //
    // Previously this site cleared both the in-memory `DiscordSession.session_id`
    // AND the DB-side `claude_session_id` via `clear_provider_session_id`, which
    // forced the next user turn to start a fresh provider session even though
    // the underlying tmux session (and the provider session running inside it)
    // typically survives the watcher death. The provider session_id is just an
    // opaque resume token: if the resume fails the CLI transparently falls
    // back to creating a new session, so keeping it costs nothing and lets
    // healthy resumes happen on the next turn.
    //
    // We still log the recovery event so the operator can correlate it with
    // any subsequent resume failure, and we still let `clear_inflight_state`
    // (called by the parent `start_restart_handoff_from_state`) drop the
    // in-flight turn metadata — that part really must be cleared because the
    // bound dispatch was just failed.
    let session_key =
        match build_restart_handoff_session_key(state, &shared.token_hash, provider_kind) {
            Some(key) => Some(key),
            None => {
                super::adk_session::build_adk_session_key(shared, channel_id, provider_kind, None)
                    .await
            }
        };
    let preserved_session_id = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone())
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    match (session_key.as_deref(), preserved_session_id.as_deref()) {
        (Some(key), Some(sid)) => tracing::info!(
            "  [{ts}] ↻ watcher death recovery: preserved provider session before restart handoff for channel {} ({} session_id={})",
            channel_id.get(),
            key,
            sid
        ),
        (Some(key), None) => tracing::info!(
            "  [{ts}] ↻ watcher death recovery: no provider session to preserve for channel {} ({})",
            channel_id.get(),
            key
        ),
        (None, Some(sid)) => tracing::info!(
            "  [{ts}] ↻ watcher death recovery: preserved provider session before restart handoff for channel {} (session_id={})",
            channel_id.get(),
            sid
        ),
        (None, None) => tracing::info!(
            "  [{ts}] ↻ watcher death recovery: no provider session to preserve for channel {}",
            channel_id.get()
        ),
    }
}

pub(super) async fn start_restart_handoff_from_state(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider_kind: &ProviderKind,
    state: super::inflight::InflightTurnState,
    best_response: &str,
) -> bool {
    let stale_text = super::turn_bridge::stale_inflight_message(best_response);
    match restart_handoff_notice_target(&state) {
        RestartHandoffNoticeTarget::Edit(current_msg_id) => {
            let current_msg_id = serenity::MessageId::new(current_msg_id);
            forget_completion_footer_for_restart_handoff(channel_id, current_msg_id);
            let relay_ok = super::formatting::replace_long_message_raw(
                http,
                channel_id,
                current_msg_id,
                &stale_text,
                shared,
            )
            .await
            .is_ok();
            if !relay_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ watcher death recovery: handoff notice failed before dispatch failure — preserving inflight for retry"
                );
                return false;
            }
        }
        RestartHandoffNoticeTarget::SkipRebindOrigin => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ watcher death recovery: rebind-origin inflight has no Discord handoff message for channel {}, continuing cleanup without notice",
                channel_id.get()
            );
        }
        RestartHandoffNoticeTarget::MissingCurrentMessage => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher death recovery: inflight current_msg_id=0 for channel {}, continuing cleanup without handoff notice",
                channel_id.get()
            );
        }
    }

    clear_restart_handoff_provider_session(channel_id, shared, provider_kind, &state).await;

    // tmux death during an inflight turn usually fails the bound dispatch.
    // Review and phase-gate dispatches are different: their durable outcome
    // is submitted through explicit verdict/repair paths. Marking them failed
    // from watcher death can erase a verdict-ready turn before the endpoint
    // write lands, so preserve those rows for recovery/retry.
    if let Some(dispatch_id) = state.dispatch_id.as_deref() {
        let dispatch_type = load_dispatch_type_for_restart_handoff(shared, dispatch_id).await;
        if preserve_dispatch_on_watcher_death(dispatch_type.as_deref()) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ↻ watcher death recovery: preserved {} dispatch {} for explicit verdict/repair recovery",
                dispatch_type.as_deref().unwrap_or("unknown"),
                dispatch_id
            );
        } else {
            let failure_text = format!(
                "tmux session died mid-turn (watcher death recovery) — session={}",
                state.tmux_session_name.as_deref().unwrap_or("<unknown>")
            );
            super::turn_bridge::fail_dispatch_with_retry(
                shared.api_port,
                Some(dispatch_id),
                &failure_text,
            )
            .await;
        }
    }

    let seeded_channel_name = {
        let mut data = shared.core.lock().await;
        seed_restart_handoff_session_metadata(&mut data.sessions, channel_id, &state)
    };
    if seeded_channel_name {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: seeded session metadata after interrupted restart cleanup for channel {}",
            channel_id.get()
        );
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ watcher death recovery: suppressed auto post-restart handoff for channel {}",
        channel_id.get()
    );

    super::inflight::clear_inflight_state(provider_kind, channel_id.get());
    true
}

pub(super) async fn resume_aborted_restart_turn(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: &str,
) -> bool {
    let Some((provider_kind, _)) = parse_provider_and_channel_from_tmux_name(tmux_session_name)
    else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ watcher death recovery: failed to parse provider/channel from tmux session {}",
            tmux_session_name
        );
        return false;
    };
    let Some(state) = super::inflight::load_inflight_state(&provider_kind, channel_id.get()) else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        // #3014: there is no persisted inflight turn to hand off, but the pane
        // may have died *mid-turn while Discord inputs were queued*. Without
        // inflight there is nothing to resume, so the historical behavior just
        // returned here — leaving the queued backlog orphaned until the next
        // dcserver restart (or the next user message) finally triggered a
        // kickoff. That produced multi-minute "stuck queue" stalls (71-minute
        // case). Reuse the proven deferred idle-queue kickoff so the backlog
        // drains on its own: the kickoff gate passes for a dead session
        // (no live pane ⇒ not "busy", no active turn ⇒ kickable), and the
        // dispatch path spawns a fresh session for the queued item.
        let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        let has_queued_backlog = !snapshot.intervention_queue.is_empty();
        // codex review P2: only auto-drain when the mailbox is IDLE. The
        // watcher death handler runs asynchronously, so by the time it fires a
        // concurrent kickoff / user message may already have started the next
        // queued turn in a fresh session — that turn holds a live cancel_token
        // while the remaining backlog is still queued. The tmux session name is
        // derived from the channel and thus shared across the dead and the
        // fresh session, so it cannot distinguish the stale dead turn from a
        // live one. Acting on backlog-presence alone would let a channel-scoped
        // finish cancel that unrelated live turn and corrupt global_active.
        // When an active turn is present we therefore leave the mailbox
        // untouched: a live turn drains the backlog when it completes, and a
        // stale dead-turn slot is reconciled by the watchdog / placeholder
        // sweeper paths (which then trigger their own kickoff). Only the
        // genuinely-idle case is the orphaned backlog this path must rescue.
        let mailbox_idle = snapshot.cancel_token.is_none()
            && snapshot.active_request_owner.is_none()
            && snapshot.active_user_message_id.is_none();
        if has_queued_backlog && mailbox_idle {
            tracing::info!(
                "  [{ts}] ↻ watcher death recovery: idle mailbox for channel {} (provider {}) with queued backlog — scheduling idle-queue kickoff (#3014)",
                channel_id.get(),
                provider_kind.as_str()
            );
            super::queue_io::schedule_deferred_idle_queue_kickoff(
                shared.clone(),
                provider_kind,
                channel_id,
                "watcher_death_backlog_recovery",
            );
        } else if has_queued_backlog {
            tracing::info!(
                "  [{ts}] ⚠ watcher death recovery: no inflight for channel {} (provider {}); queued backlog deferred — mailbox has an active turn (live drain or reconciler will handle)",
                channel_id.get(),
                provider_kind.as_str()
            );
        } else {
            tracing::info!(
                "  [{ts}] ⚠ watcher death recovery: no inflight state for channel {} (provider {})",
                channel_id.get(),
                provider_kind.as_str()
            );
        }
        return false;
    };

    let scope = resolve_restart_handoff_scope(&state, tmux_session_name, output_path);
    if matches!(scope, RestartHandoffScope::ProviderChannelScopedFallback) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: inflight metadata mismatch for channel {} (state tmux: {:?}, watcher tmux: {}, state output: {:?}, watcher output: {}) — proceeding with provider/channel scoped handoff",
            channel_id.get(),
            state.tmux_session_name.as_deref(),
            tmux_session_name,
            state.output_path.as_deref(),
            output_path
        );
    }

    let extracted_full = super::recovery::extract_response_from_output_pub(output_path, 0);
    let best_response = if matches!(scope, RestartHandoffScope::ProviderChannelScopedFallback) {
        state.full_response.clone()
    } else if !extracted_full.trim().is_empty() {
        extracted_full
    } else {
        state.full_response.clone()
    };
    start_restart_handoff_from_state(
        channel_id,
        http,
        shared,
        &provider_kind,
        state,
        &best_response,
    )
    .await
}

#[cfg(test)]
mod notice_target_tests {
    use super::{RestartHandoffNoticeTarget, restart_handoff_notice_target};
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn sample_inflight_state() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            1_479_671_301_387_059_200,
            Some("adk-cdx".to_string()),
            1,
            10,
            11,
            "restart me".to_string(),
            Some("session-123".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/adk-cdx.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn restart_handoff_notice_targets_existing_message() {
        let state = sample_inflight_state();

        let target = restart_handoff_notice_target(&state);

        assert_eq!(target, RestartHandoffNoticeTarget::Edit(11));
    }

    #[test]
    fn restart_handoff_notice_skips_rebind_origin() {
        let mut state = sample_inflight_state();
        state.rebind_origin = true;
        state.current_msg_id = 0;

        let target = restart_handoff_notice_target(&state);

        assert_eq!(target, RestartHandoffNoticeTarget::SkipRebindOrigin);
    }

    #[test]
    fn restart_handoff_notice_skips_missing_current_message() {
        let mut state = sample_inflight_state();
        state.current_msg_id = 0;

        let target = restart_handoff_notice_target(&state);

        assert_eq!(target, RestartHandoffNoticeTarget::MissingCurrentMessage);
    }

    #[test]
    fn restart_handoff_takeover_forgets_registered_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_202);
        let shared = super::super::make_shared_data_for_tests();
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_302),
            &ProviderKind::Codex,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        assert!(super::forget_completion_footer_for_restart_handoff(
            channel_id,
            MessageId::new(3_089_302),
        ));

        assert_eq!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    #[test]
    fn restart_handoff_keeps_different_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_212);
        let shared = super::super::make_shared_data_for_tests();
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_312),
            &ProviderKind::Codex,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        assert!(!super::forget_completion_footer_for_restart_handoff(
            channel_id,
            MessageId::new(3_089_313),
        ));

        assert!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            )
            .is_some()
        );
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
    }
}
