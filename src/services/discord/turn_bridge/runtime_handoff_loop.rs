//! Runtime handoff stream-loop arms for `turn_bridge::spawn_turn_bridge`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind};

use super::*;

mod claude_e;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeHandoffLoopOutcome {
    ContinueDraining,
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

struct WatcherRuntimeHandoffContext<'a> {
    shared_owned: &'a Arc<SharedData>,
    provider: &'a ProviderKind,
    channel_id: ChannelId,
    runtime_kind: RuntimeHandoffKind,
    output_path: String,
    input_fifo_path: Option<String>,
    tmux_session_name: String,
    last_offset: u64,
    done: bool,
}

struct WatcherRuntimeHandoffState<'a> {
    inflight_state: &'a mut InflightTurnState,
    tmux_last_offset: &'a mut Option<u64>,
    watcher_owner_channel_id: &'a mut ChannelId,
    standby_relay_owns_output: &'a mut bool,
    watcher_relay_available_for_turn: &'a mut bool,
    watcher_handoff_claim_outcome: &'a mut WatcherHandoffClaimOutcome,
    tmux_handed_off: &'a mut bool,
    watcher_owns_assistant_relay: &'a mut bool,
    state_dirty: &'a mut bool,
    terminal_control_drain_until: &'a mut Option<std::time::Instant>,
}

// #4259 PR-2a: the `TmuxReady` identity-guarded save + its outcome-conditional
// dirty policy live in a child module so this parent stays below the giant
// (>= 1000 prod LoC) threshold (codex r1).
mod guarded_save;
use guarded_save::{guarded_runtime_handoff_save, tmux_ready_state_dirty_after_guarded_save};

pub(super) async fn handle_runtime_handoff_loop_message(
    message: RuntimeHandoffLoopMessage,
    ctx: RuntimeHandoffLoopContext<'_>,
    state: RuntimeHandoffLoopState<'_>,
) -> RuntimeHandoffLoopOutcome {
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

    match message {
        RuntimeHandoffLoopMessage::TmuxReady {
            output_path,
            input_fifo_path,
            tmux_session_name,
            last_offset,
        } => {
            terminal_control_ready_observed = true;
            // #4259 PR-2a (codex r1): the LAST guarded-save outcome of this
            // arm; drives the outcome-conditional dirty marking at the arm's
            // tail so a skipped save is not undone by the stream_tick blind
            // dirty flush re-writing the stale snapshot.
            let mut tmux_ready_guarded_save_outcome = None;
            tmux_last_offset = Some(last_offset);
            inflight_state.runtime_kind = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
            inflight_state.output_path = Some(output_path.clone());
            inflight_state.input_fifo_path = Some(input_fifo_path).filter(|path| !path.is_empty());
            inflight_state.last_offset = last_offset;

            // #226: Atomic claim via try_claim_watcher
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
            #[cfg(unix)]
            let (watcher_claimed, watcher_claim_replaced_existing) = {
                // #1135: Reuse a live watcher for the same
                // tmux session; replace only stale or
                // different-session incumbents.
                let claim = super::tmux::claim_or_reuse_watcher(
                    &shared_owned.tmux_watchers,
                    channel_id,
                    handle,
                    &provider,
                    "turn_bridge_tmux_ready",
                );
                watcher_owner_channel_id = claim.owner_channel_id();
                let _ = inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
                (claim.should_spawn(), claim.replaced_existing())
            };
            #[cfg(not(unix))]
            let (watcher_claimed, watcher_claim_replaced_existing) = {
                let _ = handle;
                (false, false)
            };
            #[cfg(unix)]
            let mut watcher_ready_for_relay = !watcher_claimed;
            #[cfg(not(unix))]
            let mut watcher_ready_for_relay = false;
            watcher_handoff_claim_outcome = if watcher_claimed {
                WatcherHandoffClaimOutcome::Spawned
            } else {
                WatcherHandoffClaimOutcome::ReusedExisting
            };
            if watcher_claimed {
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
                            tmux_ready_guarded_save_outcome = Some(guarded_runtime_handoff_save(
                                &inflight_state,
                                channel_id,
                                "turn_bridge::runtime_handoff_loop::tmux_ready_standby_relay",
                            ));
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ standby relay skipped: no Http source for channel {}",
                                channel_id
                            );
                        }
                        // Leave watcher_relay_available_for_turn=false
                        // and watcher_ready_for_relay=false so the
                        // bridge does NOT delegate to a non-existent
                        // watcher. The standby_relay task delivers
                        // the response independently.
                    } else if let Some(http_bg) = shared_owned.serenity_http_or_token_fallback() {
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
                        tmux_ready_guarded_save_outcome = Some(guarded_runtime_handoff_save(
                            &inflight_state,
                            channel_id,
                            "turn_bridge::runtime_handoff_loop::tmux_ready_watcher_spawn",
                        ));
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
                tmux_ready_guarded_save_outcome = Some(guarded_runtime_handoff_save(
                    &inflight_state,
                    channel_id,
                    "turn_bridge::runtime_handoff_loop::tmux_ready_watcher_handoff",
                ));
                if let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id) {
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
                    shared_owned.turn_finalizer.register_start(
                        super::turn_finalizer::TurnKey::new(
                            channel_id,
                            inflight_state.effective_finalizer_turn_id(),
                            shared_owned.restart.current_generation,
                        ),
                        provider.clone(),
                        super::inflight::RelayOwnerKind::Watcher,
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
            // #4259 PR-2a (codex r1): this arm's mutations are queued for the
            // stream_tick BLIND dirty flush only while this turn still owns the
            // durable row — an unconditional `state_dirty = true` here let the
            // flush clobber a re-owned row with the stale snapshot right after
            // the guarded save had (correctly) skipped it.
            state_dirty = tmux_ready_state_dirty_after_guarded_save(
                state_dirty,
                tmux_ready_guarded_save_outcome,
            );
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
                    handle_watcher_runtime_handoff(
                        WatcherRuntimeHandoffContext {
                            shared_owned: &shared_owned,
                            provider: &provider,
                            channel_id,
                            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                            output_path,
                            input_fifo_path: Some(input_fifo_path),
                            tmux_session_name,
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
                    );
                }
                RuntimeHandoff::ClaudeTui {
                    transcript_path,
                    tmux_session_name,
                    last_offset,
                } => {
                    handle_watcher_runtime_handoff(
                        WatcherRuntimeHandoffContext {
                            shared_owned: &shared_owned,
                            provider: &provider,
                            channel_id,
                            runtime_kind: RuntimeHandoffKind::ClaudeTui,
                            output_path: transcript_path,
                            input_fifo_path: None,
                            tmux_session_name,
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
                    );
                }
                RuntimeHandoff::CodexTui {
                    rollout_path,
                    thread_id,
                    tmux_session_name,
                    last_offset,
                } => {
                    if let Some(thread_id) = thread_id {
                        inflight_state.session_id = Some(thread_id);
                    }
                    handle_watcher_runtime_handoff(
                        WatcherRuntimeHandoffContext {
                            shared_owned: &shared_owned,
                            provider: &provider,
                            channel_id,
                            runtime_kind: RuntimeHandoffKind::CodexTui,
                            output_path: rollout_path,
                            input_fifo_path: None,
                            tmux_session_name,
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
                    );
                }
                RuntimeHandoff::ProcessBackend {
                    output_path,
                    session_name,
                    last_offset,
                } => {
                    tmux_last_offset = Some(last_offset);
                    inflight_state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
                    inflight_state.tmux_session_name = Some(session_name);
                    inflight_state.output_path = Some(output_path);
                    inflight_state.input_fifo_path = None;
                    inflight_state.last_offset = last_offset;
                    state_dirty = true;
                    // #2235: see CodexTui arm — durable stamp of
                    // runtime_kind across a bridge-crash window.
                    // #4259 PR-2a: kept BLIND (held-back ratchet row) — this
                    // stamp rewrites identity-pinned `tmux_session_name` (plus
                    // the ProcessBackend `output_path`); even the
                    // `_allow_output_restamp` variant (codex r1) pins the
                    // 4-field identity, so convert only after verifying the
                    // session name is stable across the stamp.
                    let _ = save_inflight_state(&inflight_state);
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
                    state_dirty = claude_e::stamp_process_evidence(
                        inflight_state,
                        output_path,
                        last_offset,
                        pid,
                        state_dirty,
                    );
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
            tmux_last_offset = Some(last_offset);
            inflight_state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            inflight_state.tmux_session_name = Some(session_name);
            inflight_state.output_path = Some(output_path);
            inflight_state.input_fifo_path = None;
            inflight_state.last_offset = last_offset;
            state_dirty = true;
            // #2235: persist runtime_kind stamp immediately —
            // ProcessBackend has no watcher so we want the
            // on-disk row to reflect the new backend before
            // any potential bridge crash.
            // #4259 PR-2a: kept BLIND (held-back ratchet row) — rewrites
            // identity-pinned `tmux_session_name` (plus `output_path`); even
            // the `_allow_output_restamp` variant (codex r1) pins the 4-field
            // identity, so convert only after verifying session-name stability.
            let _ = save_inflight_state(&inflight_state);
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

    RuntimeHandoffLoopOutcome::ContinueDraining
}

fn handle_watcher_runtime_handoff(
    ctx: WatcherRuntimeHandoffContext<'_>,
    state: WatcherRuntimeHandoffState<'_>,
) {
    let shared_owned = ctx.shared_owned;
    let provider = ctx.provider;
    let channel_id = ctx.channel_id;
    let runtime_kind = ctx.runtime_kind;
    let output_path = ctx.output_path;
    let input_fifo_path = ctx.input_fifo_path;
    let tmux_session_name = ctx.tmux_session_name;
    let last_offset = ctx.last_offset;
    let done = ctx.done;
    let inflight_state = state.inflight_state;
    let tmux_last_offset = state.tmux_last_offset;
    let watcher_owner_channel_id = state.watcher_owner_channel_id;
    let standby_relay_owns_output = state.standby_relay_owns_output;
    let watcher_relay_available_for_turn = state.watcher_relay_available_for_turn;
    let watcher_handoff_claim_outcome = state.watcher_handoff_claim_outcome;
    let tmux_handed_off = state.tmux_handed_off;
    let watcher_owns_assistant_relay = state.watcher_owns_assistant_relay;
    let state_dirty = state.state_dirty;
    let terminal_control_drain_until = state.terminal_control_drain_until;

    *tmux_last_offset = Some(last_offset);
    inflight_state.runtime_kind = Some(runtime_kind);
    inflight_state.tmux_session_name = Some(tmux_session_name.clone());
    inflight_state.output_path = Some(output_path.clone());
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
    // #2235 NOTE: we deliberately do NOT durably save the row here.
    // `watcher_owns_live_relay` is still `false` at this point and only flips
    // to `true` after the watcher is successfully claimed and spawned (the
    // leader-branch path below). A save before that flag is set would leak a
    // v8 row with the new handoff shape alongside `watcher_owns_live_relay =
    // false`, which on restart would make the restored watcher yield to a
    // phantom bridge owner (codex adversarial review on #2235). The
    // existing branch-specific saves at the post-flag flip points plus the
    // centralized `state_dirty` flush already cover the durable-stamp
    // guarantee for watcher-owned RuntimeReady paths.
    //
    // #4259 PR-2a: the three branch-specific `save_inflight_state` calls in
    // this helper (standby, leader-watcher spawn, and the
    // `watcher_ready_for_relay` handoff) stay BLIND (held-back ratchet rows).
    // This helper rewrites identity-pinned `tmux_session_name` from the
    // handoff and serves ClaudeTui (transcript_path) / CodexTui (rollout_path)
    // / recovery+rebind LegacyTmuxWrapper flows; the `_allow_output_restamp`
    // variant (codex r1) tolerates only `output_path` drift, so converting
    // these needs per-flow verification that the session name is stable (or an
    // adoption-aware variant, cf. `save_existing_inflight_rebind_adoption_*`)
    // plus the TmuxReady-arm-style outcome-conditional `state_dirty` handling.
    //
    // #2263: the standby branch is INTENTIONALLY not covered by this
    // invariant — see the in-branch comment near the
    // `*standby_relay_owns_output = true` assignment for why the flag
    // stays `false` on standby and the trade-off vs duplicate delivery.

    // #226: Atomic claim via try_claim_watcher
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
    #[cfg(unix)]
    let (watcher_claimed, watcher_claim_replaced_existing) = {
        let claim = super::tmux::claim_or_reuse_watcher(
            &shared_owned.tmux_watchers,
            channel_id,
            handle,
            provider,
            "turn_bridge_runtime_ready",
        );
        *watcher_owner_channel_id = claim.owner_channel_id();
        *state_dirty |= inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
        (claim.should_spawn(), claim.replaced_existing())
    };
    #[cfg(not(unix))]
    let (watcher_claimed, watcher_claim_replaced_existing) = {
        let _ = handle;
        (false, false)
    };
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
                let _ = shared_owned.tmux_watchers.remove(watcher_owner_channel_id);
                if let Some(http_for_standby) = shared_owned.serenity_http_or_token_fallback() {
                    let placeholder_msg_id_opt = if inflight_state.current_msg_id == 0 {
                        None
                    } else {
                        Some(serenity::MessageId::new(inflight_state.current_msg_id))
                    };
                    let output_path_for_standby = output_path.clone();
                    let turn_binding_for_standby =
                        super::standby_relay::StandbyRelayTurnBinding::from_state(&inflight_state);
                    let cancel_for_standby = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let shared_for_standby = shared_owned.clone();
                    let provider_for_standby = provider.clone();
                    super::task_supervisor::spawn_observed(
                        "turn_bridge_standby_relay",
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
                        .set_relay_owner_kind(super::inflight::RelayOwnerKind::StandbyRelay);
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
                    let _ = save_inflight_state(inflight_state);
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ standby relay skipped: no Http source for channel {}",
                        channel_id
                    );
                }
            } else if let Some(http_bg) = shared_owned.serenity_http_or_token_fallback() {
                let shared_bg = shared_owned.clone();
                inflight_state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
                let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
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
                super::task_supervisor::spawn_observed_tmux_watcher(
                    "turn_bridge_tmux_output_watcher_with_restore",
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
                *watcher_relay_available_for_turn = true;
                let _ = save_inflight_state(inflight_state);
                watcher_ready_for_relay = true;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ no Http source (neither cached_serenity_ctx nor cached_bot_token); tmux watcher not started for channel {}",
                    channel_id
                );
                if let Some((_, handle)) =
                    shared_owned.tmux_watchers.remove(watcher_owner_channel_id)
                {
                    handle
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    if watcher_ready_for_relay {
        *tmux_handed_off = true;
        inflight_state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
        *watcher_owns_assistant_relay = true;
        let _ = save_inflight_state(inflight_state);
        if let Some(watcher) = shared_owned.tmux_watchers.get(watcher_owner_channel_id) {
            *watcher_relay_available_for_turn = true;
            if let Ok(mut guard) = watcher.resume_offset.lock() {
                *guard = Some(last_offset);
            }
            watcher
                .turn_delivered
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // #3016 phase 2: register the turn with the single-authority
            // finalizer BEFORE unpausing the watcher. Message arrival order in
            // the actor replaces the deleted Release/AcqRel ordering: the
            // ledger now knows the turn exists (with the watcher as relay
            // owner) before the watcher can submit its terminal. The ledger is
            // the authority that superseded the legacy `mailbox_finalize_owed`
            // flag (removed in #3016 phase-5b2) and the CAS revoke deleted from
            // the bridge finalize branches below.
            shared_owned.turn_finalizer.register_start(
                super::turn_finalizer::TurnKey::new(
                    channel_id,
                    inflight_state.effective_finalizer_turn_id(),
                    shared_owned.restart.current_generation,
                ),
                provider.clone(),
                super::inflight::RelayOwnerKind::Watcher,
                // #3016 phase-5a: prime the reconcile cache at register time.
                shared_owned,
            );
            watcher
                .paused
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }
    *state_dirty = true;
    if done {
        *terminal_control_drain_until = None;
    }
}
