use super::*;
use crate::services::discord::http::{edit_channel_message, send_channel_message};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

mod provider_output_guard;
use provider_output_guard::{guard_rollover, guard_streaming_frame};
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamingStatusTickOutcome {
    ContinueStreamingLoop,
    Fallthrough,
}

pub(super) struct StreamingStatusTickContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) output_path: &'a String,
    pub(super) turn_delivered: &'a Arc<AtomicBool>,
}

pub(super) struct StreamingStatusTickTurn<'a> {
    pub(super) data_start_offset: u64,
    pub(super) current_offset: u64,
    pub(super) full_response: &'a String,
    pub(super) tool_state: &'a WatcherToolState,
    pub(super) task_notification_kind:
        Option<crate::services::agent_protocol::TaskNotificationKind>,
    pub(super) status_panel_started_at: i64,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) restored_injected_prompt_message_id: Option<u64>,
}

pub(super) struct StreamingRenderState<'a> {
    pub(super) last_status_update: &'a mut tokio::time::Instant,
    pub(super) spin_idx: &'a mut usize,
    pub(super) placeholder_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) last_edit_text: &'a mut String,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) watcher_streaming_rollover_frozen_msg_ids: &'a mut Vec<serenity::MessageId>,
}

pub(super) struct StatusPanelState<'a> {
    pub(super) status_panel_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) last_status_panel_text: &'a mut String,
}

pub(super) struct StreamingSuppressState<'a> {
    pub(super) turn_is_external_input_for_session: &'a mut bool,
    pub(super) turn_identity_for_panel:
        &'a mut Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(super) streaming_suppressed_by_recent_stop: &'a mut bool,
    pub(super) streaming_suppressed_by_missing_inflight: &'a mut bool,
    pub(super) active_stream_inflight_reacquire_logged: &'a mut bool,
}

pub(super) struct PanelGenerationState<'a> {
    pub(super) this_turn_status_panel_generation: &'a mut u64,
}

pub(super) async fn update_streaming_status_tick(
    ctx: &StreamingStatusTickContext<'_>,
    turn: StreamingStatusTickTurn<'_>,
    render: &mut StreamingRenderState<'_>,
    panel: &mut StatusPanelState<'_>,
    suppress: &mut StreamingSuppressState<'_>,
    generation: &mut PanelGenerationState<'_>,
) -> StreamingStatusTickOutcome {
    let http = ctx.http;
    let shared = ctx.shared;
    let channel_id = ctx.channel_id;
    let watcher_provider = ctx.watcher_provider;
    let tmux_session_name = ctx.tmux_session_name;
    let output_path = ctx.output_path;
    let turn_delivered = ctx.turn_delivered;
    let data_start_offset = turn.data_start_offset;
    let current_offset = turn.current_offset;
    let full_response = turn.full_response;
    let tool_state = turn.tool_state;
    let task_notification_kind = turn.task_notification_kind;
    let status_panel_started_at = turn.status_panel_started_at;
    let single_message_panel_footer_mode = turn.single_message_panel_footer_mode;
    let restored_injected_prompt_message_id = turn.restored_injected_prompt_message_id;

    // Update Discord placeholder at configurable interval
    if render.last_status_update.elapsed() >= crate::services::discord::status_update_interval() {
        let mut last_status_update = *render.last_status_update;
        let mut spin_idx = *render.spin_idx;
        let mut placeholder_msg_id = *render.placeholder_msg_id;
        let mut placeholder_from_restored_inflight = *render.placeholder_from_restored_inflight;
        let mut last_edit_text = (*render.last_edit_text).clone();
        let mut response_sent_offset = *render.response_sent_offset;
        let mut watcher_streaming_rollover_frozen_msg_ids =
            (*render.watcher_streaming_rollover_frozen_msg_ids).clone();
        let mut status_panel_msg_id = *panel.status_panel_msg_id;
        let mut last_status_panel_text = (*panel.last_status_panel_text).clone();
        let mut turn_is_external_input_for_session = *suppress.turn_is_external_input_for_session;
        let mut turn_identity_for_panel = (*suppress.turn_identity_for_panel).clone();
        let mut streaming_suppressed_by_recent_stop = *suppress.streaming_suppressed_by_recent_stop;
        let mut streaming_suppressed_by_missing_inflight =
            *suppress.streaming_suppressed_by_missing_inflight;
        let mut active_stream_inflight_reacquire_logged =
            *suppress.active_stream_inflight_reacquire_logged;
        let mut this_turn_status_panel_generation = *generation.this_turn_status_panel_generation;

        macro_rules! commit_streaming_status_tick_state {
            () => {{
                *render.last_status_update = last_status_update;
                *render.spin_idx = spin_idx;
                *render.placeholder_msg_id = placeholder_msg_id;
                *render.placeholder_from_restored_inflight = placeholder_from_restored_inflight;
                *render.last_edit_text = last_edit_text;
                *render.response_sent_offset = response_sent_offset;
                *render.watcher_streaming_rollover_frozen_msg_ids =
                    watcher_streaming_rollover_frozen_msg_ids;
                *panel.status_panel_msg_id = status_panel_msg_id;
                *panel.last_status_panel_text = last_status_panel_text;
                *suppress.turn_is_external_input_for_session = turn_is_external_input_for_session;
                *suppress.turn_identity_for_panel = turn_identity_for_panel;
                *suppress.streaming_suppressed_by_recent_stop = streaming_suppressed_by_recent_stop;
                *suppress.streaming_suppressed_by_missing_inflight =
                    streaming_suppressed_by_missing_inflight;
                *suppress.active_stream_inflight_reacquire_logged =
                    active_stream_inflight_reacquire_logged;
                *generation.this_turn_status_panel_generation = this_turn_status_panel_generation;
            }};
        }

        last_status_update = tokio::time::Instant::now();
        let indicator = SPINNER[spin_idx % SPINNER.len()];
        spin_idx += 1;

        let tick_placeholder_reclaim = watcher_should_reclaim_orphan_turn_placeholder(
            turn_is_external_input_for_session,
            placeholder_msg_id,
            !full_response.trim().is_empty(),
            &last_edit_text,
        );
        if turn_is_external_input_for_session
            && (status_panel_msg_id.is_some() || tick_placeholder_reclaim)
            && watcher_external_input_turn_abandoned(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                data_start_offset,
                turn_identity_for_panel.as_ref(),
            )
        {
            cleanup_orphan_external_input_status_panel(
                &http,
                &shared,
                channel_id,
                &mut status_panel_msg_id,
                &watcher_provider,
                &tmux_session_name,
                turn_is_external_input_for_session,
            )
            .await;
            if tick_placeholder_reclaim {
                reclaim_orphan_external_input_placeholder(
                    &http,
                    &shared,
                    channel_id,
                    &mut placeholder_msg_id,
                    &mut placeholder_from_restored_inflight,
                    &mut last_edit_text,
                    &watcher_provider,
                    &tmux_session_name,
                )
                .await;
            }
        }

        // Headless silent trigger (metadata.silent=true): skip both
        // status-panel and streaming-chunk edits to keep the channel
        // at zero bytes for the assistant turn.
        let streaming_silent_turn = crate::services::discord::inflight::load_inflight_state(
            &watcher_provider,
            channel_id.get(),
        )
        .map(|state| state.silent_turn)
        .unwrap_or(false);
        if streaming_silent_turn {
            commit_streaming_status_tick_state!();
            return StreamingStatusTickOutcome::ContinueStreamingLoop;
        }

        if shared.ui.status_panel_v2_enabled
            && (single_message_panel_footer_mode || status_panel_msg_id.is_some())
        {
            // #3055: re-derive this turn's session lifecycle panel
            // line on the throttled status tick, matching bridge
            // behavior and avoiding stale per-channel snapshots.
            refresh_watcher_session_panel_from_lifecycle(
                &shared,
                channel_id,
                turn_identity_for_panel
                    .as_ref()
                    .map(|identity| identity.user_msg_id)
                    .unwrap_or(0),
                &tmux_session_name,
                &watcher_provider, // #3983 item4: one-shot session banner render
            )
            .await;
        }
        if watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled)
            && let Some(status_msg_id) = status_panel_msg_id
        {
            let panel_text = shared.ui.placeholder_live_events.render_status_panel(
                channel_id,
                &watcher_provider,
                status_panel_started_at,
            );
            if panel_text != last_status_panel_text {
                rate_limit_wait(&shared, channel_id).await;
                match crate::services::discord::http::edit_channel_message(
                    &http,
                    channel_id,
                    status_msg_id,
                    &panel_text,
                )
                .await
                {
                    Ok(_) => {
                        last_status_panel_text = panel_text;
                    }
                    Err(error) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ tmux status-panel-v2 edit failed for msg {} in channel {}: {}",
                            status_msg_id.get(),
                            channel_id.get(),
                            error
                        );
                    }
                }
            }
        }

        let has_assistant_response_for_streaming = !full_response.trim().is_empty();
        if watcher_should_suppress_streaming_after_bridge_delivery(
            turn_delivered.load(Ordering::Relaxed),
            has_assistant_response_for_streaming,
        ) {
            if let Some(msg_id) = placeholder_msg_id {
                if watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight)
                {
                    let outcome = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_streaming_bridge_delivered_cleanup",
                    )
                    .await;
                    if outcome.is_committed() {
                        placeholder_msg_id = None;
                        placeholder_from_restored_inflight = false;
                        last_edit_text.clear();
                    }
                } else {
                    // This placeholder id came from the active inflight row.
                    // In status-panel-v2 bridge-owned delivery, the bridge
                    // edits that exact message into the final response. The
                    // watcher must drop local ownership without deleting it.
                    placeholder_msg_id = None;
                    placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                }
            }
            if !streaming_suppressed_by_recent_stop {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 🛑 watcher: suppressed streaming placeholder output for channel {} after bridge delivered turn (tmux={}, range {}..{})",
                    channel_id.get(),
                    tmux_session_name,
                    data_start_offset,
                    current_offset
                );
                streaming_suppressed_by_recent_stop = true;
            }
            commit_streaming_status_tick_state!();
            return StreamingStatusTickOutcome::ContinueStreamingLoop;
        }
        let recent_stop_for_streaming = if has_assistant_response_for_streaming {
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset)
        } else {
            None
        };
        let inflight_missing_for_streaming =
            crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            )
            .is_none();
        // #3107: lazy pane-capture probe — only when inflight is
        // missing (expensive signal stays off the hot path).
        let pane_actively_streaming_for_streaming =
            inflight_missing_for_streaming && watcher_pane_actively_streaming(&tmux_session_name);
        if inflight_missing_for_streaming && pane_actively_streaming_for_streaming {
            // #3107 self-heal: pane live but inflight cleared mid-turn —
            // re-establish a watcher-owned inflight (idempotent + 1-shot log).
            let reacquired = reacquire_watcher_inflight_for_active_stream(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                data_start_offset,
                status_panel_msg_id,
                placeholder_msg_id,
                // #3107 (P2#3, F3): thread the #3099 hourglass anchor
                // (captured before `restored_turn` was `.take()`n) so a
                // mid-stream inflight loss keeps the `⏳ → ✅` cleanup anchor.
                restored_injected_prompt_message_id,
            );
            if reacquired && !active_stream_inflight_reacquire_logged {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 🩹 watcher: re-acquired watcher-owned inflight for actively-streaming pane that lost its inflight (channel {}, tmux={}, range {}..{})",
                    channel_id.get(),
                    tmux_session_name,
                    data_start_offset,
                    current_offset
                );
                active_stream_inflight_reacquire_logged = true;
            }
        }
        if should_skip_streaming_placeholder_without_inflight(
            inflight_missing_for_streaming,
            pane_actively_streaming_for_streaming,
        ) {
            if !streaming_suppressed_by_missing_inflight {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 🛑 watcher: suppressed streaming placeholder edit for channel {} because inflight state is missing (tmux={}, range {}..{})",
                    channel_id.get(),
                    tmux_session_name,
                    data_start_offset,
                    current_offset
                );
                streaming_suppressed_by_missing_inflight = true;
            }
            commit_streaming_status_tick_state!();
            return StreamingStatusTickOutcome::ContinueStreamingLoop;
        }
        if should_suppress_streaming_placeholder_after_recent_stop(
            has_assistant_response_for_streaming,
            inflight_missing_for_streaming,
            recent_stop_for_streaming.is_some(),
        ) {
            if let Some(msg_id) = placeholder_msg_id {
                if watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight)
                {
                    let outcome = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_streaming_recent_stop_cleanup",
                    )
                    .await;
                    if outcome.is_committed() {
                        placeholder_msg_id = None;
                        placeholder_from_restored_inflight = false;
                        last_edit_text.clear();
                    }
                } else {
                    placeholder_msg_id = None;
                    placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                }
            }
            if !streaming_suppressed_by_recent_stop {
                if let Some(stop) = recent_stop_for_streaming {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] 🛑 watcher: suppressed streaming placeholder output for channel {} after recent turn stop ({}, tmux={}, range {}..{})",
                        channel_id.get(),
                        stop.reason,
                        tmux_session_name,
                        data_start_offset,
                        current_offset
                    );
                }
                streaming_suppressed_by_recent_stop = true;
            }
            // #3003: the stopped-turn panel reclaim now runs at the
            // single chokepoint at the top of this interval block, before
            // this recent-stop `continue` and the inflight-missing guard
            // can bypass it.
            commit_streaming_status_tick_state!();
            return StreamingStatusTickOutcome::ContinueStreamingLoop;
        }

        // #3003: TUI-direct turns lack a prior Discord message to
        // re-designate, so flag-off creates a dedicated v2 panel here
        // after suppression guards and only once visible work exists.
        let has_visible_streaming_work = !full_response
            .get(response_sent_offset..)
            .unwrap_or("")
            .trim()
            .is_empty()
            || watcher_should_render_status_only_placeholder(
                placeholder_msg_id.is_some(),
                tool_state.current_tool_line.as_deref(),
                task_notification_kind,
            );
        if watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled)
            && status_panel_msg_id.is_none()
            && has_visible_streaming_work
            // #3805 P2 (PR-C): under the two-message flag, defer panel
            // creation until the answer placeholder exists so the panel
            // is created BELOW it (answer-first). OFF: always true →
            // the legacy creation block runs byte-identical.
            && watcher_two_message_panel_creation_gated_by_answer(
                shared.ui.two_message_panel_enabled,
                placeholder_msg_id.is_some(),
            )
        {
            let inflight_for_panel = crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            );
            let persisted_panel_msg_id = watcher_persisted_status_panel_msg_id(
                inflight_for_panel.as_ref(),
                &tmux_session_name,
            );
            // status-panel-v2: panel eligibility (external-input OR
            // synthetic monitor/self-paced-loop) drives panel
            // creation here; the lease/⏳-anchor sites keep the
            // narrower external-input predicate.
            let panel_eligible_turn = watcher_inflight_is_panel_eligible_for_session(
                inflight_for_panel.as_ref(),
                &tmux_session_name,
            );
            if panel_eligible_turn {
                turn_is_external_input_for_session = true;
                // #3003 P2: if startup predated inflight creation,
                // capture identity now so abandon detects replacement.
                if turn_identity_for_panel.is_none() {
                    turn_identity_for_panel = inflight_for_panel
                        .as_ref()
                        .filter(|state| {
                            state.tmux_session_name.as_deref() == Some(tmux_session_name.as_str())
                        })
                        .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
                }
                // #3003 P2: no late live-event clear here; the fresh-frame
                // reset above preserved this turn's initial flush.
            }
            if let Some(persisted) = persisted_panel_msg_id {
                // Restart-safe adoption: the panel already exists and was
                // persisted on this turn's inflight; reuse it instead of
                // publishing a duplicate (#3003 codex P2). Synthetic headless
                // ids are already filtered by the persisted helper.
                status_panel_msg_id = Some(persisted);
            } else if watcher_should_create_separate_status_panel(
                single_message_panel_footer_mode,
                shared.ui.status_panel_v2_enabled,
                status_panel_msg_id.is_some(),
                panel_eligible_turn,
            ) && !watcher_external_input_turn_abandoned(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                data_start_offset,
                turn_identity_for_panel.as_ref(),
            ) {
                // #3003 (codex P2 r18): do NOT create a panel for an already
                // stopped/abandoned turn. A stop tombstone can be recorded
                // before the inflight row is removed; without this guard the
                // interval-top reclaim would delete the panel and this branch
                // would immediately recreate one for the same stopped turn.
                // Snapshot the turn identity *before* the await so a
                // stop/cancel/next-turn during send cannot persist stale
                // state onto a different turn (codex P2 r4).
                let pre_send_identity = inflight_for_panel
                    .as_ref()
                    .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
                let panel_seed =
                    crate::services::discord::formatting::build_processing_status_block(indicator);
                rate_limit_wait(&shared, channel_id).await;
                match crate::services::discord::http::send_channel_message(
                    &http,
                    channel_id,
                    &panel_seed,
                )
                .await
                {
                    Ok(panel_msg) => {
                        preregister_watcher_two_message_panel_orphan(
                            shared.ui.two_message_panel_enabled,
                            shared.as_ref(),
                            &watcher_provider,
                            channel_id,
                            panel_msg.id,
                        );
                        let fresh_inflight =
                            crate::services::discord::inflight::load_inflight_state(
                                &watcher_provider,
                                channel_id.get(),
                            );
                        let identity_matches = matches!(
                            (&pre_send_identity, &fresh_inflight),
                            (Some(pre), Some(fresh))
                                if pre == &crate::services::discord::inflight::InflightTurnIdentity::from_state(fresh)
                        );
                        // #3003 (codex P2 r18): another overlapping watcher may
                        // have already published+persisted a panel for this turn
                        // during our send await. If the fresh inflight already
                        // carries a real status_message_id, our send is a
                        // duplicate — reclaim it instead of overwriting the
                        // canonical id (which would orphan the other panel).
                        let fresh_panel_already_set = fresh_inflight.as_ref().is_some_and(|fresh| {
                            crate::services::discord::turn_bridge::normalize_status_panel_message_id(
                                fresh.status_message_id.map(serenity::MessageId::new),
                            )
                            .is_some()
                        });
                        if identity_matches && !fresh_panel_already_set && fresh_inflight.is_some()
                        {
                            // #3077: bind through the typed op so the
                            // identity guard + "don't clobber an already-set
                            // panel" check are re-validated atomically under
                            // the inflight flock — closing the window where an
                            // overlapping watcher rebinds between our snapshot
                            // load and this write (#3003).
                            let bind_outcome =
                                crate::services::discord::inflight::bind_status_panel(
                                    &watcher_provider,
                                    channel_id.get(),
                                    panel_msg.id.get(),
                                    &crate::services::discord::inflight::StatusPanelBindGuard {
                                        require_identity: pre_send_identity.clone(),
                                        skip_if_panel_already_set: true,
                                        // #3805 P2: when the two-message
                                        // flag is ON, open this turn's
                                        // panel epoch from the on-disk
                                        // row inside the bind flock.
                                        // OFF leaves the field untouched.
                                        bump_status_panel_generation: shared
                                            .ui
                                            .two_message_panel_enabled,
                                        ..Default::default()
                                    },
                                );
                            // #3077 (codex P1): the pre-send snapshot narrows but does
                            // NOT close the race (an overlapping watcher can rebind
                            // between our load and this atomic bind). The bind is the
                            // single source of truth for whether THIS panel is recorded,
                            // so the adopted handle MUST come from its return — adopting
                            // `panel_msg.id` unconditionally leaks a sent-but-unrecorded panel.
                            let decision = resolve_tui_status_panel_bind_decision(bind_outcome);
                            if decision.delete_sent_panel {
                                // The inflight row did NOT record our panel:
                                //  - SkippedPanelAlreadySet → the row already carries a
                                //    DIFFERENT (real) panel id; ours is a duplicate.
                                //  - GuardMismatch / Missing / IoError → the bind never
                                //    happened (the row changed/disappeared or a guard
                                //    failed); we must not claim ownership of a panel the
                                //    row doesn't know about.
                                // Delete the just-sent duplicate so it never leaks. This
                                // reuses the same delete path the "inflight changed
                                // during send" branch below uses
                                // (delete_nonterminal_placeholder → tmux.rs:803). It
                                // never double-deletes a legitimately-bound panel: we
                                // only reach here when our bind did NOT record
                                // `panel_msg.id`, so the row's owned panel (if any) is a
                                // *different* id we never delete.
                                let discard_outcome = delete_nonterminal_placeholder(
                                    &http,
                                    channel_id,
                                    &shared,
                                    &watcher_provider,
                                    &tmux_session_name,
                                    panel_msg.id,
                                    "watcher_external_input_status_panel_bind_unowned",
                                )
                                .await;
                                if !discard_outcome.is_committed()
                                    && !discard_outcome.is_permanent_failure()
                                {
                                    // Transient delete failure: the duplicate panel
                                    // still exists and this path does not persist it to
                                    // inflight, so record it in the durable store for
                                    // the sweeper drain to reclaim independent of turn
                                    // lifecycle (#3003 codex P2 r14 pattern).
                                    enqueue_watcher_status_panel_orphan(
                                        shared.as_ref(),
                                        &watcher_provider,
                                        channel_id,
                                        panel_msg.id,
                                    );
                                } else {
                                    remove_watcher_two_message_panel_orphan_registration(
                                        shared.ui.two_message_panel_enabled,
                                        shared.as_ref(),
                                        &watcher_provider,
                                        channel_id,
                                        panel_msg.id,
                                    );
                                }
                                // Resolve the handle from the row's CURRENT owned id as
                                // observed by the bind (`decision.owned_panel_id`), never
                                // the just-sent duplicate nor the (possibly stale) pre-bind
                                // `fresh_inflight` snapshot (#3077 codex P2 #2). It is
                                // `None` for GuardMismatch/Missing/IoError (no panel we may
                                // claim → handle unset). Adopt only for the SAME turn we
                                // sent for; a replacement turn's panel belongs to it.
                                let resolved_handle = if identity_matches {
                                    decision.owned_panel_id.map(serenity::MessageId::new)
                                } else {
                                    None
                                };
                                status_panel_msg_id = resolved_handle;
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                // Single bounded incident log per unowned-bind event.
                                tracing::warn!(
                                    "  [{ts}] ⚠ watcher: status-panel-v2 bind did not record our panel for TUI-direct turn in channel {} (outcome={:?}, panel_msg={}, delete_committed={}, adopted_handle={:?}); discarded duplicate instead of leaking it",
                                    channel_id.get(),
                                    bind_outcome,
                                    panel_msg.id.get(),
                                    discard_outcome.is_committed(),
                                    resolved_handle.map(serenity::MessageId::get)
                                );
                            } else {
                                // Bound / AlreadyBound: the row now owns this exact id.
                                debug_assert!(decision.adopt_sent_panel);
                                remove_watcher_two_message_panel_orphan_registration(
                                    shared.ui.two_message_panel_enabled,
                                    shared.as_ref(),
                                    &watcher_provider,
                                    channel_id,
                                    panel_msg.id,
                                );
                                status_panel_msg_id = Some(panel_msg.id);
                                // #3805 P2 (PR-C): a FRESH Bound opened this
                                // turn's panel epoch (the generation the
                                // guard just persisted); mirror it into the
                                // local so the completion guard proves the
                                // SAME epoch. AlreadyBound re-binds do NOT
                                // re-open it (the local already carries the
                                // on-disk seed). None/OFF → local untouched.
                                if shared.ui.two_message_panel_enabled
                                    && let Some(opened) =
                                        bind_outcome.bound_status_panel_generation()
                                {
                                    this_turn_status_panel_generation = opened;
                                }
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 🪧 watcher: created status-panel-v2 for TUI-direct turn (channel {}, tmux={}, panel_msg={})",
                                    channel_id.get(),
                                    tmux_session_name,
                                    panel_msg.id.get()
                                );
                            }
                        } else {
                            // The turn vanished/changed during the send await, or an
                            // overlapping watcher already owns the panel; ours is a
                            // duplicate/orphan — reclaim it instead of persisting stale
                            // state (the next interval adopts the canonical panel).
                            let discard_outcome = delete_nonterminal_placeholder(
                                &http,
                                channel_id,
                                &shared,
                                &watcher_provider,
                                &tmux_session_name,
                                panel_msg.id,
                                "watcher_external_input_status_panel_turn_changed",
                            )
                            .await;
                            if !discard_outcome.is_committed()
                                && !discard_outcome.is_permanent_failure()
                            {
                                // #3003 (codex P2 r14): transient delete failure but the
                                // duplicate exists and this path never persists it —
                                // record it for the sweeper drain to reclaim.
                                enqueue_watcher_status_panel_orphan(
                                    shared.as_ref(),
                                    &watcher_provider,
                                    channel_id,
                                    panel_msg.id,
                                );
                                // #3003 (codex P2 r19/r22): adopt the CANONICAL persisted
                                // panel ONLY for a same-turn overlapping-watcher duplicate
                                // (`identity_matches`), so edits/completion hit the real
                                // panel. For a *replacement* turn the persisted id is the
                                // new turn's; adopting it would let the old frame's abandon
                                // cleanup delete it — keep the just-sent duplicate locally.
                                if fresh_panel_already_set && identity_matches {
                                    status_panel_msg_id = watcher_persisted_status_panel_msg_id(
                                        fresh_inflight.as_ref(),
                                        &tmux_session_name,
                                    );
                                } else {
                                    status_panel_msg_id = Some(panel_msg.id);
                                }
                            } else {
                                remove_watcher_two_message_panel_orphan_registration(
                                    shared.ui.two_message_panel_enabled,
                                    shared.as_ref(),
                                    &watcher_provider,
                                    channel_id,
                                    panel_msg.id,
                                );
                            }
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ watcher: discarded status-panel-v2 for TUI-direct turn in channel {} — inflight changed during send (panel_msg={}, delete_committed={})",
                                channel_id.get(),
                                panel_msg.id.get(),
                                discard_outcome.is_committed()
                            );
                        }
                    }
                    Err(error) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher: failed to create status-panel-v2 for TUI-direct turn in channel {}: {}",
                            channel_id.get(),
                            error
                        );
                    }
                }
            }
        }

        let mut watcher_did_rollover_this_interval = false;
        loop {
            let current_portion = full_response.get(response_sent_offset..).unwrap_or("");
            if current_portion.is_empty() {
                break;
            }

            let status_block = build_watcher_single_message_panel_status_block(
                &shared,
                channel_id,
                &watcher_provider,
                status_panel_started_at,
                indicator,
                tool_state.prev_tool_status.as_deref(),
                tool_state.current_tool_line.as_deref(),
                &full_response,
                status_panel_msg_id,
            );
            let Some(msg_id) = placeholder_msg_id else {
                break;
            };
            if watcher_streaming_rollover_should_skip(current_portion) {
                break;
            }
            let Some(plan) = plan_streaming_rollover(current_portion, &status_block) else {
                break;
            };
            if !guard_rollover(ctx, msg_id, current_portion, &plan.frozen_chunk).await {
                break;
            }
            rate_limit_wait(&shared, channel_id).await;
            match crate::services::discord::http::edit_channel_message(
                &http,
                channel_id,
                msg_id,
                &plan.frozen_chunk,
            )
            .await
            {
                Ok(_) => {
                    rate_limit_wait(&shared, channel_id).await;
                    match crate::services::discord::http::send_channel_message(
                        &http,
                        channel_id,
                        &status_block,
                    )
                    .await
                    {
                        Ok(message) => {
                            // #3871: `msg_id` is now a FROZEN prefix — record it for terminal full-body dedup.
                            watcher_streaming_rollover_frozen_msg_ids.push(msg_id);
                            placeholder_msg_id = Some(message.id);
                            placeholder_from_restored_inflight = false;
                            watcher_did_rollover_this_interval = true;
                            response_sent_offset += plan.split_at;
                            last_edit_text = status_block;
                            persist_watcher_stream_progress(
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                turn_identity_for_panel.as_ref(),
                                placeholder_msg_id,
                                &full_response,
                                response_sent_offset,
                                tool_state.current_tool_line.as_deref(),
                                tool_state.prev_tool_status.as_deref(),
                                task_notification_kind,
                                tool_state.any_tool_used,
                                tool_state.has_post_tool_text,
                                &watcher_streaming_rollover_frozen_msg_ids,
                            );
                        }
                        Err(error) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ tmux rollover placeholder send failed in channel {}: {}",
                                channel_id.get(),
                                error
                            );
                            rate_limit_wait(&shared, channel_id).await;
                            if crate::services::discord::http::edit_channel_message(
                                &http,
                                channel_id,
                                msg_id,
                                &plan.display_snapshot,
                            )
                            .await
                            .is_ok()
                            {
                                last_edit_text = plan.display_snapshot;
                            }
                            break;
                        }
                    }
                }
                Err(error) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ tmux rollover freeze failed for msg {} in channel {}: {}",
                        msg_id.get(),
                        channel_id.get(),
                        error
                    );
                    break;
                }
            }
        }

        // #3805 P2 (PR-D): re-anchor the stranded panel below a rollover tail; OFF-inert.
        let two_message_panel_enabled = shared.ui.two_message_panel_enabled;
        let inflight_for_reanchor =
            if two_message_panel_enabled && watcher_did_rollover_this_interval {
                crate::services::discord::inflight::load_inflight_state(
                    &watcher_provider,
                    channel_id.get(),
                )
            } else {
                None
            };
        if watcher_did_rollover_this_interval
            && watcher_two_message_should_reanchor_panel_on_rollover(
                two_message_panel_enabled,
                status_panel_msg_id.is_some(),
                inflight_for_reanchor.as_ref(),
                &tmux_session_name,
            )
        {
            let panel_text = shared.ui.placeholder_live_events.render_status_panel(
                channel_id,
                &watcher_provider,
                status_panel_started_at,
            );
            reanchor_watcher_two_message_status_panel_below_answer(
                &http,
                &shared,
                channel_id,
                &watcher_provider,
                &tmux_session_name,
                turn_identity_for_panel.clone(),
                &panel_text,
                &mut status_panel_msg_id,
                &mut this_turn_status_panel_generation,
                &mut last_status_panel_text,
            )
            .await;
        }

        let status_block = build_watcher_single_message_panel_status_block(
            &shared,
            channel_id,
            &watcher_provider,
            status_panel_started_at,
            indicator,
            tool_state.prev_tool_status.as_deref(),
            tool_state.current_tool_line.as_deref(),
            &full_response,
            status_panel_msg_id,
        );
        let current_portion = full_response.get(response_sent_offset..).unwrap_or("");
        if current_portion.trim().is_empty()
            && !watcher_should_render_status_only_placeholder(
                placeholder_msg_id.is_some(),
                tool_state.current_tool_line.as_deref(),
                task_notification_kind,
            )
        {
            commit_streaming_status_tick_state!();
            return StreamingStatusTickOutcome::ContinueStreamingLoop;
        }
        let mut display_text = build_watcher_streaming_edit_text(
            shared.ui.status_panel_v2_enabled,
            current_portion,
            &status_block,
            &watcher_provider,
        );
        if guard_streaming_frame(&watcher_provider, current_portion, &mut display_text)
            && crate::services::discord::single_message_panel::streaming_footer_text_changed(
                single_message_panel_footer_mode,
                &last_edit_text,
                &display_text,
            )
        {
            let edit_committed = match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    edit_channel_message(&http, channel_id, msg_id, &display_text)
                        .await
                        .is_ok()
                }
                None => {
                    if let Ok(msg) = send_channel_message(&http, channel_id, &display_text).await {
                        placeholder_msg_id = Some(msg.id);
                        placeholder_from_restored_inflight = false;
                        true
                    } else {
                        false
                    }
                }
            };
            if edit_committed {
                last_edit_text = display_text;
                persist_watcher_stream_progress(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    turn_identity_for_panel.as_ref(),
                    placeholder_msg_id,
                    &full_response,
                    response_sent_offset,
                    tool_state.current_tool_line.as_deref(),
                    tool_state.prev_tool_status.as_deref(),
                    task_notification_kind,
                    tool_state.any_tool_used,
                    tool_state.has_post_tool_text,
                    &watcher_streaming_rollover_frozen_msg_ids,
                );
            }
        }
        commit_streaming_status_tick_state!();
    }

    StreamingStatusTickOutcome::Fallthrough
}
