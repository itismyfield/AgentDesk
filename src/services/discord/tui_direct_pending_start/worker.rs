//! Pending-start reclaim, cancellation, and worker execution.

use super::*;

pub(super) fn restart_orphan_pane_ready_for_input(
    provider: &crate::services::provider::ProviderKind,
    state: &super::super::inflight::InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(std::path::Path::new);
    let readiness = crate::services::tmux_turn_liveness::independent_tmux_readiness(
        tmux_session_name,
        provider,
        state.runtime_kind,
        output_path,
        Some(state.last_offset),
    );
    restart_orphan_independent_pane_ready(state, readiness)
}

pub(super) async fn submit_stale_foreign_inflight_cancel(
    shared: &Arc<SharedData>,
    provider: &crate::services::provider::ProviderKind,
    channel_id: poise::serenity_prelude::ChannelId,
    probe: &super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot,
) -> bool {
    let finalizer_turn_id = probe.pin.finalizer_turn_id;
    if finalizer_turn_id == 0 {
        return false;
    }
    let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel_id)
        .await
        .active_user_message_id
        .map(|id| id.get());
    if mailbox_active_user_msg_id != probe.pin.mailbox_active_user_msg_id {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
            mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
            "tui_direct_pending_start: stale FOREIGN cancel no-op; mailbox episode changed"
        );
        return false;
    }
    // #5071 T3-A1: pin the live execution identity alongside the cancel
    // pointer, the owner channel and the output path, so the registry CAS below
    // re-reads all of them as conjuncts. This helper compares only the session
    // key and the cancel pointer itself, so the owner/output half of T3-R2 rides
    // on the fence; the relay-recovery sibling's helper makes that comparison
    // internally and pins no binding.
    let pinned = probe
        .pin
        .tmux_session_name
        .as_deref()
        .and_then(|tmux_session| {
            watcher_cancel::pin_watcher_for_tmux_session(&shared.tmux_watchers, tmux_session)
        })
        .map(|pinned| {
            let identity_fence = WatcherIdentityFence::capture(
                execution_identity_mode(),
                STALE_FOREIGN_CANCEL_IDENTITY_SITE,
                &pinned.tmux_session_name,
            )
            .with_pinned_binding(pinned.owner_channel_id, &pinned.output_path);
            (pinned, identity_fence)
        });
    // #5071 relay-tail S4 (I-1): pinned alongside the execution identity above.
    // The `Arc` is the channel's LIVE lease cell, so the registry CAS below
    // re-reads the current lease through it; the key comes from the probe so the
    // relevance test names the same turn the rest of this helper committed on.
    // Scope: it is consumed by that CAS only. When `pinned` is `None` there is no
    // watcher to remove, this helper takes the `CommittedNoWatcher` path, and the
    // finalizer submit below is NOT lease-fenced.
    let delivery_fence = TerminalDeliveryFence::capture(
        shared.delivery_lease(channel_id),
        probe.delivery_lease_key.clone(),
        STALE_FOREIGN_CANCEL_IDENTITY_SITE,
    );
    let pinned_watcher = pinned.is_some();
    let commit_outcome = super::super::inflight::commit_destructive_cancel_locked(
        provider,
        channel_id.get(),
        &probe.inflight_identity,
        &probe.updated_at,
        probe.save_generation,
        // #5071 T3-A1: the flock callback no longer stores `cancel`. This
        // helper only REMOVES from the registry, so the store moved below the
        // CAS that re-compares the pinned values against the live row.
        move |_| {
            if pinned_watcher {
                Ok(super::super::inflight::CommitEvidence::CancelledWatcher)
            } else {
                Ok(super::super::inflight::CommitEvidence::NoWatcher)
            }
        },
    );
    if !matches!(
        commit_outcome,
        super::super::inflight::DestructiveCancelCommitOutcome::CommittedCancelled
            | super::super::inflight::DestructiveCancelCommitOutcome::CommittedNoWatcher
    ) {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            ?commit_outcome,
            "tui_direct_pending_start: stale FOREIGN cancel no-op; flock-held pin commit failed"
        );
        return false;
    }
    #[cfg(test)]
    run_destructive_cancel_post_gate_hook_for_tests(DestructiveCancelHookPoint::PreRegistryCas);
    // The flock is released before registry CAS; the two lock domains never overlap.
    if let Some((pinned, identity_fence)) = pinned {
        // #5071 T3-A1: unlike the relay-recovery sibling, this helper only
        // REMOVES — it never writes `cancel` — so the store belongs here, after
        // the CAS (cancel pointer, plus the pinned owner channel, output path
        // and captured/live spawn nonce carried by the fence) found every value
        // this path pinned still equal to the live one. That equality is not a
        // row-generation proof; `WatcherIdentityFence` declares what it misses.
        // A failed CAS leaves `cancel` unset and the watcher relaying, rather
        // than silencing a watcher the registry still lists.
        if shared
            .tmux_watchers
            .under_identity_fence(identity_fence)
            .with_terminal_delivery_fence(delivery_fence)
            .remove_tmux_session_if_current(&pinned.tmux_session_name, &pinned.cancel)
            .is_none()
        {
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = pinned.tmux_session_name.as_str(),
                "tui_direct_pending_start: stale FOREIGN cancel committed but watcher incarnation changed; finalizer skipped"
            );
            return false;
        }
        pinned
            .cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
    // E1 closes the watcher left behind at destruction time. It does not close a
    // last pre-cancel self-heal iteration or later restoration/reclaim recreation
    // (#5012/E3), nor durable observation of finalizer degradation (E2/E6).
    let stale_identity = probe.inflight_identity.clone();
    let _ = shared
        .turn_finalizer
        .submit_terminal(
            super::super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::turn_finalizer::TerminalEvent::Cancel,
            stale_foreign_cancel_finalize_context(),
            shared.clone(),
        )
        .await;

    let lifecycle_clear_outcome =
        super::super::inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(
            provider,
            channel_id.get(),
            &stale_identity,
            &probe.updated_at,
            probe.save_generation,
        );

    let gone_or_changed = !super::super::inflight::load_inflight_state(provider, channel_id.get())
        .is_some_and(|current| {
            stale_identity == super::super::inflight::InflightTurnIdentity::from_state(&current)
                && current.effective_finalizer_turn_id() == finalizer_turn_id
        });
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        finalizer_turn_id,
        lifecycle_clear_outcome = ?lifecycle_clear_outcome,
        gone_or_changed,
        "tui_direct_pending_start: stale FOREIGN finalizer cancel completed under death-evidence gate"
    );
    gone_or_changed
}

pub(super) async fn submit_committed_foreign_inflight_complete(
    shared: &Arc<SharedData>,
    provider: &crate::services::provider::ProviderKind,
    channel_id: poise::serenity_prelude::ChannelId,
    probe: &super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot,
    restart_orphan_evidence: bool,
) -> bool {
    let finalizer_turn_id = probe.pin.finalizer_turn_id;
    if finalizer_turn_id == 0 {
        return false;
    }
    let Some(current) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            finalizer_turn_id,
            "tui_direct_pending_start: committed FOREIGN complete no-op; inflight disappeared before finalizer submit"
        );
        return false;
    };
    let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel_id)
        .await
        .active_user_message_id
        .map(|id| id.get());
    let terminal_envelope_present =
        super::super::destructive_cancel_gate::terminal_envelope_present(provider, probe);
    if !current.terminal_delivery_committed
        || (!terminal_envelope_present && !restart_orphan_evidence)
        || !probe.pin.matches_state(&current)
        || mailbox_active_user_msg_id != probe.pin.mailbox_active_user_msg_id
        || current.updated_at != probe.updated_at
        || current.save_generation != probe.save_generation
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            expected_finalizer_turn_id = finalizer_turn_id,
            current_finalizer_turn_id = current.effective_finalizer_turn_id(),
            expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
            mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
            expected_tmux_session = ?probe.pin.tmux_session_name,
            current_tmux_session = ?current.tmux_session_name,
            terminal_delivery_committed = current.terminal_delivery_committed,
            expected_updated_at = %probe.updated_at,
            current_updated_at = %current.updated_at,
            expected_save_generation = probe.save_generation,
            current_save_generation = current.save_generation,
            "tui_direct_pending_start: committed FOREIGN complete no-op; terminal envelope or identity pin no longer matches"
        );
        return false;
    }

    let committed_identity = super::super::inflight::InflightTurnIdentity::from_state(&current);
    if restart_orphan_evidence {
        let archive_outcome =
            super::super::inflight::archive_inflight_state_if_matches_identity_generation(
                provider,
                channel_id.get(),
                &committed_identity,
                &probe.updated_at,
                probe.save_generation,
                "stuck-restart-orphan",
            );
        if archive_outcome != super::super::inflight::GuardedClearOutcome::Cleared {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                ?archive_outcome,
                "tui_direct_pending_start: restart-orphan archive failed; preserving committed FOREIGN inflight"
            );
            return false;
        }
    }
    let outcome = shared
        .turn_finalizer
        .submit_terminal(
            super::super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::turn_finalizer::TerminalEvent::Complete,
            committed_foreign_complete_finalize_context(),
            shared.clone(),
        )
        .await;

    let gone_or_changed = !super::super::inflight::load_inflight_state(provider, channel_id.get())
        .is_some_and(|current| {
            committed_identity == super::super::inflight::InflightTurnIdentity::from_state(&current)
                && current.effective_finalizer_turn_id() == finalizer_turn_id
                && current.save_generation == probe.save_generation
        });
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        finalizer_turn_id,
        finalize_outcome = ?std::mem::discriminant(&outcome),
        gone_or_changed,
        restart_orphan_evidence,
        "tui_direct_pending_start: committed FOREIGN inflight cleared via finalizer Complete under terminal or restart-orphan evidence"
    );
    gone_or_changed
}

pub(in crate::services::discord) async fn demote_stale_foreign_inflight_if_current(
    shared: &Arc<SharedData>,
    record: &TuiDirectPendingStart,
) -> bool {
    let Some(provider) = crate::services::provider::ProviderKind::from_str(&record.provider) else {
        return false;
    };
    let channel = poise::serenity_prelude::ChannelId::new(record.channel_id);
    let Some(state) = super::super::inflight::load_inflight_state(&provider, record.channel_id)
    else {
        return false;
    };
    let capture_offset = output_capture_offset(&state);
    if committed_foreign_inflight_is_finalize_clearable(&state, record) {
        let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel)
            .await
            .active_user_message_id
            .map(|id| id.get());
        let probe =
            super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot::from_state(
                shared.as_ref(),
                &state,
                mailbox_active_user_msg_id,
                channel,
            );
        let relay_frontier = probe.relay_frontier;
        let terminal_envelope_present =
            super::super::destructive_cancel_gate::terminal_envelope_present(&provider, &probe);
        let pane_ready_for_input = !terminal_envelope_present
            && restart_orphan_pane_ready_for_input(&provider, &state, &record.tmux_session_name);
        let restart_evidence = restart_orphan_evidence_at(
            &state,
            shared.restart.current_generation,
            chrono::Utc::now().timestamp(),
            pane_ready_for_input,
        );
        if !terminal_envelope_present && !restart_evidence.permits_finalize_clear() {
            tracing::warn!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                committed_user_msg_id = state.user_msg_id,
                committed_started_at = %state.started_at,
                committed_updated_at = %state.updated_at,
                relay_frontier = ?relay_frontier,
                capture_offset = ?capture_offset,
                generation_crossed = restart_evidence.generation_crossed,
                committed_frozen_past_grace = restart_evidence.committed_frozen_past_grace,
                pane_ready_for_input = restart_evidence.pane_ready_for_input,
                "tui_direct_pending_start: skipped committed FOREIGN finalize-clear; terminal envelope and restart-orphan evidence missing"
            );
            return false;
        }
        let cleared = submit_committed_foreign_inflight_complete(
            shared,
            &provider,
            channel,
            &probe,
            !terminal_envelope_present,
        )
        .await;
        if cleared {
            tracing::warn!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                committed_user_msg_id = state.user_msg_id,
                committed_started_at = %state.started_at,
                committed_updated_at = %state.updated_at,
                relay_frontier = ?relay_frontier,
                capture_offset = ?capture_offset,
                restart_orphan_evidence = !terminal_envelope_present,
                "tui_direct_pending_start: cleared committed FOREIGN inflight via finalizer Complete; re-evaluating before claiming (#4805)"
            );
        }
        return cleared;
    }
    if !stale_foreign_inflight_is_reclaimable_at(&state, record, chrono::Utc::now().timestamp()) {
        return false;
    }
    let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel)
        .await
        .active_user_message_id
        .map(|id| id.get());
    let probe = super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot::from_state(
        shared.as_ref(),
        &state,
        mailbox_active_user_msg_id,
        channel,
    );
    let relay_frontier = probe.relay_frontier;
    let gate = super::super::destructive_cancel_gate::evaluate(
        shared, &provider, channel, channel, &probe,
    )
    .await;
    if !gate.is_allowed() {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            stale_user_msg_id = state.user_msg_id,
            stale_started_at = %state.started_at,
            stale_updated_at = %state.updated_at,
            relay_frontier = ?relay_frontier,
            capture_offset = ?capture_offset,
            denied_reason = gate.denied_reason().unwrap_or("unknown"),
            "tui_direct_pending_start: skipped destructive stale FOREIGN demotion; death/identity gate did not pass (#4030)"
        );
        return false;
    }

    #[cfg(test)]
    run_destructive_cancel_post_gate_hook_for_tests(DestructiveCancelHookPoint::PostGate);
    let demoted = submit_stale_foreign_inflight_cancel(shared, &provider, channel, &probe).await;
    if demoted {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            stale_user_msg_id = state.user_msg_id,
            stale_started_at = %state.started_at,
            stale_updated_at = %state.updated_at,
            relay_frontier = ?relay_frontier,
            capture_offset = ?capture_offset,
            death_evidence = gate.allowed_reason().unwrap_or("unknown"),
            min_stale_age_secs = STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS,
            "tui_direct_pending_start: demoted stale FOREIGN inflight with dead relay frontier via finalizer Cancel; re-evaluating before claiming (#4030)"
        );
    }
    demoted
}

/// #3296 codex r3: choose the foreign identity an aborted-anchor marker pins.
/// The worker's LAST-VIEW identity is PRIMARY — that row was observed LIVE
/// during the backstop window, so it is definitionally the turn the ABORT
/// deferred on. The cleanup-instant inflight row is read (lazily) ONLY when
/// no poll ever captured an identity: between the final backstop view and the
/// cleanup's read, the foreign row may terminal-commit (tombstone + clear)
/// and a SUCCESSOR row may already hold the `(provider, channel)` slot —
/// preferring the current row pinned that WRONG turn (the genuine prior
/// commit's tombstone then never matched the marker, and the successor's own
/// commit could false-`✅` a possibly-unanswered anchor). The no-view fallback
/// is deliberately kept conservative-best-effort: with no observed identity
/// the cleanup-instant row is the only evidence available (a successor there
/// would need the never-observed prior row to clear AND a new claim to land
/// inside the same µs window), while pinning nothing forfeits drain coverage
/// outright — a guaranteed bounded `⚠` even on an answered anchor.
pub(in crate::services::discord) fn pin_abort_foreign_identity(
    last_view_foreign: Option<(u64, String)>,
    read_cleanup_instant_row: impl FnOnce() -> Option<(u64, String)>,
) -> Option<(u64, String)> {
    last_view_foreign.or_else(read_cleanup_instant_row)
}

/// Spawn the DETACHED per-channel worker. Acquires the channel lock (FIFO
/// serialization), polls the wait predicate until the prior turn finalizes (or
/// the 8s backstop fires), runs the claim, and deletes the record. On the
/// terminal backstop ABORT it runs `abort_cleanup_fn` (the aborted-anchor
/// marker record — #3282/#3296) before dropping the record. Returns immediately
/// so the observer loop is never blocked.
pub(in crate::services::discord) fn spawn_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
    reclaim_orphan_fn: ReclaimOrphanFn,
) {
    let active_guard = active_worker_guard_for_spawn(&record.provider, record.channel_id);
    super::super::task_supervisor::spawn_observed("tui_direct_pending_start_worker", async move {
        let _active_guard = active_guard;
        run_worker_inner(
            shared,
            record,
            view_fn,
            claim_fn,
            abort_cleanup_fn,
            reclaim_orphan_fn,
        )
        .await;
    });
}

/// Why the worker's wait loop ended this cycle.
pub(super) enum WaitOutcome {
    /// The prior turn genuinely finalized — claiming is safe.
    Finalized,
    /// The backstop expired AND, at the claim instant, the prior inflight is
    /// gone / only ever our own anchor — claiming is safe (a wedged-but-cleared
    /// or binding-transient prior). Carries the final view for observability.
    BackstopClaimSafe,
    /// The backstop expired but a FOREIGN prior inflight is STILL live —
    /// claiming would overwrite it (the #3154 regression). Keep deferring.
    BackstopForeignInflightLive,
}

#[cfg(test)]
pub(super) async fn run_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
    reclaim_orphan_fn: ReclaimOrphanFn,
) {
    let _active_guard = ActiveWorkerGuard::new(&record.provider, record.channel_id);
    run_worker_inner(
        shared,
        record,
        view_fn,
        claim_fn,
        abort_cleanup_fn,
        reclaim_orphan_fn,
    )
    .await;
}

pub(super) async fn run_worker_inner(
    shared: Arc<SharedData>,
    mut record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
    reclaim_orphan_fn: ReclaimOrphanFn,
) {
    let lock = channel_lock(&record.provider, record.channel_id);
    let _guard = lock.lock().await;

    let mut backstop_cycles: u32 = 0;
    let mut claim_attempts: u32 = 0;
    let worker_start = tokio::time::Instant::now();
    // codex r2: the most recent poll's live FOREIGN inflight identity. Handed
    // to the ABORT cleanup so the aborted-anchor marker pins WHICH turn it was
    // deferring on even when that row vanishes before the cleanup's own read.
    let mut last_foreign_identity: Option<(u64, String)> = None;

    loop {
        // ---- Wait window: poll until finalized or backstop expiry. ----
        let cycle_start = tokio::time::Instant::now();
        let outcome = loop {
            if let Some(obs) = view_fn(&shared, &record).await {
                if obs.foreign_inflight_identity.is_some() {
                    last_foreign_identity = obs.foreign_inflight_identity;
                }
                if prior_turn_finalized(obs.view) {
                    break WaitOutcome::Finalized;
                }
            }
            if cycle_start.elapsed() >= PENDING_START_BACKSTOP {
                break match view_fn(&shared, &record).await {
                    Some(obs) => {
                        if obs.foreign_inflight_identity.is_some() {
                            last_foreign_identity = obs.foreign_inflight_identity;
                        }
                        if backstop_claim_is_safe(obs.view) {
                            WaitOutcome::BackstopClaimSafe
                        } else {
                            WaitOutcome::BackstopForeignInflightLive
                        }
                    }
                    None => WaitOutcome::BackstopForeignInflightLive,
                };
            }
            tokio::time::sleep(PENDING_START_POLL).await;
        };

        match outcome {
            WaitOutcome::Finalized => {}
            WaitOutcome::BackstopClaimSafe => {
                tracing::warn!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    backstop_ms = PENDING_START_BACKSTOP.as_millis(),
                    backstop_cycle = backstop_cycles,
                    "tui_direct_pending_start: prior turn did not finalize within backstop, but the prior inflight is gone at the claim instant; claiming with fresh EOF offset"
                );
            }
            WaitOutcome::BackstopForeignInflightLive => {
                // #4030 + #3982: before escalating, try the bounded stale-foreign
                // recovery hook. It first demotes a FOREIGN inflight whose
                // `updated_at` crossed the #4020 120s positive-stale gate AND
                // whose relay frontier never advanced despite captured output;
                // then it falls back to the #3982 producer-dead SessionBoundRelay
                // orphan downgrade. Either success only causes an immediate
                // re-evaluation; the worker never claims on this stale view.
                let reclaim_outcome = reclaim_orphan_fn(&shared, &record).await;
                if reclaim_outcome.is_reclaimed() {
                    tracing::warn!(
                        provider = %record.provider,
                        channel_id = record.channel_id,
                        tmux_session_name = %record.tmux_session_name,
                        anchor_message_id = record.anchor_message_id,
                        backstop_cycle = backstop_cycles,
                        event = reclaim_outcome.event_key(),
                        "tui_direct_pending_start: reclaimed/demoted a stale FOREIGN inflight blocking this synthetic start; re-evaluating immediately before any claim (#4030/#3982)"
                    );
                    continue;
                }
                // No stale/demotable row matched. Keep the existing bounded
                // escalation/abort behavior; a failed recovery attempt must not
                // turn into an infinite spin or an unsafe overwrite.
                backstop_cycles = backstop_cycles.saturating_add(1);
                if backstop_cycles >= PENDING_START_MAX_BACKSTOP_CYCLES {
                    // ABORT SAFELY (P1-1): a foreign prior inflight stayed live
                    // across the escalation budget. We refuse to overwrite it.
                    // Surface an observability event and drop only the synthetic
                    // OWNERSHIP claim (the provider prompt was already submitted;
                    // the watcher/bridge still relays its output).
                    // #3296: WARN, not ERROR — this branch fires by definition
                    // only when a FOREIGN inflight is live on the SAME channel,
                    // i.e. the input was already submitted and usually merges
                    // into the prior owner's turn (a normal outcome, not a
                    // failure). The event key is load-bearing — never change it.
                    tracing::warn!(
                        provider = %record.provider,
                        channel_id = record.channel_id,
                        tmux_session_name = %record.tmux_session_name,
                        anchor_message_id = record.anchor_message_id,
                        backstop_cycles,
                        waited_ms = worker_start.elapsed().as_millis(),
                        event = "tui_direct_pending_start.backstop_abort_foreign_inflight_live",
                        "tui_direct_pending_start: prior inflight stayed LIVE across the backstop escalation budget; ABORTING the synthetic turn-start claim without overwriting the live prior turn — input already submitted; abort marker recorded, reconcile lands ✅ via prior-owner completion or ⚠ via TTL fallback (#3296)"
                    );
                    // #3282/#3296: no claim will ever run for this anchor, so
                    // the normal `⏳ → ✅` completion never fires — record the
                    // durable aborted-anchor marker here (the anchor keeps its
                    // ⏳; the watcher drain / TTL sweep own the reconcile),
                    // pinning the last-view foreign identity (codex r2).
                    abort_cleanup_fn(&shared, &record, last_foreign_identity.clone()).await;
                    delete(&record);
                    // #3540 (B′ — defense-in-depth, NO EVICT): the pending gate is
                    // now released (`delete` above), but a follow-up the user sent
                    // while this synthetic start was deferring is still parked in
                    // the mailbox queue behind a QUEUE-ACK. If the FOREIGN inflight
                    // we were deferring on is a phantom (#3540 root cause: a
                    // watermark-reset re-claim whose commit will never arrive), the
                    // queued follow-up would otherwise stay parked until the
                    // ABORT_MARKER_TTL sweep. Kick the EXISTING mailbox dispatch
                    // path once so the follow-up promotes promptly. This clears /
                    // resets / deletes NO inflight — `kickoff_idle_queues` routes
                    // through `mailbox_try_start_turn_kinded`, which (a) starts a
                    // fresh turn if the slot is genuinely free, or (b) MERGES into a
                    // still-live prior turn (worst case = normal merge, zero live
                    // loss). The phantom row, if any, is reaped later by its own
                    // commit/finalize or the bounded ⏳ sweep — never evicted here.
                    promote_queued_follow_up_after_abort(&shared, &record);
                    return;
                }
                tracing::warn!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    backstop_cycle = backstop_cycles,
                    max_cycles = PENDING_START_MAX_BACKSTOP_CYCLES,
                    "tui_direct_pending_start: backstop expired but a FOREIGN prior inflight is still live; refusing to overwrite, re-deferring (bounded escalation)"
                );
                // Re-defer: another full wait window.
                continue;
            }
        }

        // ---- Claim. Only delete the durable record on a SUCCESSFUL claim. ----
        let claimed = claim_fn(&shared, &record).await;
        if claimed {
            tracing::info!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                waited_ms = worker_start.elapsed().as_millis(),
                backstop_cycles,
                claim_attempts,
                "tui_direct_pending_start: deferred synthetic turn-start claimed after prior turn finalized"
            );
            // #3303: record the own-identity DeferredClaim marker BEFORE the
            // durable record delete (a crash between the two re-claims on
            // restart and re-records idempotently — the marker stem
            // overwrites). Fail-open: nothing in there can fail the claim.
            record_deferred_claim_marker_if_watcher_owned(&record);
            // Delete only AFTER a successful claim (P1-2). A crash between the
            // inflight save and this delete is healed on restart: the worker
            // re-runs and the claim adopts the matching anchor's existing
            // inflight idempotently, then deletes.
            delete(&record);
            return;
        }

        // Transient claim failure: do NOT delete (P1-2). Retry, bounded.
        claim_attempts = claim_attempts.saturating_add(1);
        update_claim_attempt_count(&mut record, claim_attempts);
        if claim_attempts >= PENDING_START_MAX_CLAIM_ATTEMPTS {
            tracing::error!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                claim_attempts,
                waited_ms = worker_start.elapsed().as_millis(),
                event = "tui_direct_pending_start.claim_retry_exhausted",
                "tui_direct_pending_start: claim returned false across the retry budget (another turn owns the mailbox or saves keep failing); abandoning the synthetic ownership claim to avoid an unbounded spin (record retained for restart re-attempt)"
            );
            // Leave the durable record in place: a later restart restore will
            // re-attempt idempotently rather than silently lose the prompt.
            return;
        }
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            claim_attempt = claim_attempts,
            max_attempts = PENDING_START_MAX_CLAIM_ATTEMPTS,
            "tui_direct_pending_start: claim returned false (transient); retaining durable record and retrying"
        );
        tokio::time::sleep(PENDING_START_CLAIM_RETRY_BACKOFF).await;
        // Loop back: re-confirm the prior turn is still finalized, then re-claim.
    }
}

/// #3303 — after a SUCCESSFUL deferred claim, record a
/// [`super::super::tui_direct_abort_marker`] marker of kind `DeferredClaim` pinned to
/// the worker's OWN synthetic turn identity (`user_msg_id == anchor`, the
/// freshly-claimed row's `started_at`).
///
/// Why: the claim hands the turn to the watcher, but the observed #3303
/// failure modes (the claim seeded the relay cursor at EOF after a prior
/// drain already consumed the response bytes, or the relay fails and a
/// watchdog clears the row) mean NO terminal-commit pass ever flips the
/// anchor's `⏳ → ✅` — an eternal hourglass with no reconcile owner. With the
/// marker, the watcher chokepoint's drain covers it on the own turn's commit
/// (`✅`, idempotent next to the normal completion), and the sweep bounds the
/// never-committed case with the TTL `⚠`.
///
/// Guards (in order):
/// * **SC3 scope gate** — record ONLY when the post-claim lease says the
///   `TmuxWatcher` owns the relay: a BridgeAdapter-owned turn finalizes via
///   the bridge WITHOUT the watcher chokepoint tombstone, so a marker would
///   contradict its normal completion with a TTL `⚠`.
/// * **Own-row guard** — the inflight row re-read at the record instant must
///   BE this claim's synthetic turn (anchor + tmux session match); its
///   `started_at` is the identity the marker pins (#3303 SC1: never the
///   foreign prior turn — that tombstone is already durable at claim time and
///   would false-`✅` instantly).
/// * **Fail-open** — every miss above (and a failed marker write) only warns:
///   the claim, the durable-record delete, and the turn proceed exactly as
///   before #3303.
/// #3350: the marker-record chokepoint SHARED by the deferred worker (#3303,
/// via the thin [`record_deferred_claim_marker_if_watcher_owned`] wrapper) and
/// the INLINE synthetic claim (`tui_prompt_relay`). Both claim paths must
/// leave the same durable `DeferredClaim` marker, or an inline-claimed turn
/// whose output is never committed (e.g. stale input right after `/clear`)
/// keeps an eternal anchor `⏳`. Body unchanged from the #3303 helper — every
/// guard above applies verbatim.
pub(in crate::services::discord) fn record_claim_marker_if_watcher_owned(
    provider: &str,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: &str,
) {
    if anchor_message_id == 0 {
        return; // I5: a zero anchor id could never be reconciled (record() rejects it too)
    }
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider,
        tmux_session_name,
        channel_id,
    );
    let relay_owner = lease.map(|lease| lease.relay_owner);
    if relay_owner != Some(crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::TmuxWatcher)
    {
        tracing::debug!(
            provider = %provider,
            channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id,
            relay_owner = ?relay_owner,
            "tui_direct_pending_start: deferred-claim marker skipped — turn is not watcher-owned, the watcher chokepoint will never tombstone it (#3303 SC3)"
        );
        return;
    }
    let Some(provider_kind) = crate::services::provider::ProviderKind::from_str(provider) else {
        tracing::warn!(
            provider = %provider,
            channel_id,
            anchor_message_id,
            "tui_direct_pending_start: unparseable provider; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    };
    let Some(row) = super::super::inflight::load_inflight_state(&provider_kind, channel_id) else {
        tracing::warn!(
            provider = %provider,
            channel_id,
            anchor_message_id,
            "tui_direct_pending_start: no inflight row at the record instant after a successful claim; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    };
    let row_is_own_turn = row.user_msg_id == anchor_message_id
        && row.tmux_session_name.as_deref() == Some(tmux_session_name);
    if !row_is_own_turn {
        tracing::warn!(
            provider = %provider,
            channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id,
            row_user_msg_id = row.user_msg_id,
            row_tmux_session_name = ?row.tmux_session_name,
            "tui_direct_pending_start: inflight row is not this claim's own synthetic turn; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    }
    match super::super::tui_direct_abort_marker::record_for_deferred_claim(
        provider.to_string(),
        channel_id,
        anchor_message_id,
        tmux_session_name.to_string(),
        (anchor_message_id, row.started_at),
        row.turn_start_offset,
    ) {
        Ok(marker) => tracing::info!(
            provider = %provider,
            channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id,
            own_started_at = ?marker.foreign_started_at,
            tombstone_covered = marker.covered_at_ms.is_some(),
            "tui_direct_pending_start: deferred-claim marker recorded pinning the OWN synthetic turn — its commit drains ⏳ → ✅, a never-committed turn converges to the bounded sweep ⚠ (#3303)"
        ),
        Err(error) => tracing::warn!(
            provider = %provider,
            channel_id,
            anchor_message_id,
            error = %error,
            "tui_direct_pending_start: failed to persist the deferred-claim marker; claim proceeds without it (fail-open — pre-#3303 behavior, the anchor ⏳ may linger) (#3303)"
        ),
    }
}

pub(super) fn record_deferred_claim_marker_if_watcher_owned(record: &TuiDirectPendingStart) {
    record_claim_marker_if_watcher_owned(
        &record.provider,
        record.channel_id,
        record.anchor_message_id,
        &record.tmux_session_name,
    );
}

/// #3540 (B′): after the terminal backstop ABORT has run `abort_cleanup_fn` and
/// `delete(&record)` (pending gate released), kick the EXISTING mailbox dispatch
/// path ONCE so a follow-up parked behind a QUEUE-ACK promotes promptly instead
/// of waiting out the bounded ⏳ sweep when the deferred-on FOREIGN inflight was
/// a phantom.
///
/// NO-EVICT INVARIANT (load-bearing): this function does NOT clear / reset /
/// `save_inflight(empty)` / delete ANY inflight row. It only schedules
/// [`super::super::schedule_deferred_idle_queue_kickoff`], the same idempotent
/// queued-dispatch entrypoint the post-turn / catch-up paths already use. That
/// kickoff routes through `mailbox_try_start_turn_kinded`, which either starts a
/// fresh turn (slot genuinely free) or MERGES the follow-up into a still-live
/// prior turn — so even if the deferred-on row is in fact a live turn, the worst
/// case is a normal merge with ZERO live-turn loss. The serialization is the
/// channel lock the worker already holds (this runs before its `return`, under
/// `_guard`); the kickoff's own work is detached, so no new lock-order risk.
/// Fail-soft: an unparseable provider only warns — the ABORT path is otherwise
/// unchanged (pre-#3540 behavior: the follow-up waits for the sweep).
pub(super) fn promote_queued_follow_up_after_abort(
    shared: &Arc<SharedData>,
    record: &TuiDirectPendingStart,
) {
    let Some(provider) = crate::services::provider::ProviderKind::from_str(&record.provider) else {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            "tui_direct_pending_start: unparseable provider; skipping post-abort queue promote (fail-open — follow-up still drains via the bounded sweep) (#3540)"
        );
        return;
    };
    let channel_id = poise::serenity_prelude::ChannelId::new(record.channel_id);
    tracing::info!(
        provider = provider.as_str(),
        channel_id = record.channel_id,
        anchor_message_id = record.anchor_message_id,
        "tui_direct_pending_start: post-abort queue promote — kicking the existing mailbox dispatch once so a queued follow-up is not parked until the ⏳ sweep; NO inflight is cleared/reset/deleted (#3540 B′)"
    );
    #[cfg(test)]
    {
        // Test seam: record that the promote fired exactly once without spawning
        // the real detached kickoff task (which would leak past the test and, with
        // a test `shared`, has no cached ctx/token to act on anyway). Production
        // (below) takes the real path.
        POST_ABORT_PROMOTE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = (shared, channel_id, &provider);
        return;
    }
    #[cfg(not(test))]
    super::super::schedule_deferred_idle_queue_kickoff(
        shared.clone(),
        provider,
        channel_id,
        "tui_direct_pending_start backstop abort follow-up promote (#3540)",
    );
}

/// #3540 (B′) test seam: counts `promote_queued_follow_up_after_abort` firings so
/// the ABORT-path regression test can assert it ran EXACTLY ONCE while the claim
/// (inflight write) NEVER ran — proving the queue is promoted without evicting or
/// clearing any inflight row.
#[cfg(test)]
pub(super) static POST_ABORT_PROMOTE_CALLS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// #3350 issue-3: the observer INLINE-claim wiring, separated so a unit test
/// can pin it — `relay_observed_prompt` must record the #3303 DeferredClaim
/// marker IFF the inline synthetic claim actually claimed, forwarding the
/// prompt's exact `(provider, channel, anchor, tmux)` identity. `recorder` is
/// injected (`FnOnce` flavor of the `ClaimFn` injection convention);
/// production passes [`record_claim_marker_if_watcher_owned`] itself, so the
/// signature match is compiler-pinned at the call site.
pub(in crate::services::discord) fn record_inline_claim_marker_if_claimed(
    claimed: bool,
    provider: &str,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: &str,
    recorder: impl FnOnce(&str, u64, u64, &str),
) {
    if claimed {
        recorder(provider, channel_id, anchor_message_id, tmux_session_name);
    }
}
