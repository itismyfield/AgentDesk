use super::*;
use crate::services::discord::TmuxRelayCoord;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// #4229 S7: committed-completion finalize tail of the watcher loop (TUI history
/// push / commit tombstone+drain / guarded inflight clear + lifecycle events /
/// restored watcher finalize / direct-terminal idle commit / queue kickoff /
/// terminal stop decision), moved verbatim from tmux_watcher.rs L3746-4115.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalCommitEpilogueOutcome {
    BreakWatcherLoop,
    Fallthrough,
}

pub(super) struct TerminalCommitEpilogueContext<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) provider_kind: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) output_path: &'a String,
    pub(super) relay_coord: &'a Arc<TmuxRelayCoord>,
    pub(super) turn_delivered: &'a Arc<AtomicBool>,
}

pub(super) struct TerminalCommitEpilogueLocals<'a> {
    pub(super) terminal_output_committed: bool,
    pub(super) lifecycle_stage_paused: bool,
    pub(super) relay_suppressed: bool,
    pub(super) has_assistant_response: bool,
    pub(super) completion_is_stale_for_newer_turn: bool,
    pub(super) anchor_cleanup_is_stale_for_newer_turn: bool,
    pub(super) inflight_state: &'a Option<InflightTurnState>,
    pub(super) inflight_before_relay: &'a Option<InflightTurnState>,
    pub(super) full_response: &'a String,
    pub(super) watcher_turn_nonce: &'a Option<String>,
    pub(super) resolved_did: &'a Option<String>,
    pub(super) dispatch_ok: bool,
    pub(super) terminal_delivery_committed: bool,
    pub(super) watcher_tui_gate_outcome: TuiCompletionGateOutcome,
    pub(super) tui_direct_anchor_terminal_body_visible: bool,
    pub(super) terminal_kind: Option<WatcherTerminalKind>,
    pub(super) terminal_evidence_offset: Option<u64>,
    pub(super) finish_mailbox_on_completion: bool,
    pub(super) pre_panel_release_drove_finalize: bool,
    pub(super) current_offset: u64,
    pub(super) data_start_offset: u64,
}

pub(super) struct TerminalCommitEpilogueState<'a> {
    pub(super) turn_result_relayed: &'a mut bool,
    pub(super) watcher_direct_terminal_idle_committed: &'a mut bool,
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
}

/// #4370 (codex r3 #1): may THIS terminal-commit pass stamp the re-adopted
/// mailbox ledger entry FINISHED?
///
/// Only when the pass is not flushing an older turn's trailing output while a
/// NEWER turn owns `inflight_state`. Both staleness predicates are required, and
/// the id-0-inclusive one is the load-bearing half: `completion_is_stale_for_newer_turn`
/// returns FALSE for a newer turn whose `user_msg_id == 0` (an injected /
/// task-notification turn), and such a turn can be re-adopted across a restart
/// and therefore own a ledger entry. Stamping it FINISHED while it is still
/// producing output would let a later absent-row aged reclaim steal it.
///
/// `anchor_cleanup_is_stale_for_newer_turn` (#3142) is the id-0-inclusive sibling
/// used by the committed-output anchor-cleanup branch; the FINISHED stamp is the
/// same kind of branch and takes the same guard.
pub(super) fn readopted_finish_mark_allowed(
    completion_is_stale_for_newer_turn: bool,
    anchor_cleanup_is_stale_for_newer_turn: bool,
) -> bool {
    !completion_is_stale_for_newer_turn && !anchor_cleanup_is_stale_for_newer_turn
}

pub(super) async fn run_terminal_commit_epilogue(
    context: &TerminalCommitEpilogueContext<'_>,
    locals: TerminalCommitEpilogueLocals<'_>,
    state: &mut TerminalCommitEpilogueState<'_>,
) -> TerminalCommitEpilogueOutcome {
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let provider_kind = context.provider_kind;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let relay_coord = context.relay_coord;
    let turn_delivered = context.turn_delivered;
    let TerminalCommitEpilogueLocals {
        terminal_output_committed,
        lifecycle_stage_paused,
        relay_suppressed,
        has_assistant_response,
        completion_is_stale_for_newer_turn,
        anchor_cleanup_is_stale_for_newer_turn,
        inflight_state,
        inflight_before_relay,
        full_response,
        watcher_turn_nonce,
        resolved_did,
        dispatch_ok,
        terminal_delivery_committed,
        watcher_tui_gate_outcome,
        tui_direct_anchor_terminal_body_visible,
        terminal_kind,
        terminal_evidence_offset,
        finish_mailbox_on_completion,
        pre_panel_release_drove_finalize,
        current_offset,
        data_start_offset,
    } = locals;
    let turn_result_relayed = &mut *state.turn_result_relayed;
    let watcher_direct_terminal_idle_committed = &mut *state.watcher_direct_terminal_idle_committed;
    let monitor_auto_turn_claimed = &mut *state.monitor_auto_turn_claimed;
    let monitor_auto_turn_finished = &mut *state.monitor_auto_turn_finished;
    let monitor_auto_turn_synthetic_msg_id = &mut *state.monitor_auto_turn_synthetic_msg_id;
    let monitor_auto_turn_ledger_generation = &mut *state.monitor_auto_turn_ledger_generation;
    // #225 P1-2 / #1708 follow-up: clear inflight when the terminal output
    // was either delivered to Discord or intentionally suppressed as an
    // internal task notification. Only genuine delivery failure preserves
    // retry/handoff state for next startup.
    //
    // #2293 H2 — skip the entire block on `lifecycle_stage_paused`. Wiping
    // inflight + releasing the mailbox cancel_token while the pane is
    // still busy is exactly the cascade the issue is filed against: the
    // intake gate would see an empty inflight and a free mailbox and
    // admit a new turn into a non-quiescent pane. The next watcher pass
    // re-evaluates the gate and finishes the cleanup once the pane
    // actually reports idle.
    if terminal_output_committed && !lifecycle_stage_paused {
        // #3142: gate the TUI history push on `!completion_is_stale_for_newer_turn`.
        // When stale, the late `inflight_state.user_text` is the NEWER turn's
        // prompt; pairing it with the OLDER `full_response` would poison the
        // TUI history. The newer turn pushes its own (user_text, response) pair
        // on its own completion pass. Only the push is suppressed —
        // `turn_result_relayed` and the clear/finalize bookkeeping below keep
        // their own (already-shipped) stale gates. FALSE in every normal case.
        // #3142: gate on BOTH the id!=0 stale helper AND the id==0-inclusive
        // `anchor_cleanup_is_stale_for_newer_turn` (computed above) so a NEWER
        // external-input turn with `user_msg_id == 0` (no own dispatch id,
        // `rebind_origin == false`, populated `user_text`) cannot cross-pair
        // its `user_text` with the OLDER `full_response` in the TUI history.
        if has_assistant_response
            && !completion_is_stale_for_newer_turn
            && !anchor_cleanup_is_stale_for_newer_turn
            && let Some(state) = inflight_state.as_ref().filter(|state| !state.rebind_origin)
        {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if !session.cleared {
                    session.history.push(crate::ui::ai_screen::HistoryItem {
                        item_type: crate::ui::ai_screen::HistoryType::User,
                        content: state.user_text.clone(),
                    });
                    session.history.push(crate::ui::ai_screen::HistoryItem {
                        item_type: crate::ui::ai_screen::HistoryType::Assistant,
                        content: full_response.clone(),
                    });
                }
            }
            drop(data);
        }
        *turn_result_relayed = true;
        // #1670/#1708: always consume the handoff debt and clear inflight
        // when terminal output was committed — the bridge's
        // `bridge_relay_delegated_to_watcher` arm saves inflight and never
        // returns to clear it even if dispatch finalization fails (a stale
        // fallback dispatch_id with `dispatch_ok = false` used to orphan
        // the inflight + cancel_token forever). Decoupling rule: clear +
        // `finish_restored_watcher_active_turn` fire on every committed
        // terminal (idempotent under bridge/watcher concurrency), while
        // dispatch-lifecycle side-effects (queue kickoff, followup,
        // terminal-stop) stay gated on `dispatch_ok` below.
        // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag is
        // removed — exactly-once is the ledger phase gate's job.
        // #3016 (codex R3): do NOT delete on-disk inflight owned by a
        // NEWER follow-up turn — the same offset decision that zeroes
        // `pinned_finalizer_turn_id` below gates this clear, so a
        // stale-range pass cannot wipe it. Only the on-disk file is gated;
        // the in-memory `inflight_state` and `cleared_by_watcher` keep
        // their semantics.
        // #3296 codex r2: aborted-anchor reconcile, sited BEFORE the row
        // clear — tombstone evidence (committed turn identity) lands first,
        // then the drain covers, then the clear; a sweep claiming a marker
        // mid-commit sees "no live row" only AFTER the tombstone is durable,
        // so its 대조 lands ✅ not ⚠ (r2 finding 1). An ABORT recording its
        // marker after this drain still converges via the tombstone 대조.
        // #3350 issue-1: ALSO tombstone+drain body-INVISIBLE commits of
        // watcher-owned synthetic rows (suppressed task-notification
        // completions) — their `⏳ → ✅` block fires regardless, and skipping
        // here left their own-pin marker to a false TTL `⚠`.
        let pinned_committed_clear_identity = if !completion_is_stale_for_newer_turn {
            inflight_state
                .as_ref()
                .map(crate::services::discord::inflight::InflightTurnIdentity::from_state)
        } else {
            None
        };
        let pinned_committed_clear_turn_nonce = if !completion_is_stale_for_newer_turn {
            watcher_turn_nonce.as_deref()
        } else {
            None
        };

        if !completion_is_stale_for_newer_turn
            && let Some(committed) = inflight_state.as_ref()
            && (tui_direct_anchor_terminal_body_visible
                || committed_row_requires_marker_tombstone(committed))
        {
            crate::services::discord::tui_direct_abort_marker::record_commit_tombstone_with_offsets(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
                committed.user_msg_id,
                &committed.started_at,
                committed.turn_start_offset,
                terminal_evidence_offset,
            );
            let _ =
                    crate::services::discord::tui_direct_abort_marker::drain_on_terminal_commit_with_offsets(
                        &shared,
                        watcher_provider.as_str(),
                        &tmux_session_name,
                        channel_id.get(),
                        committed.user_msg_id,
                        &committed.started_at,
                        committed.turn_start_offset,
                        terminal_evidence_offset,
                    )
                    .await;
        }
        if !completion_is_stale_for_newer_turn {
            if let Some(pinned_clear_identity) = pinned_committed_clear_identity.as_ref() {
                let clear_outcome =
                        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
                            &provider_kind,
                            channel_id.get(),
                            pinned_clear_identity,
                            pinned_committed_clear_turn_nonce,
                        );
                match clear_outcome {
                    crate::services::discord::inflight::GuardedClearOutcome::Cleared => {
                        let watcher_turn_id = inflight_state
                            .as_ref()
                            .filter(|s| s.user_msg_id != 0)
                            .map(|s| format!("discord:{}:{}", s.channel_id, s.user_msg_id));
                        let watcher_session_key_owned =
                            inflight_state.as_ref().and_then(|s| s.session_key.clone());
                        let watcher_dispatch_id_owned = resolved_did.clone().or_else(|| {
                            inflight_state.as_ref().and_then(|s| s.dispatch_id.clone())
                        });
                        crate::services::observability::emit_inflight_lifecycle_event(
                            provider_kind.as_str(),
                            channel_id.get(),
                            watcher_dispatch_id_owned.as_deref(),
                            watcher_session_key_owned.as_deref(),
                            watcher_turn_id.as_deref(),
                            "cleared_by_watcher",
                            serde_json::json!({
                                "dispatch_ok": dispatch_ok,
                                "has_assistant_response": has_assistant_response,
                                "full_response_len": full_response.len(),
                            }),
                        );
                        // #3646 OBSERVATION-ONLY (event 3/3 — inflight_clear + invariant
                        // signal): the committed-output, non-stale watcher clear — the exact
                        // #3607 chokepoint. The clear ABOVE has already run; this only
                        // RECORDS the live lifecycle signals and fires a NON-FATAL
                        // ERROR-level invariant signal if a committed terminal was cleared
                        // with neither a visible UI completion nor a persisted obligation
                        // (#3607). Never gates cleanup. `terminal_ui_obligation_persisted` is
                        // `false` on the watcher path. S2-b removed completion-gate
                        // suppression, so a busy pane observation no longer suppresses this
                        // clear. Orchestration + the non-fatal invariant live in
                        // relay_owner_observability (non-hot file).
                        crate::services::discord::relay_owner_observability::emit_inflight_clear_with_invariant(
                                provider_kind.as_str(),
                                channel_id.get(),
                                watcher_dispatch_id_owned.as_deref(),
                                watcher_session_key_owned.as_deref(),
                                watcher_turn_id.as_deref(),
                                terminal_delivery_committed,
                                terminal_output_committed
                                    && watcher_tui_gate_outcome.should_emit_completion(),
                                false,
                            );
                    }
                    crate::services::discord::inflight::GuardedClearOutcome::IoError => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            clear_outcome = ?clear_outcome,
                            "  [{ts}] ⚠ watcher committed-output clear for {tmux_session_name}: atomic identity-matched clear failed with IO error at offset {current_offset}; see preceding inflight guarded-clear error detail"
                        );
                    }
                    other => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 watcher committed-output clear for {tmux_session_name}: atomic identity-matched clear was a no-op (outcome={other:?}) at offset {current_offset} — on-disk inflight is no longer the pinned committed turn"
                        );
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 watcher committed-output clear for {tmux_session_name}: no pinned committed-turn identity at offset {current_offset}; skipping the on-disk clear"
                );
            }
        }
        // #4370 R3-1: this pass just committed the terminal delivery for the
        // pinned turn and cleared its on-disk row above (the row-ABSENT "Path B"
        // shape). Stamp the re-adopted-mailbox ledger entry FINISHED so a later
        // starved synthetic relay turn can reclaim the stuck mailbox on a POSITIVE
        // liveness signal instead of the unenforced "absent row => not live"
        // assumption. Runs only inside the terminal-commit pass
        // (`terminal_output_committed && !lifecycle_stage_paused`), and is a no-op
        // unless THIS exact turn was re-adopted from inflight (the ledger has no
        // entry otherwise, and the mark requires an exact `(owner, id)` match).
        //
        // #4370 codex r3 #1 — the staleness guard MUST be the id-0-inclusive one.
        // `completion_is_stale_for_newer_turn` deliberately ignores a newer turn
        // whose `user_msg_id == 0` (`turn_identity.rs`
        // `committed_completion_is_stale_for_newer_turn`). An injected /
        // task-notification turn is exactly that shape, it CAN be re-adopted across
        // a restart, and it therefore CAN own a ledger entry. Gating only on the
        // id!=0 predicate let a pass that is merely flushing an OLDER turn's
        // trailing output stamp FINISHED on that still-producing id-0 turn — after
        // which an absent-row aged reclaim would steal it, losing its prose and
        // suppressing its footer. That is the very bug class this PR fixes, in
        // reverse. `anchor_cleanup_is_stale_for_newer_turn` is the id-0-inclusive
        // sibling (#3142) already used by the anchor-cleanup branch above; the
        // FINISHED stamp is the same kind of committed-output branch, so it takes
        // the same guard.
        if readopted_finish_mark_allowed(
            completion_is_stale_for_newer_turn,
            anchor_cleanup_is_stale_for_newer_turn,
        ) && let Some(committed) = inflight_state.as_ref()
        {
            shared.mark_readopted_mailbox_owner_finished_for_episode(
                provider_kind,
                channel_id.get(),
                committed.request_owner_user_id,
                committed.effective_finalizer_turn_id(),
                committed,
            );
        }
        // codex P2 (#1670): cleanup (mailbox_finish_turn + cancel_token
        // release) MUST run on every relay-completed terminal even when
        // `dispatch_ok = false` (else organic turns leak forever), but the
        // queue-kickoff side-effect stays gated on `dispatch_ok`. The redundant
        // `should_kickoff_queue` block further below is also `dispatch_ok`-gated
        // as a fallback for paths where the helper short-circuited.
        // #3016/#3645: derive the finalizer id from the TURN-PINNED pre-relay
        // snapshot, with `pinned_finalizer_turn_id` mirroring the output-range
        // guard so newer same-session follow-ups yield 0 and skip destructive
        // completion side effects consistently.
        let restored_finalizer_turn_id = pinned_finalizer_turn_id(
            inflight_before_relay.as_ref(),
            &tmux_session_name,
            current_offset,
        );
        // #3016 (codex B1): SKIP the normal-completion finalize ENTIRELY in the
        // stale-newer-turn case — do NOT call it with a channel-only id 0.
        // A 0-id submit is unsafe, not a no-op: `normal_completion = true`
        // finalizes UNCONDITIONALLY, and a 0-id `TurnKey` reaches
        // `resolve_channel_only` (turn_finalizer.rs:161-181) which collapses onto
        // the single live non-finalized entry. In the stale case the OLD turn
        // (whose trailing output this is) already finalized, so the only live
        // entry is the NEWER still-running turn — a 0-id Complete would
        // wrong-finalize it, releasing its cancel_token/ledger mid-flight. Finalize
        // NOTHING here; the newer turn finalizes itself when ITS output commits in
        // a later loop iteration. `completion_is_stale_for_newer_turn` is the exact
        // complement of the `< current_offset` range test in
        // `pinned_finalizer_turn_id`; a same-session snapshot whose pinned id
        // resolves to 0 is skipped rather than submitted channel-only.
        //
        // Skip-path bookkeeping: `watcher_drove_finalize = false`. #3016
        // phase-5b2: the legacy `mailbox_finalize_owed` flag is removed — the
        // newer turn finalizes via its own real-id path, and the stale-skip is
        // already kickoff-suppressed by `has_active_turn` (the newer live turn).
        let watcher_drove_finalize = if should_submit_restored_watcher_finalize(
            completion_is_stale_for_newer_turn,
            restored_finalizer_turn_id,
        ) {
            finish_restored_watcher_active_turn_with_ctx(
                    &shared,
                    &provider_kind,
                    channel_id,
                    restored_finalizer_turn_id,
                    finish_mailbox_on_completion,
                    // #3016 option A: terminal output was committed above
                    // (`terminal_output_committed && !lifecycle_stage_paused`), the
                    // canonical *normal completion* point. Finalize unconditionally —
                    // independent of `finish_mailbox_on_completion` — so the normal
                    // live bridge→watcher delegation turn no longer depends on the
                    // legacy `mailbox_finalize_owed` flag (removed in #3016
                    // phase-5b2). The finalizer is idempotent (bridge winner →
                    // AlreadyFinalized here), so this cannot over-finalize.
                    true,
                    dispatch_ok,
                    // #3350 codex r1-1: inflight was cleared above — carry the
                    // pre-relay snapshot (the same row `restored_user_msg_id` was
                    // pinned from) for the finalize-time marker ensure.
                    inflight_before_relay.as_ref().map(
                        crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row,
                    ),
                    // #4106: when the pre-panel early release already drove the
                    // release of this pinned id, the late submit here is a
                    // deterministic identity-guard miss; route it through the
                    // guard-miss-expected context so the EXPECTED no-op logs at
                    // debug instead of the wrong-turn WARN. When the early release
                    // did NOT drive it, keep the plain watcher() context so a
                    // genuine wrong-turn miss still WARNs.
                    if pre_panel_release_drove_finalize {
                        crate::services::discord::turn_finalizer::FinalizeContext::watcher_after_pre_panel_release()
                    } else {
                        crate::services::discord::turn_finalizer::FinalizeContext::watcher()
                    },
                    "restored watcher completed with queued backlog",
                )
                .await
        } else {
            // Stale-newer-turn: finalize skipped (see above). The watcher did
            // not drive any finalize on this pass.
            false
        };
        if !*watcher_direct_terminal_idle_committed {
            *watcher_direct_terminal_idle_committed = commit_watcher_direct_terminal_session_idle(
                &shared,
                &provider_kind,
                channel_id,
                &tmux_session_name,
                terminal_kind,
                data_start_offset,
                current_offset,
            )
            .await;
        }
        let mailbox = shared.mailbox(channel_id);
        let has_active_turn = mailbox.has_active_turn().await;
        // #3016 (codex R1) / phase-5b2: couple the post-finalize lifecycle to
        // the ACTUAL finalize. `watcher_drove_finalize` is true whenever the
        // helper ran the finalizer (here always, via `normal_completion = true`),
        // so kickoff-suppression and the terminal-stop path below account for it.
        // The legacy `mailbox_finalize_owed`-derived `delegated_finalize_owed`
        // term is dropped: the only false path (stale-newer-turn skip) has a newer
        // live turn, so `has_active_turn` already suppresses kickoff — identical.
        let watcher_handled_mailbox_finish = watcher_drove_finalize || finish_mailbox_on_completion;
        let should_kickoff_queue =
            if watcher_handled_mailbox_finish || *monitor_auto_turn_finished || has_active_turn {
                false
            } else {
                mailbox
                    .has_pending_soft_queue(crate::services::discord::queue_persistence_context(
                        &shared,
                        &provider_kind,
                        channel_id,
                    ))
                    .await
                    .has_pending
            };
        if dispatch_ok && should_kickoff_queue {
            crate::services::discord::schedule_deferred_idle_queue_kickoff(
                shared.clone(),
                provider_kind.clone(),
                channel_id,
                "watcher completed with queued backlog",
            );
        }
        if is_terminal_finalize_stop_candidate(
            terminal_output_committed,
            dispatch_ok,
            watcher_handled_mailbox_finish,
        ) {
            let tmux_alive = probe_tmux_session_liveness(&tmux_session_name).await;
            let confirmed_end = relay_coord.confirmed_end_offset.load(Ordering::Acquire);
            let tmux_tail_offset = std::fs::metadata(&output_path)
                .map(|meta| meta.len())
                .unwrap_or(current_offset);
            match watcher_stop_decision_after_terminal_finalize(
                terminal_output_committed,
                dispatch_ok,
                watcher_handled_mailbox_finish,
                tmux_alive,
                confirmed_end,
                tmux_tail_offset,
                None,
            ) {
                WatcherStopDecision::Stop => {
                    turn_delivered.store(true, Ordering::Release);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 watcher: terminal turn finalized; stopping watcher for {} after tmux exit",
                        tmux_session_name
                    );
                    finish_monitor_auto_turn_if_claimed(
                        &shared,
                        &watcher_provider,
                        channel_id,
                        monitor_auto_turn_claimed,
                        monitor_auto_turn_finished,
                        monitor_auto_turn_synthetic_msg_id,
                        monitor_auto_turn_ledger_generation,
                    )
                    .await;
                    return TerminalCommitEpilogueOutcome::BreakWatcherLoop;
                }
                WatcherStopDecision::Continue
                | WatcherStopDecision::PostTerminalSuccessContinuation => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 watcher: terminal turn finalized but tmux is still alive for {}; watcher staying attached",
                        tmux_session_name
                    );
                }
            }
        }
    } else if !relay_suppressed {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ watcher: relay failed — preserving inflight for retry");
    }
    TerminalCommitEpilogueOutcome::Fallthrough
}
