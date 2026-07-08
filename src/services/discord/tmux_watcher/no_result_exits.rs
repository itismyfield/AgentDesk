use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// #4229 S6: no-result exit handler of the watcher loop (fresh ready-for-input
/// idle structural finalize / tmux-death break / cancel-shutdown break /
/// ready-for-input stall notice + dispatch-fail / #3419 watchdog-timeout
/// finalize), moved verbatim from tmux_watcher.rs L557-1272.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoResultExitOutcome {
    ContinueWatcherLoop,
    BreakWatcherLoop,
    Fallthrough,
}

pub(super) struct NoResultExitContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) output_path: &'a String,
    pub(super) paused: &'a Arc<AtomicBool>,
    pub(super) pause_epoch: &'a Arc<AtomicU64>,
    pub(super) cancel: &'a Arc<AtomicBool>,
    pub(super) turn_delivered: &'a Arc<AtomicBool>,
    pub(super) watcher_instance_id: u64,
}

pub(super) struct NoResultExitLocals<'a> {
    pub(super) found_result: bool,
    pub(super) was_paused: bool,
    pub(super) epoch_snapshot: u64,
    pub(super) full_response: &'a String,
    pub(super) turn_is_external_input_for_session: bool,
    pub(super) finish_mailbox_on_completion: bool,
    pub(super) startup_inflight_snapshot: Option<InflightTurnState>,
    pub(super) is_prompt_too_long: bool,
    pub(super) is_auth_error: bool,
    pub(super) is_provider_overloaded: bool,
    pub(super) prompt_too_long_killed: bool,
    pub(super) terminal_delivery_observed: bool,
    pub(super) active_read_state: Option<ActiveReadState>,
}

pub(super) struct NoResultExitState<'a> {
    pub(super) current_offset: &'a mut u64,
    pub(super) all_data: &'a mut String,
    pub(super) all_data_start_offset: &'a mut u64,
    pub(super) all_data_fully_mirrored_to_session_relay: &'a mut bool,
    pub(super) all_data_session_bound_relay_ack: &'a mut Option<SessionBoundRelayAckTarget>,
    pub(super) all_data_first_forwarded_relay_sequence: &'a mut Option<u64>,
    pub(super) last_relayed_offset: &'a mut Option<u64>,
    pub(super) last_observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(super) placeholder_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) status_panel_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) last_edit_text: &'a mut String,
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
}

pub(super) async fn handle_no_result_exits(
    context: &NoResultExitContext<'_>,
    locals: NoResultExitLocals<'_>,
    state: &mut NoResultExitState<'_>,
) -> NoResultExitOutcome {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let paused = context.paused;
    let pause_epoch = context.pause_epoch;
    let cancel = context.cancel;
    let turn_delivered = context.turn_delivered;
    let watcher_instance_id = context.watcher_instance_id;
    let NoResultExitLocals {
        found_result,
        was_paused,
        epoch_snapshot,
        full_response,
        turn_is_external_input_for_session,
        finish_mailbox_on_completion,
        startup_inflight_snapshot,
        is_prompt_too_long,
        is_auth_error,
        is_provider_overloaded,
        prompt_too_long_killed,
        terminal_delivery_observed,
        active_read_state,
    } = locals;
    let mut current_offset = *state.current_offset;
    if !found_result {
        let ActiveReadState {
            turn_start,
            turn_timeout,
            turn_idle_timeout,
            last_output_at,
            tmux_death_observed,
            ready_for_input_failure_notice,
            ready_for_input_stall_dispatch_id,
            ready_for_input_stall_inflight_snapshot,
            fresh_ready_for_input_idle,
        } = active_read_state.expect("active read state must exist when result was not found");

        if fresh_ready_for_input_idle {
            // #3016 S3: the STRUCTURAL completion signal — the authority that
            // finally distinguishes "turn done" from "paused-live" (which the
            // old flag-only path could not). Resolve the runtime kind exactly
            // as `watcher_session_ready_for_input` does (runtime binding →
            // tmux marker), then read the relay-offset-independent strict
            // terminator probe via the S1 read-only API. `output_path` is the
            // provider's on-disk JSONL transcript for this session.
            let watcher_runtime_kind =
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                    &tmux_session_name,
                )
                .map(|binding| binding.runtime_kind)
                .or_else(|| {
                    crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
                        &tmux_session_name,
                    )
                });
            let fresh_idle_completion_signal = shared.turn_finalizer.completion_signal_state(
                &watcher_provider,
                watcher_runtime_kind,
                std::path::Path::new(&output_path),
            );
            // #3016 S3 (A2 wrong-turn race fix): pin the finalize id from a
            // snapshot taken NOW — BEFORE the cleanup `.await`s below — and
            // gate it on the SAME output-range relationship the canonical
            // normal-completion site uses. A LATE re-read after the cleanup
            // awaits could observe a follow-up turn that became current on the
            // SAME session and rewrote inflight, finalizing the WRONG turn.
            let pinned_pre_cleanup_inflight =
                crate::services::discord::inflight::load_inflight_state(
                    &watcher_provider,
                    channel_id.get(),
                );
            // #3016 S3 / phase-5b1 (codex HIGH fix): the DEFER decision keys on
            // the STRUCTURAL TERMINATOR and — for non-JSONL `Unknown` runtimes —
            // on response EMPTINESS, NOT on the `mailbox_finalize_owed` flag. This
            // is the flag-independent reconstruction of the OLD (pre-5b1) defer
            // condition (`delegated_finalize_owed && empty`): `owed` was ~always
            // true for a delegated `Unknown` turn at this arm, so the old gate was
            // effectively "empty → defer". Re-keying on emptiness alone reproduces
            // it without the flag. Rationale: non-JSONL runtimes (Gemini / OpenCode
            // / Qwen / LegacyTmuxWrapper) have NO structured PausedLive signal — a
            // turn awaiting a selector / permission / interactive prompt can look
            // idle (ready_for_input sustained over the timeout) with EMPTY output.
            // Finalizing it here would kill the turn mid-work; instead we defer and
            // let the 5a 1800s far-backstop (which re-checks pane-idle at the
            // deadline) be its finalizer. NON-empty `Unknown` finalizes promptly
            // (the intended 5b1 improvement, flag-independent). `PausedLive` (no
            // terminator) always defers. `Done` (JSONL terminator proven) never
            // defers and finalizes even when empty. The wrong-turn-race guards in
            // `watcher_fresh_idle_finalize_decision` (paused/epoch abort, stale-skip)
            // still handle the follow-up-took-over cases for the finalize arms.
            let defer_fresh_idle = match fresh_idle_completion_signal {
                crate::services::discord::turn_finalizer::CompletionSignal::PausedLive => true,
                crate::services::discord::turn_finalizer::CompletionSignal::Done => false,
                crate::services::discord::turn_finalizer::CompletionSignal::Unknown => {
                    full_response.trim().is_empty()
                }
            };
            if defer_fresh_idle {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}, but no structural completion terminator yet (signal={fresh_idle_completion_signal:?}); preserving inflight and waiting for terminal commit"
                );
                state.all_data.clear();
                *state.all_data_start_offset = current_offset;
                *state.all_data_fully_mirrored_to_session_relay = true;
                *state.all_data_session_bound_relay_ack = None;
                *state.all_data_first_forwarded_relay_sequence = None;
                *state.last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut *state.monitor_auto_turn_claimed,
                    &mut *state.monitor_auto_turn_finished,
                    &mut *state.monitor_auto_turn_synthetic_msg_id,
                    &mut *state.monitor_auto_turn_ledger_generation,
                )
                .await;
                return NoResultExitOutcome::ContinueWatcherLoop;
            }
            let fresh_idle_effective_committed_offset = dr::effective_committed_offset(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                pinned_pre_cleanup_inflight
                    .as_ref()
                    .and_then(|state| state.output_path.as_deref())
                    .and_then(|path| std::fs::metadata(path).ok().map(|meta| meta.len())),
            );
            let fresh_idle_pending_session_bound_delivery =
                watcher_fresh_idle_session_bound_retry_plan(
                    pinned_pre_cleanup_inflight.as_ref(),
                    &tmux_session_name,
                    current_offset,
                    fresh_idle_effective_committed_offset,
                );
            if let Some(plan) = fresh_idle_pending_session_bound_delivery {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = watcher_provider.as_str(),
                    channel_id = channel_id.get(),
                    tmux_session = %tmux_session_name,
                    turn_start_offset = plan.turn_start_offset,
                    current_offset,
                    committed = fresh_idle_effective_committed_offset,
                    retry_offset = plan.retry_offset,
                    "  [{ts}] 👁 watcher fresh ready-for-input idle refused finalize: session-bound terminal body is not effectively committed; preserving inflight and rewinding for retry"
                );
                current_offset = plan.retry_offset;
                *state.current_offset = current_offset;
                state.all_data.clear();
                *state.all_data_start_offset = current_offset;
                *state.all_data_fully_mirrored_to_session_relay = true;
                *state.all_data_session_bound_relay_ack = None;
                *state.all_data_first_forwarded_relay_sequence = None;
                *state.last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                return NoResultExitOutcome::ContinueWatcherLoop;
            }
            let cleanup_committed = if let Some(msg_id) = *state.placeholder_msg_id {
                if watcher_should_delete_suppressed_placeholder(
                    *state.placeholder_from_restored_inflight,
                ) {
                    let outcome = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_fresh_ready_for_input_idle_cleanup",
                    )
                    .await;
                    if outcome.is_committed() {
                        let _ = state.placeholder_msg_id.take();
                        *state.placeholder_from_restored_inflight = false;
                        state.last_edit_text.clear();
                        true
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher: fresh ready-for-input cleanup did not commit for channel {} msg {}; preserving inflight for retry",
                            channel_id.get(),
                            msg_id.get()
                        );
                        false
                    }
                } else if watcher_should_reclaim_orphan_turn_placeholder(
                    turn_is_external_input_for_session,
                    *state.placeholder_msg_id,
                    !full_response.trim().is_empty(),
                    state.last_edit_text.as_str(),
                ) {
                    // #3351 (codex r2 #1): route the restored placeholder through the
                    // gated reclaim instead of stranding it; transient failure defers
                    // finalization like the panel guard above.
                    reclaim_orphan_external_input_placeholder(
                        &http,
                        &shared,
                        channel_id,
                        &mut *state.placeholder_msg_id,
                        &mut *state.placeholder_from_restored_inflight,
                        &mut *state.last_edit_text,
                        &watcher_provider,
                        &tmux_session_name,
                    )
                    .await
                } else {
                    let _ = state.placeholder_msg_id.take();
                    *state.placeholder_from_restored_inflight = false;
                    state.last_edit_text.clear();
                    true
                }
            } else {
                true
            };
            if !cleanup_committed {
                return NoResultExitOutcome::ContinueWatcherLoop;
            }
            // #3003 (codex P2 r3): fresh idle with no committed response means the
            // terminal completion path will not run, so reclaim any watcher-created
            // status panel before it orphans at "계속 처리 중". Self-gated to
            // external-input turns on this session (bridge-owned panels untouched).
            // #3003 (codex P2 r5): if the panel delete did not commit, defer
            // finalization — clearing the inflight here would drop the persisted
            // status_message_id and strand the panel with no retry path. Re-enter
            // fresh idle next iteration to retry, mirroring the placeholder guard.
            let panel_cleanup_committed = cleanup_orphan_external_input_status_panel(
                &http,
                &shared,
                channel_id,
                &mut *state.status_panel_msg_id,
                &watcher_provider,
                &tmux_session_name,
                turn_is_external_input_for_session,
            )
            .await;
            if !panel_cleanup_committed {
                return NoResultExitOutcome::ContinueWatcherLoop;
            }
            // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag is
            // removed. The finalize DECISION never depended on it — both `Done`
            // and `Unknown` route to the structural / pane-idle `Finalize` arm
            // with `normal_completion = true`; the residual `swap(false)` (whose
            // value fed only the observability payload) is gone with the field.
            // #3016 S3 / phase-5b1 (codex HIGH fix): the finalize DECISION,
            // computed by the same pure helper the unit tests drive. The defer
            // gate above already deferred `PausedLive` and EMPTY `Unknown`, so
            // here the signal is `Done` (empty or not) or NON-empty `Unknown` —
            // both route to the `Finalize` arm. Emptiness is threaded in
            // flag-independently so the helper can re-assert the empty-`Unknown`
            // defer defensively (it is the unreachable mirror of the gate above).
            let fresh_idle_decision = watcher_fresh_idle_finalize_decision(
                fresh_idle_completion_signal,
                full_response.trim().is_empty(),
                paused.load(Ordering::Relaxed),
                pause_epoch.load(Ordering::Relaxed) != epoch_snapshot,
                pinned_pre_cleanup_inflight.as_ref(),
                &tmux_session_name,
                current_offset,
            );
            match fresh_idle_decision {
                FreshIdleFinalizeDecision::DeferPausedLive => {
                    // Unreachable: PausedLive was deferred at the defer gate
                    // above. Treat defensively as a defer (preserve inflight).
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: PausedLive reached the finalize gate unexpectedly; preserving inflight"
                    );
                    state.all_data.clear();
                    *state.all_data_start_offset = current_offset;
                    *state.all_data_fully_mirrored_to_session_relay = true;
                    *state.all_data_session_bound_relay_ack = None;
                    *state.all_data_first_forwarded_relay_sequence = None;
                    return NoResultExitOutcome::ContinueWatcherLoop;
                }
                FreshIdleFinalizeDecision::DeferEmptyUnknown => {
                    // Unreachable: empty `Unknown` was deferred at the defer gate
                    // above. Treat defensively as a defer (preserve inflight) —
                    // the 5a 1800s far-backstop finalizes the empty turn later.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: empty Unknown reached the finalize gate unexpectedly; preserving inflight (far-backstop will finalize)"
                    );
                    state.all_data.clear();
                    *state.all_data_start_offset = current_offset;
                    *state.all_data_fully_mirrored_to_session_relay = true;
                    *state.all_data_session_bound_relay_ack = None;
                    *state.all_data_first_forwarded_relay_sequence = None;
                    return NoResultExitOutcome::ContinueWatcherLoop;
                }
                FreshIdleFinalizeDecision::AbortFollowupTookOver => {
                    // #3016 S3 (A2 wrong-turn race fix): a Discord turn claimed
                    // this session during the cleanup `.await`s (paused / epoch
                    // bumped at handoff). The canonical pause/epoch guard sits
                    // AFTER this branch's `continue`, so we mirror it HERE,
                    // before the destructive clear, to avoid releasing the
                    // follow-up turn.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name} aborted before finalize: follow-up turn took over (paused/epoch changed); preserving inflight"
                    );
                    state.all_data.clear();
                    *state.all_data_start_offset = current_offset;
                    *state.all_data_fully_mirrored_to_session_relay = true;
                    *state.all_data_session_bound_relay_ack = None;
                    *state.all_data_first_forwarded_relay_sequence = None;
                    return NoResultExitOutcome::ContinueWatcherLoop;
                }
                FreshIdleFinalizeDecision::SkipStale { pinned_user_msg_id } => {
                    // #3016 S3 (A2 wrong-turn race fix): the pinned pre-cleanup
                    // snapshot is a NEWER turn that began AT/AFTER this
                    // committed range; finalizing would release the follow-up.
                    // Skip and preserve inflight for the current/newer turn.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name} skipped finalize: pinned id {pinned_user_msg_id} is stale for a newer turn at offset {current_offset}; preserving inflight"
                    );
                    state.all_data.clear();
                    *state.all_data_start_offset = current_offset;
                    *state.all_data_fully_mirrored_to_session_relay = true;
                    *state.all_data_session_bound_relay_ack = None;
                    *state.all_data_first_forwarded_relay_sequence = None;
                    return NoResultExitOutcome::ContinueWatcherLoop;
                }
                FreshIdleFinalizeDecision::Finalize { user_msg_id } => {
                    // #3016 S3 (the A2 / phase-5 enabler): a structural JSONL
                    // terminator is PROVEN on disk for this turn (Done) AND no
                    // follow-up took over — finalize via the single-authority
                    // path with `normal_completion = true`, FLAG-INDEPENDENT,
                    // so an EMPTY-but-terminated completion finalizes too (the
                    // old flag-gated path could not tell it from a paused-live
                    // turn). The finalizer is idempotent (`AlreadyFinalized`),
                    // and `user_msg_id` is PINNED from the pre-cleanup snapshot
                    // at this `current_offset` (never a late re-read), so the
                    // ledger match is the CURRENT turn's real, non-zero id.
                    //
                    // #3016 S3 (Concern 2 — residual TOCTOU): the destructive
                    // on-disk clear must not wipe a FOLLOW-UP turn's inflight.
                    // The earlier read→check→unconditional-clear spanned TWO
                    // locks, so a follow-up saved on another worker thread in
                    // the gap was wiped. The nonce-aware guarded clear closes
                    // the window atomically: read + validate + unlink under ONE
                    // sidecar lock, deleting only while the on-disk FULL
                    // identity (`user_msg_id` + `started_at` + `tmux_session_name`
                    // + `turn_start_offset`) and, when both rows carry one, the
                    // `turn_nonce` still equal the PINNED pre-cleanup snapshot.
                    // A nonce-distinct follow-up is a no-op; finalize still runs on the PINNED id.
                    let pinned_clear_identity = pinned_pre_cleanup_inflight
                        .as_ref()
                        .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
                    let pinned_clear_turn_nonce = pinned_pre_cleanup_inflight
                        .as_ref()
                        .and_then(|state| state.turn_nonce.as_deref());
                    if let Some(pinned_clear_identity) = pinned_clear_identity.as_ref() {
                        let clear_outcome =
                                crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
                                    &watcher_provider,
                                    channel_id.get(),
                                    pinned_clear_identity,
                                    pinned_clear_turn_nonce,
                                );
                        match clear_outcome {
                            crate::services::discord::inflight::GuardedClearOutcome::Cleared => {
                                crate::services::observability::emit_inflight_lifecycle_event(
                                    watcher_provider.as_str(),
                                    channel_id.get(),
                                    None,
                                    None,
                                    None,
                                    "cleared_by_watcher_fresh_idle",
                                    serde_json::json!({
                                        "finish_mailbox_on_completion": finish_mailbox_on_completion,
                                        // #3016 phase-5b1: Done (structural) OR
                                        // Unknown (pane-idle proxy) both reach here.
                                        "completion_signal": format!("{fresh_idle_completion_signal:?}"),
                                        "tmux_session": tmux_session_name.as_str(),
                                        "offset": current_offset,
                                    }),
                                );
                            }
                            crate::services::discord::inflight::GuardedClearOutcome::IoError => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    clear_outcome = ?clear_outcome,
                                    "  [{ts}] ⚠ watcher fresh ready-for-input idle for {tmux_session_name}: atomic identity-matched clear failed with IO error at offset {current_offset}; see preceding inflight guarded-clear error detail"
                                );
                            }
                            other => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: atomic identity-matched clear was a no-op (outcome={other:?}) at offset {current_offset} — on-disk inflight is no longer the pinned turn (follow-up preserved); finalizing the pinned current turn only"
                                );
                            }
                        }
                    } else {
                        // No pinned snapshot identity available — there is
                        // nothing safe to clear by identity. Skip the clear and
                        // finalize on the pinned id only. (Unreachable on the
                        // `Finalize` arm, since `pinned_finalize_user_msg_id`
                        // requires a non-zero pinned snapshot to return a
                        // finalizable id; kept defensive.)
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: no pinned snapshot identity for the atomic clear at offset {current_offset}; skipping the on-disk clear and finalizing the pinned current turn only"
                        );
                    }
                    finish_restored_watcher_active_turn(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            user_msg_id,
                            finish_mailbox_on_completion,
                            // #3016 S3 / phase-5b1: Done = confirmed structural
                            // completion; Unknown = non-JSONL runtime at proven
                            // pane-idle. Both drive the finalizer on the
                            // normal-completion authority, independent of the legacy
                            // flag (removed in #3016 phase-5b2).
                            true,
                            true,
                            // #3350 codex r1-1: the row was cleared above — the
                            // finalize-time marker ensure authenticates against
                            // this pre-clear snapshot instead of a no-op re-load.
                            pinned_pre_cleanup_inflight.as_ref().map(
                                crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row,
                            ),
                            "watcher fresh ready-for-input idle (structural/pane-idle completion)",
                        )
                        .await;
                }
            }
            state.all_data.clear();
            *state.all_data_start_offset = current_offset;
            *state.all_data_fully_mirrored_to_session_relay = true;
            *state.all_data_session_bound_relay_ack = None;
            *state.all_data_first_forwarded_relay_sequence = None;
            *state.last_relayed_offset = Some(current_offset);
            *state.last_observed_generation_mtime_ns =
                Some(read_generation_file_mtime_ns(&tmux_session_name));
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                current_offset,
                "src/services/discord/tmux.rs:ready_for_input_fresh_idle",
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return NoResultExitOutcome::ContinueWatcherLoop;
        }

        if tmux_death_observed {
            handle_tmux_watcher_observed_death(
                channel_id,
                &http,
                &shared,
                &tmux_session_name,
                &output_path,
                &watcher_provider,
                prompt_too_long_killed,
                watcher_lifecycle_terminal_delivery_observed(
                    terminal_delivery_observed,
                    turn_delivered.load(Ordering::Acquire),
                ),
            )
            .await;
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return NoResultExitOutcome::BreakWatcherLoop;
        }

        if cancel.load(Ordering::Relaxed) || shared.restart.shutting_down.load(Ordering::Relaxed) {
            // #3277 (Defect B): same stop-reason visibility as the early break.
            tracing::info!(
                instance = watcher_instance_id,
                cancel = cancel.load(Ordering::Relaxed),
                shutting_down = shared.restart.shutting_down.load(Ordering::Relaxed),
                "tmux watcher stopping for #{tmux_session_name}: cancelled/shutdown"
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return NoResultExitOutcome::BreakWatcherLoop;
        }

        if let Some(notice) = ready_for_input_failure_notice {
            let notice_ok = match *state.placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    crate::services::discord::http::edit_channel_message(
                        &http, channel_id, msg_id, &notice,
                    )
                    .await
                    .is_ok()
                }
                None => {
                    crate::services::discord::http::send_channel_message(&http, channel_id, &notice)
                        .await
                        .is_ok()
                }
            };
            if !notice_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ watcher: Ready-for-input stall notice failed before dispatch failure — preserving inflight for retry"
                );
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut *state.monitor_auto_turn_claimed,
                    &mut *state.monitor_auto_turn_finished,
                    &mut *state.monitor_auto_turn_synthetic_msg_id,
                    &mut *state.monitor_auto_turn_ledger_generation,
                )
                .await;
                return NoResultExitOutcome::ContinueWatcherLoop;
            }

            if let Some(dispatch_id) = ready_for_input_stall_dispatch_id {
                match fail_dispatch_for_ready_for_input_stall(
                    &shared,
                    &dispatch_id,
                    &tmux_session_name,
                )
                .await
                {
                    Ok(result) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher marked post-work Ready-for-input stall as failed for {} / dispatch {} (card={:?}, card_marked={}, human_alert_sent={})",
                            tmux_session_name,
                            dispatch_id,
                            result.card_id,
                            result.card_marked,
                            result.human_alert_sent
                        );
                        // Skip rebind-origin (synthetic, no real user
                        // message) and user_msg_id == 0 (a TUI-direct turn
                        // with no anchored Discord user message): there is
                        // no message to react against, and
                        // `MessageId::new(0)` would panic.
                        if let Some(state) = ready_for_input_stall_inflight_snapshot
                            .as_ref()
                            .filter(|state| !state.rebind_origin && state.user_msg_id != 0)
                        {
                            let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                            crate::services::discord::turn_view_reconciler::note_intake_turn_failed(
                                    &shared,
                                    &http,
                                    channel_id,
                                    user_msg_id,
                                    state.born_generation,
                                    "tmux_watcher_ready_for_input_stall",
                                )
                                .await;
                        }
                        finalize_pinned_watcher_exit(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            ready_for_input_stall_inflight_snapshot.as_ref(),
                            "watcher_ready_for_input_stall",
                        )
                        .await;
                    }
                    Err(error) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher failed to persist Ready-for-input stall failure for {} / dispatch {}: {}",
                            tmux_session_name,
                            dispatch_id,
                            error
                        );
                        let failure_notice = format!(
                            "⚠️ 작업 후 `Ready for input` 상태에서 멈췄지만 dispatch 실패 처리를 저장하지 못했습니다.\n사유: {}",
                            truncate_str(&error, 300)
                        );
                        match *state.placeholder_msg_id {
                            Some(msg_id) => {
                                rate_limit_wait(&shared, channel_id).await;
                                let _ = crate::services::discord::http::edit_channel_message(
                                    &http,
                                    channel_id,
                                    msg_id,
                                    &failure_notice,
                                )
                                .await;
                            }
                            None => {
                                let _ = crate::services::discord::http::send_channel_message(
                                    &http,
                                    channel_id,
                                    &failure_notice,
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            clear_provider_overload_retry_state(channel_id);
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return NoResultExitOutcome::ContinueWatcherLoop;
        }

        // #3419 R2: turn-watchdog timeout fall-through (`!found_result` past the
        // fresh-idle / tmux-death / cancel / notice exits). Pre-#3419 this left
        // the turn UN-finalized (TurnFinalizer never ran, mailbox cancel_token
        // leaked, soft-queue wedged). Route through the SAME
        // `finish_restored_watcher_active_turn` entry normal completion uses (no
        // new authority; once-gate makes a later normal finalize idempotent).
        // Skip when paused/epoch-bumped or an error branch owns cleanup (below).
        // #3419 R3 (codex HIGH — drain re-acquire id-0 wedge, no steal): key the
        // decision on the LIVE MAILBOX active-turn id, not the on-disk inflight
        // (the mailbox token wedges the queue; re-acquire can mint an id-0
        // inflight while pinned A's token is still active, so R2's on-disk test
        // Skipped A and left it wedged). Finalize ONLY when the mailbox still
        // holds pinned A's token; a DIFFERENT live turn B / no active turn → Skip.
        // The submit is A's REAL pinned id via identity-guarded
        // `mailbox_finish_turn_if_matches`, so B can't be stolen / id-0 submitted.
        // #3419 B: NOT-active (idle OR cap expired) routes the stuck turn
        // through this C finalize; same predicate as the loop (single authority).
        if !found_result
            && !watcher_turn_still_active(
                last_output_at.elapsed(),
                turn_idle_timeout,
                turn_start.elapsed(),
                turn_timeout,
            )
            && !was_paused
            && pause_epoch.load(Ordering::Relaxed) == epoch_snapshot
            && !is_prompt_too_long
            && !is_auth_error
            && !is_provider_overloaded
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // Wedge is the mailbox token; decide on its CURRENT active-turn id (different/absent = B took over / released).
            let mailbox_active_user_msg_id = shared
                .mailbox(channel_id)
                .snapshot()
                .await
                .active_user_message_id
                .map(serenity::MessageId::get);
            match watcher_timeout_finalize_decision(
                startup_inflight_snapshot.as_ref(),
                mailbox_active_user_msg_id,
                &tmux_session_name,
            ) {
                TimeoutFinalizeDecision::Skip { pinned_user_msg_id } => {
                    tracing::warn!(
                        "  [{ts}] ⚠ #3419: watcher turn watchdog timed out for {tmux_session_name} after {}s, but pinned turn {pinned_user_msg_id} no longer holds the mailbox token (id-0 / no active turn / newer turn took over); NOT finalizing — the live turn finalizes itself",
                        turn_start.elapsed().as_secs()
                    );
                }
                TimeoutFinalizeDecision::Finalize { user_msg_id } => {
                    tracing::warn!(
                        "  [{ts}] ⚠ #3419: watcher turn watchdog timed out for {tmux_session_name} after {}s (pinned turn {user_msg_id} still holds the mailbox token); routing through the single-authority finalizer to release the token and drain the queue",
                        turn_start.elapsed().as_secs()
                    );
                    // Identity-matched clear: removes the row ONLY while still
                    // the pinned turn (same identity INCL. turn_start_offset, so
                    // clear key == decision key). A re-acquired id-0 / newer row →
                    // `UserMsgMismatch` no-op (drain frees the token, stale row untouched).
                    if let Some(pinned) = startup_inflight_snapshot.as_ref() {
                        let _ = crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
                                &watcher_provider,
                                channel_id.get(),
                                &crate::services::discord::inflight::InflightTurnIdentity::from_state(pinned),
                            );
                    }
                    // finish_mailbox=true releases the watcher token (wedge fix);
                    // normal_completion=false; kickoff_queue=true admits the next
                    // turn. The REAL pinned id keys IDENTITY-GUARDED
                    // `mailbox_finish_turn_if_matches` (can't release a newer turn).
                    finish_restored_watcher_active_turn(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            user_msg_id,
                            true,
                            false,
                            true,
                            startup_inflight_snapshot.as_ref().map(
                                crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row,
                            ),
                            "watcher turn watchdog timeout (#3419)",
                        )
                        .await;
                }
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return NoResultExitOutcome::ContinueWatcherLoop;
        }
    }

    NoResultExitOutcome::Fallthrough
}
