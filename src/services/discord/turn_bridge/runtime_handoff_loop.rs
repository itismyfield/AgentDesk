//! Runtime handoff stream-loop arms for `turn_bridge::spawn_turn_bridge`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind};

use super::*;

mod claude_e;
mod watcher_handoff;
use watcher_handoff::{
    WatcherRuntimeHandoffContext, WatcherRuntimeHandoffState,
    cancel_provisional_watcher_claim_if_matches, handle_watcher_runtime_handoff,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RuntimeHandoffLoopMessage {
    TmuxReady {
        output_path: String,
        input_fifo_path: String,
        tmux_session_name: String,
        last_offset: u64,
    },
    RuntimeReady {
        handoff: RuntimeHandoff,
    },
    ProcessReady {
        output_path: String,
        session_name: String,
        last_offset: u64,
    },
    OutputOffset {
        offset: u64,
    },
}

impl RuntimeHandoffLoopMessage {
    pub(super) fn into_stream_message(self) -> StreamMessage {
        match self {
            Self::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name,
                last_offset,
            } => StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name,
                last_offset,
            },
            Self::RuntimeReady { handoff } => StreamMessage::RuntimeReady { handoff },
            Self::ProcessReady {
                output_path,
                session_name,
                last_offset,
            } => StreamMessage::ProcessReady {
                output_path,
                session_name,
                last_offset,
            },
            Self::OutputOffset { offset } => StreamMessage::OutputOffset { offset },
        }
    }
}

pub(super) struct RuntimeHandoffLoopOutcome {
    pub(super) guarded_save_outcome: Option<crate::services::discord::inflight::GuardedSaveOutcome>,
    pub(super) retry_message: Option<RuntimeHandoffLoopMessage>,
}

pub(super) struct RuntimeHandoffLoopContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) done: bool,
    pub(super) adk_session_name: &'a Option<String>,
}

pub(super) struct RuntimeHandoffLoopState<'a> {
    pub(super) terminal_control_ready_observed: &'a mut bool,
    pub(super) tmux_last_offset: &'a mut Option<u64>,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) watcher_owner_channel_id: &'a mut ChannelId,
    pub(super) standby_relay_owns_output: &'a mut bool,
    pub(super) watcher_relay_available_for_turn: &'a mut bool,
    pub(super) watcher_handoff_claim_outcome: &'a mut WatcherHandoffClaimOutcome,
    pub(super) tmux_handed_off: &'a mut bool,
    pub(super) watcher_owns_assistant_relay: &'a mut bool,
    pub(super) state_dirty: &'a mut bool,
    pub(super) terminal_control_drain_until: &'a mut Option<std::time::Instant>,
    pub(super) last_activity_heartbeat_at: &'a mut Option<std::time::Instant>,
}

mod guarded_save;
#[cfg(test)]
mod tests;
use guarded_save::{
    guarded_runtime_atomic_stamp, guarded_runtime_handoff_save,
    tmux_ready_state_dirty_after_guarded_save,
};

pub(super) async fn handle_runtime_handoff_loop_message(
    message: RuntimeHandoffLoopMessage,
    ctx: RuntimeHandoffLoopContext<'_>,
    state: RuntimeHandoffLoopState<'_>,
) -> RuntimeHandoffLoopOutcome {
    let retry_message = message.clone();
    let shared_owned = Arc::clone(ctx.shared_owned);
    let provider = ctx.provider.clone();
    let channel_id = ctx.channel_id;
    let done = ctx.done;
    let adk_session_name = ctx.adk_session_name;

    let mut terminal_control_ready_observed = *state.terminal_control_ready_observed;
    let mut tmux_last_offset = *state.tmux_last_offset;
    let inflight_state = &mut *state.inflight_state;
    let mut watcher_owner_channel_id = *state.watcher_owner_channel_id;
    let mut standby_relay_owns_output = *state.standby_relay_owns_output;
    let mut watcher_relay_available_for_turn = *state.watcher_relay_available_for_turn;
    let mut watcher_handoff_claim_outcome = *state.watcher_handoff_claim_outcome;
    let mut tmux_handed_off = *state.tmux_handed_off;
    let mut watcher_owns_assistant_relay = *state.watcher_owns_assistant_relay;
    let mut state_dirty = *state.state_dirty;
    let mut terminal_control_drain_until = *state.terminal_control_drain_until;
    let mut last_activity_heartbeat_at = *state.last_activity_heartbeat_at;
    let mut guarded_save_outcome = None;
    let pre_frame_inflight_state = inflight_state.clone();
    let pre_frame_terminal_control_ready_observed = terminal_control_ready_observed;
    let pre_frame_tmux_last_offset = tmux_last_offset;
    let pre_frame_watcher_owner_channel_id = watcher_owner_channel_id;
    let pre_frame_standby_relay_owns_output = standby_relay_owns_output;
    let pre_frame_watcher_relay_available_for_turn = watcher_relay_available_for_turn;
    let pre_frame_watcher_handoff_claim_outcome = watcher_handoff_claim_outcome;
    let pre_frame_tmux_handed_off = tmux_handed_off;
    let pre_frame_watcher_owns_assistant_relay = watcher_owns_assistant_relay;
    let pre_frame_state_dirty = state_dirty;
    let pre_frame_terminal_control_drain_until = terminal_control_drain_until;
    let pre_frame_last_activity_heartbeat_at = last_activity_heartbeat_at;

    match message {
        RuntimeHandoffLoopMessage::TmuxReady {
            output_path,
            input_fifo_path,
            tmux_session_name,
            last_offset,
        } => {
            terminal_control_ready_observed = true;
            let state_dirty_before_handoff = state_dirty;
            let mut tmux_ready_guarded_save_outcome;
            let tmux_ready_baseline = inflight_state.clone();
            let tmux_ready_expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(
                    &tmux_ready_baseline,
                );
            tmux_last_offset = Some(last_offset);
            inflight_state.runtime_kind = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
            inflight_state.output_path = Some(output_path.clone());
            inflight_state.input_fifo_path = Some(input_fifo_path).filter(|path| !path.is_empty());
            inflight_state.last_offset = last_offset;
            let _ = inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
            #[cfg(unix)]
            let relay_http_available = shared_owned.serenity_http_or_token_fallback().is_some();
            #[cfg(unix)]
            let on_standby = shared_owned.http.cached_serenity_ctx.get().is_none();
            #[cfg(unix)]
            let intended_relay_owner = if relay_http_available {
                if on_standby {
                    super::inflight::RelayOwnerKind::StandbyRelay
                } else {
                    super::inflight::RelayOwnerKind::Watcher
                }
            } else {
                super::inflight::RelayOwnerKind::None
            };
            #[cfg(not(unix))]
            let intended_relay_owner = super::inflight::RelayOwnerKind::None;
            inflight_state.set_relay_owner_kind(intended_relay_owner);
            tmux_ready_guarded_save_outcome = Some(guarded_runtime_handoff_save(
                &tmux_ready_baseline,
                inflight_state,
                &tmux_ready_expected,
                channel_id,
                "turn_bridge::runtime_handoff_loop::tmux_ready_admission",
            ));

            if matches!(
                tmux_ready_guarded_save_outcome,
                Some(crate::services::discord::inflight::GuardedSaveOutcome::Saved)
            ) {
                tmux_last_offset = Some(inflight_state.last_offset);
                #[cfg(not(unix))]
                {
                    watcher_owner_channel_id = ChannelId::new(
                        inflight_state
                            .watcher_owner_channel_id
                            .unwrap_or(channel_id.get()),
                    );
                }
                // #226: Atomic claim via try_claim_watcher. The durable runtime
                // admission above must succeed before this registry mutation.
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
                let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
                let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
                    super::tmux_watcher_now_ms(),
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
                let (watcher_claimed, watcher_claim_replaced_existing, owner_changed_after_claim) = {
                    // #1135: Reuse a live watcher for the same
                    // tmux session; replace only stale or
                    // different-session incumbents.
                    let claim = super::tmux::claim_or_reuse_watcher_with_thread_parent(
                        &shared_owned.tmux_watchers,
                        channel_id,
                        handle,
                        &provider,
                        "turn_bridge_tmux_ready",
                        super::tmux::thread_follow_up_parent_channel_id(
                            channel_id,
                            inflight_state.logical_channel_id,
                            inflight_state.thread_id,
                        ),
                    );
                    watcher_owner_channel_id = claim.owner_channel_id();
                    let owner_changed =
                        inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
                    (
                        claim.should_spawn(),
                        claim.replaced_existing(),
                        owner_changed,
                    )
                };
                #[cfg(not(unix))]
                let (watcher_claimed, watcher_claim_replaced_existing, owner_changed_after_claim) = {
                    let _ = handle;
                    (false, false, false)
                };
                if owner_changed_after_claim {
                    let claim_expected =
                        crate::services::discord::inflight::InflightTurnIdentity::from_state(
                            &pre_claim_persisted,
                        );
                    tmux_ready_guarded_save_outcome = Some(guarded_runtime_handoff_save(
                        &pre_claim_persisted,
                        inflight_state,
                        &claim_expected,
                        channel_id,
                        "turn_bridge::runtime_handoff_loop::tmux_ready_claim_owner",
                    ));
                }
                let claim_persisted = matches!(
                    tmux_ready_guarded_save_outcome,
                    Some(crate::services::discord::inflight::GuardedSaveOutcome::Saved)
                );
                if !claim_persisted && watcher_claimed {
                    cancel_provisional_watcher_claim_if_matches(
                        shared_owned.as_ref(),
                        watcher_owner_channel_id,
                        &cancel,
                    );
                }
                #[cfg(unix)]
                let mut watcher_ready_for_relay = !watcher_claimed && claim_persisted;
                #[cfg(not(unix))]
                let mut watcher_ready_for_relay = false;
                watcher_handoff_claim_outcome = if !claim_persisted {
                    WatcherHandoffClaimOutcome::None
                } else if watcher_claimed {
                    WatcherHandoffClaimOutcome::Spawned
                } else {
                    WatcherHandoffClaimOutcome::ReusedExisting
                };
                if watcher_claimed && claim_persisted {
                    #[cfg(unix)]
                    {
                        // Phase 5.3 of intake-node-routing
                        // (issue #2011): on cluster-standby nodes
                        // (no Discord gateway lease, no
                        // `cached_serenity_ctx`), bypass the tmux
                        // watcher entirely — its internal state
                        // machine has multiple gateway-coupled
                        // assumptions that prevent the relay step
                        // from firing on standby (verified
                        // 2026-05-10). Instead, leave
                        // `watcher_relay_available_for_turn=false`
                        // so the bridge delivers the response
                        // itself via
                        // `gateway.replace_message_with_outcome`
                        // after the producer's `Done` event
                        // populates `delivery_response`. The
                        // bridge's REST gateway path already uses
                        // `serenity_http_or_token_fallback()`
                        // (Phase 5.2) so the post lands on Discord
                        // even without the gateway runtime.
                        //
                        // Leader path is unchanged: when
                        // `cached_serenity_ctx` is set, spawn the
                        // watcher as before so streaming partial
                        // output continues to work.
                        let on_standby = shared_owned.http.cached_serenity_ctx.get().is_none();
                        if on_standby {
                            // Phase 5.3 of intake-node-routing (issue #2011):
                            // skip the watcher entirely on standby and
                            // spawn the standalone JSONL → Discord relay
                            // task instead. The watcher's leader-only
                            // state machine prevents its relay step from
                            // firing on standby nodes; bypassing it
                            // sidesteps an entire class of
                            // gateway-coupling bugs.
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] ⏭ standby relay: skipping tmux watcher spawn for channel {}; spawning JSONL→Discord standby_relay",
                                channel_id
                            );
                            // Drop the registered watcher slot so a
                            // subsequent turn does not falsely reuse
                            // a "live" watcher that we never spawned.
                            // Do NOT call `cancel.store(true)` on the
                            // returned handle: the inner cancel Arc
                            // is shared with the local `cancel` and
                            // would pre-cancel the standby_relay we
                            // are about to spawn (Codex P1 review on
                            // PR #2012). The cancel Arc is otherwise
                            // unused on this branch since no watcher
                            // task ever reads it.
                            let _ = shared_owned.tmux_watchers.remove(&watcher_owner_channel_id);
                            if let Some(http_for_standby) =
                                shared_owned.serenity_http_or_token_fallback()
                            {
                                let placeholder_msg_id_opt = if inflight_state.current_msg_id == 0 {
                                    None
                                } else {
                                    Some(serenity::MessageId::new(inflight_state.current_msg_id))
                                };
                                let output_path_for_standby = output_path.clone();
                                let turn_binding_for_standby =
                                    super::standby_relay::StandbyRelayTurnBinding::from_state(
                                        &inflight_state,
                                    );
                                // Use a fresh cancel Arc, independent
                                // from the watcher's `cancel` (which
                                // is shared via `handle.cancel`).
                                let cancel_for_standby =
                                    Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let shared_for_standby = shared_owned.clone();
                                let provider_for_standby = provider.clone();
                                super::task_supervisor::spawn_observed(
                                    "turn_bridge_runtime_standby_relay",
                                    super::standby_relay::run_standby_relay(
                                        http_for_standby,
                                        channel_id,
                                        placeholder_msg_id_opt,
                                        output_path_for_standby,
                                        turn_binding_for_standby.clone(),
                                        turn_binding_for_standby.polling_start_offset(last_offset),
                                        cancel_for_standby,
                                        shared_for_standby,
                                        provider_for_standby,
                                        // #2448: see TmuxReady branch
                                        // — timeout demoted to safety
                                        // backstop after broadcast
                                        // exit signal landed.
                                        std::time::Duration::from_secs(1800),
                                    ),
                                );
                                standby_relay_owns_output = true;
                                inflight_state.set_relay_owner_kind(
                                    super::inflight::RelayOwnerKind::StandbyRelay,
                                );
                                // #2263: see the helper-fn
                                // `handle_watcher_runtime_handoff`
                                // standby branch — intentionally
                                // leave `watcher_owns_live_relay = false`
                                // because the standby_relay task
                                // is not a tmux watcher, and the
                                // yield-gate flag would over-claim
                                // ownership for a watcher restored
                                // by a different node, risking
                                // duplicate Discord delivery.
                                // Per-turn delivery ownership is
                                // tracked both locally by
                                // `standby_relay_owns_output` and
                                // durably by `relay_owner_kind`.
                            } else {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] ⚠ standby relay skipped: no Http source for channel {}",
                                    channel_id
                                );
                            }
                            // Leave watcher_relay_available_for_turn=false
                            // and watcher_ready_for_relay=false so the
                            // bridge does NOT delegate to a non-existent
                            // watcher. The standby_relay task delivers
                            // the response independently.
                        } else if let Some(http_bg) = shared_owned.serenity_http_or_token_fallback()
                        {
                            let shared_bg = shared_owned.clone();
                            inflight_state
                                .set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
                            let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
                                &inflight_state,
                                &tmux_session_name,
                                true,
                            );
                            if let Ok(mut guard) = resume_offset.lock() {
                                *guard = Some(last_offset);
                            }
                            turn_delivered.store(false, Ordering::Relaxed);
                            if watcher_claim_replaced_existing {
                                shared_owned.record_tmux_watcher_reconnect(channel_id);
                            }
                            super::task_supervisor::spawn_observed_tmux_watcher(
                                "turn_bridge_runtime_tmux_output_watcher_with_restore",
                                shared_bg.clone(),
                                tmux_session_name.clone(),
                                cancel.clone(),
                                super::tmux::tmux_output_watcher_with_restore(
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
                            watcher_relay_available_for_turn = true;
                            watcher_ready_for_relay = true;
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ no Http source (neither cached_serenity_ctx nor cached_bot_token); tmux watcher not started for channel {}",
                                channel_id
                            );
                            if let Some((_, handle)) =
                                shared_owned.tmux_watchers.remove(&watcher_owner_channel_id)
                            {
                                handle.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
                if watcher_ready_for_relay {
                    tmux_handed_off = true;
                    inflight_state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
                    watcher_owns_assistant_relay = true;
                    if let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
                    {
                        watcher_relay_available_for_turn = true;
                        if let Ok(mut guard) = watcher.resume_offset.lock() {
                            *guard = Some(last_offset);
                        }
                        watcher.turn_delivered.store(false, Ordering::Relaxed);
                        // #1452 (Codex P1): publish the mailbox-finalization
                        // debt BEFORE unpausing the watcher.
                        //
                        // The watcher's terminal `swap(false, AcqRel)` runs
                        // as soon as it sees a Done event; if we delayed
                        // the store until the bridge's later delegation
                        // decision (line 2419+), the watcher could swap
                        // first, observe `false`, skip `mailbox_finish_turn`,
                        // and the bridge's late `store(true)` would leave
                        // stale debt that either keeps `cancel_token`
                        // permanently set OR is consumed by a future
                        // watcher event for the WRONG turn.
                        //
                        // #3016 phase-5b2: the legacy
                        // `mailbox_finalize_owed` store that used to
                        // publish "bridge will delegate finalization"
                        // here is removed; the `register_start` below
                        // (RelayOwnerKind::Watcher) is the ledger
                        // authority that replaced it.
                        // #3016 phase 3: register the turn with the
                        // single-authority finalizer BEFORE
                        // unpausing the watcher — same as the
                        // `handle_watcher_runtime_handoff` helper.
                        // This legacy `StreamMessage::TmuxReady`
                        // handoff does NOT go through that helper, so
                        // without this the watcher terminal would
                        // have no Watcher-owned ledger entry — and
                        // a busy-pane gate-timeout would finalize
                        // immediately instead of arming the
                        // deadline-backstop. Registering here with
                        // the same finalizer id makes it defer.
                        shared_owned
                        .turn_finalizer
                        .register_start_with_completion_admission(
                            super::turn_finalizer::TurnKey::new(
                                channel_id,
                                inflight_state.effective_finalizer_turn_id(),
                                shared_owned.restart.current_generation,
                            ),
                            provider.clone(),
                            super::inflight::RelayOwnerKind::Watcher,
                            super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                            // #3016 phase-5a: prime the reconcile cache
                            // at register time.
                            &shared_owned,
                        );
                        // #1452 (Codex iter 3 P1) / #3016 phase-5b2:
                        // unpause uses Release ordering so a watcher
                        // observing `paused = false` is guaranteed to
                        // also observe the prior writes — the
                        // `register_start` (RelayOwnerKind::Watcher)
                        // ledger entry that now drives the
                        // gate-timeout defer. With Relaxed ordering on
                        // a weakly-ordered platform the writes could
                        // be reordered, letting the watcher unpause
                        // and submit a terminal before the ledger
                        // knows the turn exists.
                        watcher.paused.store(false, Ordering::Release);
                    }
                }
            } else {
                watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
                watcher_relay_available_for_turn = false;
                tmux_handed_off = false;
                watcher_owns_assistant_relay = false;
            }
            // #4259 PR-2a (codex r1): this arm's mutations are queued for the
            // stream_tick BLIND dirty flush only while this turn still owns the
            // durable row — an unconditional `state_dirty = true` here let the
            // flush clobber a re-owned row with the stale snapshot right after
            // the guarded save had (correctly) skipped it.
            state_dirty = tmux_ready_state_dirty_after_guarded_save(
                state_dirty_before_handoff,
                tmux_ready_guarded_save_outcome,
            );
            guarded_save_outcome = tmux_ready_guarded_save_outcome;
            if done {
                terminal_control_drain_until = None;
            }
        }
        RuntimeHandoffLoopMessage::RuntimeReady { handoff } => {
            terminal_control_ready_observed = true;
            match handoff {
                RuntimeHandoff::LegacyTmuxWrapper {
                    output_path,
                    input_fifo_path,
                    tmux_session_name,
                    last_offset,
                } => {
                    guarded_save_outcome = Some(handle_watcher_runtime_handoff(
                        WatcherRuntimeHandoffContext {
                            shared_owned: &shared_owned,
                            provider: &provider,
                            channel_id,
                            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                            output_path,
                            input_fifo_path: Some(input_fifo_path),
                            tmux_session_name,
                            session_id: None,
                            last_offset,
                            done,
                        },
                        WatcherRuntimeHandoffState {
                            inflight_state,
                            tmux_last_offset: &mut tmux_last_offset,
                            watcher_owner_channel_id: &mut watcher_owner_channel_id,
                            standby_relay_owns_output: &mut standby_relay_owns_output,
                            watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
                            watcher_handoff_claim_outcome: &mut watcher_handoff_claim_outcome,
                            tmux_handed_off: &mut tmux_handed_off,
                            watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                            state_dirty: &mut state_dirty,
                            terminal_control_drain_until: &mut terminal_control_drain_until,
                        },
                    ));
                }
                RuntimeHandoff::ClaudeTui {
                    transcript_path,
                    tmux_session_name,
                    last_offset,
                } => {
                    guarded_save_outcome = Some(handle_watcher_runtime_handoff(
                        WatcherRuntimeHandoffContext {
                            shared_owned: &shared_owned,
                            provider: &provider,
                            channel_id,
                            runtime_kind: RuntimeHandoffKind::ClaudeTui,
                            output_path: transcript_path,
                            input_fifo_path: None,
                            tmux_session_name,
                            session_id: None,
                            last_offset,
                            done,
                        },
                        WatcherRuntimeHandoffState {
                            inflight_state,
                            tmux_last_offset: &mut tmux_last_offset,
                            watcher_owner_channel_id: &mut watcher_owner_channel_id,
                            standby_relay_owns_output: &mut standby_relay_owns_output,
                            watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
                            watcher_handoff_claim_outcome: &mut watcher_handoff_claim_outcome,
                            tmux_handed_off: &mut tmux_handed_off,
                            watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                            state_dirty: &mut state_dirty,
                            terminal_control_drain_until: &mut terminal_control_drain_until,
                        },
                    ));
                }
                RuntimeHandoff::CodexTui {
                    rollout_path,
                    thread_id,
                    tmux_session_name,
                    last_offset,
                } => {
                    guarded_save_outcome = Some(handle_watcher_runtime_handoff(
                        WatcherRuntimeHandoffContext {
                            shared_owned: &shared_owned,
                            provider: &provider,
                            channel_id,
                            runtime_kind: RuntimeHandoffKind::CodexTui,
                            output_path: rollout_path,
                            input_fifo_path: None,
                            tmux_session_name,
                            session_id: thread_id,
                            last_offset,
                            done,
                        },
                        WatcherRuntimeHandoffState {
                            inflight_state,
                            tmux_last_offset: &mut tmux_last_offset,
                            watcher_owner_channel_id: &mut watcher_owner_channel_id,
                            standby_relay_owns_output: &mut standby_relay_owns_output,
                            watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
                            watcher_handoff_claim_outcome: &mut watcher_handoff_claim_outcome,
                            tmux_handed_off: &mut tmux_handed_off,
                            watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                            state_dirty: &mut state_dirty,
                            terminal_control_drain_until: &mut terminal_control_drain_until,
                        },
                    ));
                }
                RuntimeHandoff::ProcessBackend {
                    output_path,
                    session_name,
                    last_offset,
                } => {
                    let persisted_baseline = inflight_state.clone();
                    let expected_identity =
                        crate::services::discord::inflight::InflightTurnIdentity::from_state(
                            &persisted_baseline,
                        );
                    tmux_last_offset = Some(last_offset);
                    inflight_state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
                    inflight_state.tmux_session_name = Some(session_name);
                    inflight_state.output_path = Some(output_path);
                    inflight_state.input_fifo_path = None;
                    inflight_state.last_offset = last_offset;
                    let outcome = guarded_runtime_atomic_stamp(
                        &persisted_baseline,
                        inflight_state,
                        &expected_identity,
                        channel_id,
                        "turn_bridge::runtime_handoff_loop::runtime_ready_process_backend",
                    );
                    if outcome == crate::services::discord::inflight::GuardedSaveOutcome::Saved {
                        tmux_last_offset = Some(inflight_state.last_offset);
                        watcher_owner_channel_id = ChannelId::new(
                            inflight_state
                                .watcher_owner_channel_id
                                .unwrap_or(channel_id.get()),
                        );
                    }
                    state_dirty =
                        tmux_ready_state_dirty_after_guarded_save(state_dirty, Some(outcome));
                    guarded_save_outcome = Some(outcome);
                    if done {
                        terminal_control_drain_until = None;
                    }
                }
                RuntimeHandoff::ClaudeEAdapter {
                    output_path,
                    session_name,
                    last_offset,
                    pid,
                } => {
                    // Phase 1 of the claude-e rollout (see
                    // `docs/claude-e-rollout/`). The adapter
                    // is a per-turn PTY spawn — no tmux pane
                    // backs it, so `tmux_session_name` must
                    // stay `None` to satisfy the
                    // `inflight_tmux_one_to_one` invariant
                    // when a channel switches between TUI
                    // and claude-e. `session_name` is the
                    // logical adapter id (Claude session uuid
                    // or `claude-e-{pid}`); it does not map
                    // to a tmux pane and is intentionally
                    // not stamped here.
                    let _ = session_name;
                    tmux_last_offset = Some(last_offset);
                    let (next_state_dirty, outcome) = claude_e::stamp_process_evidence(
                        inflight_state,
                        output_path,
                        last_offset,
                        pid,
                        state_dirty,
                    );
                    state_dirty = next_state_dirty;
                    guarded_save_outcome = Some(outcome);
                    if outcome == crate::services::discord::inflight::GuardedSaveOutcome::Saved {
                        tmux_last_offset = Some(inflight_state.last_offset);
                    }
                    if done {
                        terminal_control_drain_until = None;
                    }
                }
            }
        }
        RuntimeHandoffLoopMessage::ProcessReady {
            output_path,
            session_name,
            last_offset,
        } => {
            terminal_control_ready_observed = true;
            // ProcessBackend completed first turn.
            // No tmux watcher needed — process sessions are monitored
            // inline via SessionProbe::process during read_output_file_until_result.
            // Do NOT set tmux_handed_off: ProcessBackend has no watcher,
            // so the handoff cleanup path would delete the placeholder
            // with no one to send the final response.
            let persisted_baseline = inflight_state.clone();
            let expected_identity =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(
                    &persisted_baseline,
                );
            tmux_last_offset = Some(last_offset);
            inflight_state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            inflight_state.tmux_session_name = Some(session_name);
            inflight_state.output_path = Some(output_path);
            inflight_state.input_fifo_path = None;
            inflight_state.last_offset = last_offset;
            let outcome = guarded_runtime_atomic_stamp(
                &persisted_baseline,
                inflight_state,
                &expected_identity,
                channel_id,
                "turn_bridge::runtime_handoff_loop::process_ready",
            );
            if outcome == crate::services::discord::inflight::GuardedSaveOutcome::Saved {
                tmux_last_offset = Some(inflight_state.last_offset);
                watcher_owner_channel_id = ChannelId::new(
                    inflight_state
                        .watcher_owner_channel_id
                        .unwrap_or(channel_id.get()),
                );
            }
            state_dirty = tmux_ready_state_dirty_after_guarded_save(state_dirty, Some(outcome));
            guarded_save_outcome = Some(outcome);
            if done {
                terminal_control_drain_until = None;
            }
        }
        RuntimeHandoffLoopMessage::OutputOffset { offset } => {
            tmux_last_offset = Some(offset);
            inflight_state.last_offset = offset;
            maybe_refresh_active_turn_activity_heartbeat(
                shared_owned.as_ref(),
                &provider,
                &inflight_state,
                adk_session_name.as_deref(),
                &mut last_activity_heartbeat_at,
            );
            state_dirty = true;
        }
    }

    if matches!(
        guarded_save_outcome,
        Some(crate::services::discord::inflight::GuardedSaveOutcome::IoError)
    ) {
        // The exact handoff frame is the retry unit. Roll back every local
        // projection mutated while attempting it, not only the persisted row:
        // CodexTui stamps session_id before watcher admission and the second
        // watcher-owner CAS mutates a detached owner channel after claiming.
        // Retrying either partial projection could reuse cancelled authority.
        // A later phase can fail after an earlier stamp already committed.
        // Keep that exact adopted durable checkpoint so the retained frame's
        // identity matches on retry; when no generation advanced, restore the
        // true pre-frame row so no unsaved pre-admission mutation (including
        // CodexTui session_id) leaks into the retained retry.
        if inflight_state.save_generation == pre_frame_inflight_state.save_generation {
            inflight_state.clone_from(&pre_frame_inflight_state);
        }
        terminal_control_ready_observed = pre_frame_terminal_control_ready_observed;
        tmux_last_offset = pre_frame_tmux_last_offset;
        watcher_owner_channel_id = pre_frame_watcher_owner_channel_id;
        standby_relay_owns_output = pre_frame_standby_relay_owns_output;
        watcher_relay_available_for_turn = pre_frame_watcher_relay_available_for_turn;
        watcher_handoff_claim_outcome = pre_frame_watcher_handoff_claim_outcome;
        tmux_handed_off = pre_frame_tmux_handed_off;
        watcher_owns_assistant_relay = pre_frame_watcher_owns_assistant_relay;
        state_dirty = pre_frame_state_dirty;
        terminal_control_drain_until = pre_frame_terminal_control_drain_until;
        last_activity_heartbeat_at = pre_frame_last_activity_heartbeat_at;
    }

    *state.terminal_control_ready_observed = terminal_control_ready_observed;
    *state.tmux_last_offset = tmux_last_offset;
    *state.watcher_owner_channel_id = watcher_owner_channel_id;
    *state.standby_relay_owns_output = standby_relay_owns_output;
    *state.watcher_relay_available_for_turn = watcher_relay_available_for_turn;
    *state.watcher_handoff_claim_outcome = watcher_handoff_claim_outcome;
    *state.tmux_handed_off = tmux_handed_off;
    *state.watcher_owns_assistant_relay = watcher_owns_assistant_relay;
    *state.state_dirty = state_dirty;
    *state.terminal_control_drain_until = terminal_control_drain_until;
    *state.last_activity_heartbeat_at = last_activity_heartbeat_at;

    RuntimeHandoffLoopOutcome {
        retry_message: matches!(
            guarded_save_outcome,
            Some(crate::services::discord::inflight::GuardedSaveOutcome::IoError)
        )
        .then_some(retry_message),
        guarded_save_outcome,
    }
}
