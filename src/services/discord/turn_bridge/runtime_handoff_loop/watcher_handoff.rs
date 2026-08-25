//! Watcher-backed runtime handoff claim, persistence, and relay activation.

use super::*;

pub(super) struct WatcherRuntimeHandoffContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) runtime_kind: RuntimeHandoffKind,
    pub(super) output_path: String,
    pub(super) input_fifo_path: Option<String>,
    pub(super) tmux_session_name: String,
    pub(super) session_id: Option<String>,
    pub(super) last_offset: u64,
    pub(super) done: bool,
}

pub(super) struct WatcherRuntimeHandoffState<'a> {
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) tmux_last_offset: &'a mut Option<u64>,
    pub(super) watcher_owner_channel_id: &'a mut ChannelId,
    pub(super) standby_relay_owns_output: &'a mut bool,
    pub(super) watcher_relay_available_for_turn: &'a mut bool,
    pub(super) watcher_delivery_pin: &'a mut Option<Arc<std::sync::atomic::AtomicBool>>,
    pub(super) watcher_handoff_claim_outcome: &'a mut WatcherHandoffClaimOutcome,
    pub(super) tmux_handed_off: &'a mut bool,
    pub(super) watcher_owns_assistant_relay: &'a mut bool,
    pub(super) state_dirty: &'a mut bool,
    pub(super) terminal_control_drain_until: &'a mut Option<std::time::Instant>,
}

pub(super) fn cancel_provisional_watcher_claim_if_matches(
    shared: &SharedData,
    owner_channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    provisional_cancel: &Arc<std::sync::atomic::AtomicBool>,
) {
    shared.tmux_watchers.cancel_and_remove_channel_if_current(
        &owner_channel_id,
        tmux_session_name,
        output_path,
        provisional_cancel,
    );
}

pub(super) fn handle_watcher_runtime_handoff(
    ctx: WatcherRuntimeHandoffContext<'_>,
    state: WatcherRuntimeHandoffState<'_>,
) -> crate::services::discord::inflight::GuardedSaveOutcome {
    let shared_owned = ctx.shared_owned;
    let provider = ctx.provider;
    let channel_id = ctx.channel_id;
    let runtime_kind = ctx.runtime_kind;
    let output_path = ctx.output_path;
    let input_fifo_path = ctx.input_fifo_path;
    let tmux_session_name = ctx.tmux_session_name;
    let session_id = ctx.session_id;
    let last_offset = ctx.last_offset;
    let done = ctx.done;
    let inflight_state = state.inflight_state;
    let tmux_last_offset = state.tmux_last_offset;
    let watcher_owner_channel_id = state.watcher_owner_channel_id;
    let standby_relay_owns_output = state.standby_relay_owns_output;
    let watcher_relay_available_for_turn = state.watcher_relay_available_for_turn;
    let watcher_delivery_pin = state.watcher_delivery_pin;
    let watcher_handoff_claim_outcome = state.watcher_handoff_claim_outcome;
    let tmux_handed_off = state.tmux_handed_off;
    let watcher_owns_assistant_relay = state.watcher_owns_assistant_relay;
    let state_dirty = state.state_dirty;
    let terminal_control_drain_until = state.terminal_control_drain_until;
    let state_dirty_before_handoff = *state_dirty;
    let persisted_baseline = inflight_state.clone();
    let expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&persisted_baseline);

    *tmux_last_offset = Some(last_offset);
    inflight_state.runtime_kind = Some(runtime_kind);
    inflight_state.tmux_session_name = Some(tmux_session_name.clone());
    inflight_state.output_path = Some(output_path.clone());
    if let Some(session_id) = session_id {
        inflight_state.session_id = Some(session_id);
    }
    let mut fifo_path = input_fifo_path.filter(|path| !path.is_empty());
    // #2235 one-release compat window: ClaudeTui rows must still ship a
    // populated `input_fifo_path` so a rollback to an old binary can satisfy
    // its FIFO-required recovery branch. Synthesize from the canonical
    // per-session tmux path when the caller didn't supply one.
    if matches!(runtime_kind, RuntimeHandoffKind::ClaudeTui) && fifo_path.is_none() {
        let (_, synthesized_fifo) = tmux_runtime_paths(&tmux_session_name);
        if !synthesized_fifo.is_empty() {
            fifo_path = Some(synthesized_fifo);
        }
    }
    inflight_state.input_fifo_path = fifo_path;
    inflight_state.last_offset = last_offset;
    *state_dirty |= inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
    #[cfg(unix)]
    let relay_http_available = shared_owned.serenity_http_or_token_fallback().is_some();
    #[cfg(unix)]
    let on_standby = shared_owned.http.cached_serenity_ctx.get().is_none();
    #[cfg(unix)]
    let intended_relay_owner = if relay_http_available {
        if on_standby {
            super::super::inflight::RelayOwnerKind::StandbyRelay
        } else {
            super::super::inflight::RelayOwnerKind::Watcher
        }
    } else {
        super::super::inflight::RelayOwnerKind::None
    };
    #[cfg(not(unix))]
    let intended_relay_owner = super::super::inflight::RelayOwnerKind::None;
    inflight_state.set_relay_owner_kind(intended_relay_owner);

    // Durable ownership is the admission ticket for every watcher/relay side
    // effect below. The exact stamped row replaces the local projection before
    // a registry claim can observe it.
    let mut outcome = guarded_runtime_atomic_stamp(
        &persisted_baseline,
        inflight_state,
        &expected_identity,
        channel_id,
        "turn_bridge::runtime_handoff_loop::watcher_runtime_owner_handoff",
    );
    if outcome != crate::services::discord::inflight::GuardedSaveOutcome::Saved {
        *watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
        *watcher_relay_available_for_turn = false;
        *tmux_handed_off = false;
        *watcher_owns_assistant_relay = false;
        *state_dirty =
            tmux_ready_state_dirty_after_guarded_save(state_dirty_before_handoff, Some(outcome));
        if done {
            *terminal_control_drain_until = None;
        }
        return outcome;
    }
    *tmux_last_offset = Some(inflight_state.last_offset);
    *watcher_owner_channel_id = ChannelId::new(
        inflight_state
            .watcher_owner_channel_id
            .unwrap_or(channel_id.get()),
    );

    // #226: Atomic claim via try_claim_watcher
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
    let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
        super::super::tmux_watcher_now_ms(),
    ));
    let handle = TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.clone(),
        output_path: output_path.clone(),
        paused: paused.clone(),
        resume_offset: resume_offset.clone(),
        cancel: cancel.clone(),
        pause_epoch: pause_epoch.clone(),
        turn_delivered: turn_delivered.clone(),
        last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
    };
    let pre_claim_persisted = inflight_state.clone();
    #[cfg(unix)]
    let (
        watcher_claimed,
        watcher_claim_replaced_existing,
        owner_changed_after_claim,
        watcher_claim_incarnation,
    ) = {
        let claim = super::super::tmux::claim_or_reuse_watcher_with_thread_parent(
            &shared_owned.tmux_watchers,
            channel_id,
            handle,
            provider,
            "turn_bridge_runtime_ready",
            super::super::tmux::thread_follow_up_parent_channel_id(
                channel_id,
                inflight_state.logical_channel_id,
                inflight_state.thread_id,
            ),
        );
        *watcher_owner_channel_id = claim.owner_channel_id();
        let owner_changed =
            inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
        *state_dirty |= owner_changed;
        let incarnation = claim.incarnation().clone();
        (
            claim.should_spawn(),
            claim.replaced_existing(),
            owner_changed,
            Some(incarnation),
        )
    };
    #[cfg(not(unix))]
    let (watcher_claimed, watcher_claim_replaced_existing, owner_changed_after_claim) = {
        let _ = handle;
        (false, false, false)
    };
    if owner_changed_after_claim {
        let claim_expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(
            &pre_claim_persisted,
        );
        outcome = guarded_runtime_atomic_stamp(
            &pre_claim_persisted,
            inflight_state,
            &claim_expected,
            channel_id,
            "turn_bridge::runtime_handoff_loop::watcher_runtime_claim_owner",
        );
        if outcome != crate::services::discord::inflight::GuardedSaveOutcome::Saved {
            if watcher_claimed {
                cancel_provisional_watcher_claim_if_matches(
                    shared_owned.as_ref(),
                    *watcher_owner_channel_id,
                    &tmux_session_name,
                    &output_path,
                    &cancel,
                );
            }
            *watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
            *watcher_relay_available_for_turn = false;
            *tmux_handed_off = false;
            *watcher_owns_assistant_relay = false;
            *state_dirty = tmux_ready_state_dirty_after_guarded_save(
                state_dirty_before_handoff,
                Some(outcome),
            );
            if done {
                *terminal_control_drain_until = None;
            }
            return outcome;
        }
    }
    #[cfg(unix)]
    let mut watcher_ready_for_relay = !watcher_claimed;
    #[cfg(not(unix))]
    let mut watcher_ready_for_relay = false;
    *watcher_handoff_claim_outcome = if watcher_claimed {
        WatcherHandoffClaimOutcome::Spawned
    } else {
        WatcherHandoffClaimOutcome::ReusedExisting
    };
    let _ = watcher_claim_replaced_existing;
    if watcher_claimed {
        #[cfg(unix)]
        {
            let on_standby = shared_owned.http.cached_serenity_ctx.get().is_none();
            if on_standby {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ standby relay: skipping tmux watcher spawn for channel {}; spawning JSONL→Discord standby_relay",
                    channel_id
                );
                // Retire only this provisional watcher. Do not cancel:
                // the standby relay is the successor owner.
                let _ = shared_owned
                    .tmux_watchers
                    .remove_tmux_session_if_current(&tmux_session_name, &cancel);
                *watcher_delivery_pin = None;
                if let Some(http_for_standby) = shared_owned.serenity_http_or_token_fallback() {
                    let placeholder_msg_id_opt = if inflight_state.current_msg_id == 0 {
                        None
                    } else {
                        Some(serenity::MessageId::new(inflight_state.current_msg_id))
                    };
                    let output_path_for_standby = output_path.clone();
                    let turn_binding_for_standby =
                        super::super::standby_relay::StandbyRelayTurnBinding::from_state(
                            &inflight_state,
                        );
                    let cancel_for_standby = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let shared_for_standby = shared_owned.clone();
                    let provider_for_standby = provider.clone();
                    super::super::task_supervisor::spawn_observed(
                        "turn_bridge_standby_relay",
                        super::super::standby_relay::run_standby_relay(
                            http_for_standby,
                            channel_id,
                            placeholder_msg_id_opt,
                            output_path_for_standby,
                            turn_binding_for_standby.clone(),
                            turn_binding_for_standby.polling_start_offset(last_offset),
                            cancel_for_standby,
                            shared_for_standby,
                            provider_for_standby,
                            // #2448: bumped from 900s (15min) heuristic stop
                            // signal to a 1800s (30min) safety backstop. The
                            // authoritative exit signal is now
                            // `InflightSignal::Completed`, broadcast by
                            // `CompletionGuard` on bridge drop.
                            std::time::Duration::from_secs(1800),
                        ),
                    );
                    *standby_relay_owns_output = true;
                    inflight_state
                        .set_relay_owner_kind(super::super::inflight::RelayOwnerKind::StandbyRelay);
                    // #2263: intentionally leave `watcher_owns_live_relay = false`
                    // on the standby branch.
                    //
                    // The flag's downstream contract in
                    // `tmux::watcher_should_yield_to_inflight_state` is
                    // narrowly "the restored TMUX WATCHER itself owns
                    // delivery for this turn — do not yield". The standby
                    // branch never spawns a watcher (the briefly-claimed
                    // slot was just removed at line ~1477); the
                    // `standby_relay` task is a separate, non-persisted
                    // delivery owner whose ownership is NOT representable
                    // by this single boolean.
                    //
                    // Setting the flag to `true` here would over-claim
                    // ownership for any watcher restored against this
                    // state on a different node (or after failover) — it
                    // would short-circuit the yield gate and let a
                    // restored watcher deliver concurrently with a still-
                    // alive standby_relay, producing duplicate Discord
                    // posts (codex adversarial review on #2263).
                    //
                    // The cost of keeping it `false` is the phantom-
                    // bridge yield window: on restart, a restored watcher
                    // whose tmux offset overlaps `turn_start_offset` will
                    // yield to a bridge owner that died with the original
                    // standby process and will suppress relay for the
                    // overlapping batch. The inflight row is then cleared
                    // by the `INFLIGHT_STALENESS_THRESHOLD_SECS` (300s)
                    // staleness path in `classify_inflight_diagnostic_state`
                    // (router/message_handler.rs) and the recovery-engine
                    // sweep, after which a follow-up user turn proceeds
                    // normally. The completed standby_relay response that
                    // landed before the crash is preserved on Discord (it
                    // was posted before the process died); the failure
                    // mode is the user-visible stall on the FOLLOW-UP
                    // turn until staleness sweep, NOT a dropped response.
                    //
                    // #2376 records `relay_owner_kind = standby_relay` so a
                    // restored watcher can yield for every live batch, not
                    // only batches that overlap the original turn_start_offset.
                    // A future owner-lease timestamp can distinguish
                    // dead-standby from live-standby and remove the phantom
                    // yield window entirely.
                    //
                    // Per-turn in-process state is still correctly tracked
                    // by `standby_relay_owns_output = true` above; that
                    // local flag is what gates the bridge's terminal
                    // delivery suppression for the current turn.
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ standby relay skipped: no Http source for channel {}",
                        channel_id
                    );
                }
            } else if let Some(http_bg) = shared_owned.serenity_http_or_token_fallback() {
                let shared_bg = shared_owned.clone();
                inflight_state
                    .set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
                let restored_turn = super::super::tmux::restored_watcher_turn_from_inflight(
                    inflight_state,
                    &tmux_session_name,
                    true,
                );
                if let Ok(mut guard) = resume_offset.lock() {
                    *guard = Some(last_offset);
                }
                turn_delivered.store(false, std::sync::atomic::Ordering::Relaxed);
                if watcher_claim_replaced_existing {
                    shared_owned.record_tmux_watcher_reconnect(channel_id);
                }
                super::super::task_supervisor::spawn_observed_tmux_watcher(
                    "turn_bridge_tmux_output_watcher_with_restore",
                    shared_bg.clone(),
                    tmux_session_name.clone(),
                    cancel.clone(),
                    super::super::tmux::tmux_output_watcher_with_restore(
                        channel_id,
                        http_bg,
                        shared_bg,
                        output_path,
                        tmux_session_name,
                        last_offset,
                        cancel,
                        paused,
                        resume_offset,
                        pause_epoch,
                        turn_delivered,
                        last_heartbeat_ts_ms,
                        restored_turn,
                    ),
                );
                *watcher_relay_available_for_turn = true;
                watcher_ready_for_relay = true;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ no Http source (neither cached_serenity_ctx nor cached_bot_token); tmux watcher not started for channel {}",
                    channel_id
                );
                cancel_provisional_watcher_claim_if_matches(
                    shared_owned.as_ref(),
                    *watcher_owner_channel_id,
                    &tmux_session_name,
                    &output_path,
                    &cancel,
                );
                *watcher_delivery_pin = None;
            }
        }
    }
    #[cfg(unix)]
    if watcher_ready_for_relay {
        let watcher_claim_incarnation = watcher_claim_incarnation
            .as_ref()
            .expect("unix watcher claim carries its exact incarnation");
        let adopted = watcher_claim_incarnation.adopt_if_current(
            &shared_owned.tmux_watchers,
            |watcher_claim_incarnation| {
                super::adopt_claimed_watcher_delivery_marker(
                    watcher_delivery_pin,
                    &watcher_claim_incarnation.turn_delivered,
                );
                if let Ok(mut guard) = watcher_claim_incarnation.resume_offset.lock() {
                    *guard = Some(last_offset);
                }
                watcher_claim_incarnation
                    .turn_delivered
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                *tmux_handed_off = true;
                *watcher_relay_available_for_turn = true;
                *watcher_owns_assistant_relay = true;
                inflight_state
                    .set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        // #3016 phase 2: register the turn with the single-authority
        // finalizer BEFORE unpausing the watcher. Message arrival order in
        // the actor replaces the deleted Release/AcqRel ordering: the
        // ledger now knows the turn exists (with the watcher as relay
        // owner) before the watcher can submit its terminal. The ledger is
        // the authority that superseded the legacy `mailbox_finalize_owed`
        // flag (removed in #3016 phase-5b2) and the CAS revoke deleted from
        // the bridge finalize branches below.
        shared_owned
                .turn_finalizer
                .register_start_with_completion_admission(
                    super::super::turn_finalizer::TurnKey::new(
                        channel_id,
                        inflight_state.effective_finalizer_turn_id(),
                        shared_owned.restart.current_generation,
                    ),
                    provider.clone(),
                    super::super::inflight::RelayOwnerKind::Watcher,
                    super::super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                    // #3016 phase-5a: prime the reconcile cache at register time.
                    shared_owned,
                );
                watcher_claim_incarnation
                    .paused
                    .store(false, std::sync::atomic::Ordering::Release);
            },
        )
        .is_some();
        if !adopted {
            *tmux_handed_off = false;
            *watcher_relay_available_for_turn = false;
            *watcher_owns_assistant_relay = false;
            *watcher_delivery_pin = None;
            *watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
            inflight_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::None);
        }
    }
    *state_dirty = tmux_ready_state_dirty_after_guarded_save(*state_dirty, Some(outcome));
    if done {
        *terminal_control_drain_until = None;
    }
    outcome
}
