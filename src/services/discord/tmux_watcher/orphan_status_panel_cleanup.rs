//! #3479 item-2: orphan status-panel cleanup cluster for the tmux watcher.
//!
//! Verbatim extraction (zero logic change) of the watcher-direct status-panel
//! cleanup/completion/refresh helpers from `tmux_watcher.rs`. Items are
//! `pub(super)` here and re-imported by the parent so the watcher loop's call
//! sites — and the sibling `single_message_footer.rs` completion call — stay
//! byte-identical.

use super::*;

/// #3003 (codex P2 r3): delete a watcher-created TUI-direct status panel that
/// will never reach terminal completion — the turn was stopped or returned to
/// idle with no committed response, so `complete_watcher_status_panel_v2` never
/// runs and the panel would stay stuck at "계속 처리 중".
///
/// Ownership is decided by `turn_is_external_input` — a flag cached *while the
/// inflight row was still present* — rather than reloading inflight here (codex
/// P2 r4): a stopped/cancelled TUI-direct turn has already cleared its inflight,
/// so a fresh read would miss the very panel this reclaim was added for. A
/// bridge-owned panel never sets the flag, so it is never touched.
///
/// Deletion routes through `delete_nonterminal_placeholder` so the in-memory and
/// persisted ids are dropped only on a committed delete (codex P3 r4) — a
/// transient Discord error leaves the ids intact for a later retry. The
/// persisted `status_message_id` is cleared only when it still points at this
/// exact panel, so a newer turn's panel is never clobbered.
///
/// Returns `false` only when a delete was attempted and did not commit, so the
/// caller can defer finalization/inflight-clearing and let a later iteration
/// retry (codex P2 r5); `true` means nothing to clean or the delete committed.
pub(super) async fn cleanup_orphan_external_input_status_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    status_panel_msg_id: &mut Option<serenity::MessageId>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    turn_is_external_input: bool,
) -> bool {
    if !watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled) {
        *status_panel_msg_id = None;
        return true;
    }
    if !turn_is_external_input {
        return true;
    }
    let Some(panel_msg_id) = *status_panel_msg_id else {
        return true;
    };
    let outcome = delete_nonterminal_placeholder(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        panel_msg_id,
        "watcher_orphan_external_input_status_panel_cleanup",
    )
    .await;
    if !outcome.is_committed() && !outcome.is_permanent_failure() {
        // #3003 (codex P2 r10/r11/r13): the inline delete failed transiently. The
        // local id is kept for an in-turn retry, but a stopped/cancelled turn may
        // clear its inflight before any retry runs, leaving no per-turn handle.
        // Record the panel in the durable store so the sweeper drain reclaims it
        // independent of inflight lifecycle.
        enqueue_watcher_status_panel_orphan(shared.as_ref(), provider, channel_id, panel_msg_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: orphan status-panel-v2 delete did not commit for channel {} panel_msg {}; kept local id + enqueued durable retry",
            channel_id.get(),
            panel_msg_id.get()
        );
        return false;
    }
    // Committed (succeeded / already-gone) OR a permanent failure (403/410): neither
    // is retried, so treat a permanent failure as terminal and clear the handle
    // (codex P2 r16) rather than wedge finalization forever. Drop the durable record
    // too, since the drain would also give up on the same permanent error.
    if !outcome.is_committed() {
        crate::services::discord::status_panel_orphan_store::remove(
            provider,
            &shared.token_hash,
            channel_id.get(),
            panel_msg_id.get(),
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: orphan status-panel-v2 delete permanently failed for channel {} panel_msg {}; giving up (treated as committed)",
            channel_id.get(),
            panel_msg_id.get()
        );
    }
    *status_panel_msg_id = None;
    // #3077: compare-and-clear under the inflight flock so a newer turn that
    // rebound this panel between our load and our clear is never wiped. The
    // tmux-session guard preserves the prior precondition (only clear our own
    // TUI-direct turn's row).
    let _ = crate::services::discord::inflight::clear_status_panel_if_current(
        provider,
        channel_id.get(),
        panel_msg_id.get(),
        &crate::services::discord::inflight::StatusPanelClearGuard {
            require_tmux_session_name: Some(tmux_session_name.to_string()),
            ..Default::default()
        },
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 watcher: cleaned orphan status-panel-v2 for TUI-direct turn (channel {}, tmux={}, panel_msg={})",
        channel_id.get(),
        tmux_session_name,
        panel_msg_id.get()
    );
    true
}

/// Returns whether the completion edit/send committed. `false` means the final
/// panel edit hit a transient Discord error and the panel is still showing the
/// processing state — the caller must preserve a retry handle (enqueue the panel
/// for the durable drain) before clearing the inflight, or the panel orphans
/// (codex P2 r20).
#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_watcher_status_panel_v2(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    status_panel_msg_id: Option<serenity::MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    background_agent_pending: bool,
    expected_user_msg_id: Option<u64>,
) -> bool {
    // #2427 D wire (Codex round 2 HIGH-1): explicit-signal inflight cleanup
    // is intentionally NOT emitted from the watcher path. The watcher is
    // not turn-scoped, so any user_msg_id read here would be the *current*
    // on-disk value (possibly the next turn's). The committed-output path
    // at L~2996 already performs the unconditional `clear_inflight_state`
    // for the turn the watcher actually finished. Recovery-driven
    // TurnCompleted still emits the guarded signal (see recovery_engine.rs)
    // because its state snapshot is pinned at recovery entry.
    if !watcher_should_complete_separate_status_panel(shared.ui.status_panel_v2_enabled) {
        return true;
    }
    crate::services::discord::turn_bridge::complete_status_panel_v2_with_http(
        shared,
        http,
        channel_id,
        status_panel_msg_id,
        provider,
        started_at_unix,
        last_status_panel_text,
        background,
        background_agent_pending,
        "tmux_watcher",
        (expected_user_msg_id, None),
    )
    .await
}

/// #3055 — the per-channel session lifecycle panel snapshot (`🆕 새 세션 시작`,
/// `기존 세션 복원`, …) is set by the bridge's
/// `refresh_session_panel_line_from_lifecycle` and is keyed only by channel,
/// not by turn. The bridge re-derives it from the *current* turn's lifecycle
/// row on every status tick (and clears it when the current turn has no
/// session lifecycle event). The watcher-direct render/completion paths never
/// performed that refresh, so a watcher-direct TUI turn would reuse a stale
/// snapshot left behind by a prior turn's `session_fresh`/`session_resumed`
/// event (e.g. a `(최근 대화 N개…)` recovery line from an earlier
/// recovery/new-session turn).
///
/// Mirror the bridge behaviour for the watcher: load the latest session
/// lifecycle event for *this* watcher turn and set the panel from it, or clear
/// the panel when the current turn has no such event. Watcher-direct TUI turns
/// carry `user_msg_id == 0` (no anchored Discord message) so they key onto the
/// invariant-guarded `discord:<channel>:0` turn id, which by construction has
/// no session lifecycle row — the panel is therefore cleared and the stale
/// line is never reused.
pub(super) async fn refresh_watcher_session_panel_from_lifecycle(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    user_msg_id: u64,
    tmux_session_name: &str,
) {
    if !shared.ui.status_panel_v2_enabled {
        return;
    }
    let Some(pg_pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let turn_id = format!("discord:{}:{}", channel_id.get(), user_msg_id);
    let session_instance_key = session_panel_instance_key(tmux_session_name);
    let channel_id_text = channel_id.get().to_string();
    match crate::services::observability::turn_lifecycle::load_latest_session_lifecycle_event(
        pg_pool,
        &channel_id_text,
        &turn_id,
    )
    .await
    {
        Ok(Some(event)) => {
            shared
                .ui
                .placeholder_live_events
                .set_session_panel_lifecycle_event(
                    channel_id,
                    session_instance_key.as_deref(),
                    &event.kind,
                    &event.details_json,
                );
        }
        Ok(None) => {
            shared
                .ui
                .placeholder_live_events
                .clear_session_panel(channel_id);
        }
        Err(error) => {
            tracing::debug!(
                "[tmux_watcher] failed to load session lifecycle line for turn {} in channel {}: {}",
                turn_id,
                channel_id,
                error
            );
        }
    }
}
