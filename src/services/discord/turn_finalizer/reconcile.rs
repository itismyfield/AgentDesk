//! Timer-driven lease recovery, deadline finalization, and ledger expiry.

use super::completion_admission::publish_claimed_queue_eligible;
use super::watcher_backstop::watcher_backstop_check;
use super::*;

#[cfg(unix)]
fn generic_backstop_allowed(entry: &LedgerEntry) -> bool {
    entry.completion_admission.native_input.is_none()
}

#[cfg(not(unix))]
fn generic_backstop_allowed(_entry: &LedgerEntry) -> bool {
    true
}

/// Run the deadline-elapsed backstop finalize for ONE entry: flip
/// `Pending → Finalizing` (skip if a concurrent terminal already advanced it),
/// run `do_finalize` on the backstop context, then flip `Finalized`. Shared by
/// the gate-timeout deadline arm and the phase-5a watcher far-backstop arm so
/// exactly-once is decided in one place. (#3016 phase-5b2: the legacy
/// `mailbox_finalize_owed` revoke that used to run here is gone with the flag.)
async fn run_backstop_finalize(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    ledger_key: LedgerKey,
    turn_key: TurnKey,
    provider: ProviderKind,
    shared: &Arc<SharedData>,
    now: Instant,
) {
    let Some(entry) = ledger
        .get_mut(&ledger_key)
        .filter(|entry| entry.phase == Phase::Pending)
    else {
        return;
    };
    if !generic_backstop_allowed(entry) {
        // Native settlement alone ends this turn; name the stuck entry once instead of every tick.
        if !std::mem::replace(&mut entry.native_backstop_warned, true) {
            tracing::warn!(
                channel_id = ledger_key.channel_id.get(),
                user_msg_id = ledger_key.user_msg_id,
                generation = ledger_key.generation,
                provider = provider.as_str(),
                reason = "native_input",
                event = "watcher_backstop_native_input_guard",
                "generic backstop skipped for native input; only its native settlement can end the turn"
            );
        }
        return;
    }
    entry.phase = Phase::Finalizing;
    // Backstop finalize: the deferred terminal originated from the watcher but
    // no caller is around to clear inflight, so the backstop context clears it
    // here — and the row, still on disk, feeds the marker-ensure row fallback.
    //
    // #3866: this is the reconcile/backstop branch of the finalize side-effect
    // surface (gate-timeout deadline + watcher far-backstop). Contain a panic
    // here so it cannot unwind `reconcile` and kill the actor loop, and so the
    // Finalizing->Finalized flip below STILL runs — the entry was just flipped to
    // `Finalizing`, and a stuck `Finalizing` entry would never be GC'd (GC reaps
    // only `Finalized`) nor re-finalized (backstops/probes gate on `Pending`),
    // leaking forever. Resetting it to `Finalized` lets GC reap it normally.
    let finalized = AssertUnwindSafe(do_finalize(
        turn_key,
        provider,
        &TerminalEvent::GateTimeout {
            pane_quiescent: Some(true),
        },
        FinalizeContext::gate_backstop(),
        None,
        shared,
    ))
    .catch_unwind()
    .await;
    if let Ok(FinalizeOutcome::Finalized {
        removed_token: Some(_),
        ..
    }) = &finalized
        && let Some(entry) = ledger.get_mut(&ledger_key)
    {
        entry.completion_admission.note_mailbox_released();
        publish_claimed_queue_eligible(shared, entry);
    }
    if let Err(payload) = finalized {
        tracing::error!(
            panic = %panic_payload_summary(payload.as_ref()),
            channel_id = ledger_key.channel_id.get(),
            user_msg_id = ledger_key.user_msg_id,
            "TurnFinalizer do_finalize panicked on the reconcile/backstop path; contained, the \
             ledger entry is reset Finalizing->Finalized below so it is never stuck and the \
             actor loop survives (#3866)"
        );
    }
    // #3016 phase-5b2: the legacy `mailbox_finalize_owed` revoke that ran here
    // is gone — the ledger's exactly-once phase gate is the sole arbiter, so
    // there is no stale flag a surviving watcher could swap.
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(now);
        entry.terminal_deadline = None;
        entry.watcher_backstop_deadline = None;
    }
}

/// Consume identity-guarded finalize misses without ever converting a stale
/// terminal into channel-scoped release authority.
///
/// A different mailbox owner id or episode nonce means a newer turn won and
/// the residue is resolved without mutation. A matching episode is released
/// only through the same #5176 terminal-evidence conjunction: explicit release
/// authority (same terminal episode, or a separately cancelled token), no
/// inflight owner, and structurally idle TUI. The mailbox actor then repeats an
/// id + nonce + start-cutoff CAS at the mutation point.
async fn reconcile_guarded_finish_residues(shared: &Arc<SharedData>) {
    let residues: Vec<_> = shared
        .turn_finalizer
        .guarded_finish_residues()
        .iter()
        .map(|entry| (*entry.key(), entry.value().clone()))
        .collect();

    for (channel_id, residue) in residues {
        let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
        // The owner we recorded is gone — idle mailbox, different id, or a
        // successor that reused the id under a new nonce. Nothing of ours is
        // left to recover, so consume the record without mutating anything.
        if !residue.matches_observed_owner(&snapshot) {
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .remove(&channel_id);
            continue;
        }
        let same_terminal_episode = residue.same_terminal_episode(&snapshot);
        let proven_different_episode = match (
            residue.terminal_turn_nonce.as_deref(),
            snapshot.active_turn_nonce.as_deref(),
        ) {
            (Some(terminal_nonce), Some(active_nonce)) => {
                !terminal_nonce.is_empty()
                    && !active_nonce.is_empty()
                    && terminal_nonce != active_nonce
            }
            _ => false,
        };
        if proven_different_episode {
            // This is the protected counter-direction: a stale terminal saw a
            // genuinely newer episode. The record has been consumed; no
            // recovery work belongs to that live owner.
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .remove(&channel_id);
            tracing::debug!(
                provider = residue.provider.as_str(),
                channel_id = channel_id.get(),
                expected_user_msg_id = residue.expected_user_msg_id,
                active_user_msg_id = residue.active_user_msg_id,
                reason = residue.reason(),
                "TurnFinalizer guarded miss resolved as a different live episode"
            );
            continue;
        }
        let token = snapshot
            .cancel_token
            .as_ref()
            .expect("active residual snapshot must carry its token");
        let separately_cancelled = token.cancelled.load(std::sync::atomic::Ordering::Relaxed);
        // Single definition, shared with the health reporter (#5068 r2).
        let release_authorized = residue.release_authorized(&snapshot);
        let inflight_state_present = crate::services::discord::inflight::inflight_state_file_exists(
            &residue.provider,
            channel_id.get(),
        );
        let tui_structurally_idle = crate::services::discord::host_liveness::tui_structurally_idle(
            &residue.provider,
            token,
        );
        let release =
            crate::services::discord::host_liveness::terminal_evidence_allows_mailbox_release(
                true,
                release_authorized,
                inflight_state_present,
                tui_structurally_idle,
            );
        if !release {
            tracing::debug!(
                provider = residue.provider.as_str(),
                channel_id = channel_id.get(),
                expected_user_msg_id = residue.expected_user_msg_id,
                active_user_msg_id = residue.active_user_msg_id,
                generation = residue.generation,
                reason = residue.reason(),
                same_terminal_episode,
                separately_cancelled,
                inflight_state_present,
                ?tui_structurally_idle,
                "TurnFinalizer residual mailbox ownership held pending terminal evidence"
            );
            continue;
        }

        let released = super::guarded_finish_residue::release(shared, channel_id, &residue).await;
        if released {
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .remove(&channel_id);
            tracing::warn!(
                provider = residue.provider.as_str(),
                channel_id = channel_id.get(),
                expected_user_msg_id = residue.expected_user_msg_id,
                released_user_msg_id = residue.active_user_msg_id,
                generation = residue.generation,
                reason = residue.reason(),
                same_terminal_episode,
                separately_cancelled,
                "TurnFinalizer reconciler released terminal residual mailbox ownership"
            );
        }
    }
}

/// The one reconciler. Finalizes deadline-armed gate-timeouts whose backstop
/// elapsed, GUARANTEES the phase-5a watcher far-backstop for watcher-owned
/// `register_start` Pending entries that never received a terminal (re-checking
/// liveness so a paused-live turn is deferred, never over-finalized), and
/// garbage-collects `Finalized` entries past their TTL so the ledger stays
/// bounded.
pub(super) async fn reconcile(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    shared: &Arc<SharedData>,
) {
    let now = Instant::now();
    #[cfg(unix)]
    for entry in ledger.values_mut() {
        super::terminal_handler::native_policy::reconcile(entry, shared).await;
    }

    // #5068: the finalizer actor owns guarded-miss recovery. Run before other
    // deadline work so releasing a terminal residue promptly publishes the
    // mailbox completion edge and re-arms queued delivery.
    reconcile_guarded_finish_residues(shared).await;

    // #3041 P1-1 (B3): reclaim any delivery lease whose acquire deadline has
    // elapsed (a dead/stuck holder), so a legitimate successor can acquire. This
    // runs on the reconcile tick (1s) and is identity-agnostic; a `Committed`
    // lease is never reclaimed (it awaits an explicit holder release). Uses the
    // process-monotonic `lease_now_ms()` clock — the SAME clock the watcher's
    // acquire deadline is computed against — so a live holder mid-send (whose
    // ~15s deadline is kept ahead by the watcher's heartbeat-renew) is never
    // reclaimed.
    let _ = shared.reclaim_expired_delivery_leases(crate::services::discord::lease_now_ms());

    // Collect deadline-elapsed gate-timeout entries to finalize. We must not
    // hold a `&mut` borrow across the `do_finalize` await, so snapshot first.
    // The stored `turn_key` carries the identity `do_finalize` needs.
    let due: Vec<(LedgerKey, TurnKey, ProviderKind)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            if entry.phase == Phase::Pending
                && let Some(deadline) = entry.terminal_deadline
                && now >= deadline
            {
                Some((*ledger_key, entry.turn_key, entry.provider.clone()))
            } else {
                None
            }
        })
        .collect();

    for (ledger_key, turn_key, provider) in due {
        run_backstop_finalize(ledger, ledger_key, turn_key, provider, shared, now).await;
    }

    // #3277 (Defect C) — proven-terminal fast-path probe (no await). For
    // watcher-owned Pending entries whose far deadline is still distant, run
    // the STRICT (`at_deadline = false`) predicate: transcript-proven `Done`
    // under a LIVE unpaused handle ONLY — absent/cancelled/stale handles and
    // non-JSONL runtimes always defer here (codex r1, #3277 verify-3). After
    // WATCHER_BACKSTOP_TERMINAL_STREAK interval-spaced terminal probes, pull
    // the deadline in to GATE_BACKSTOP for the deadline arm's third (still
    // strict — the entry is flagged `pulled`) confirmation within seconds
    // instead of 1800s. Any non-terminal probe resets the streak.
    let probe_due: Vec<(LedgerKey, TurnKey, ProviderKind)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            let probe_spacing_elapsed = entry.watcher_backstop_probe_at.is_none_or(|at| {
                now.duration_since(at) >= WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL
            });
            if entry.phase == Phase::Pending
                && entry.relay_owner == RelayOwnerKind::Watcher
                && probe_spacing_elapsed
                && let Some(deadline) = entry.watcher_backstop_deadline
                && deadline > now + GATE_BACKSTOP
            {
                Some((*ledger_key, entry.turn_key, entry.provider.clone()))
            } else {
                None
            }
        })
        .collect();
    for (ledger_key, turn_key, provider) in probe_due {
        let channel_id = turn_key.channel_id;
        let terminal = watcher_backstop_turn_is_terminal(shared, turn_key, &provider, false);
        let Some(entry) = ledger.get_mut(&ledger_key) else {
            continue;
        };
        entry.watcher_backstop_probe_at = Some(now);
        if !terminal {
            entry.watcher_backstop_terminal_streak = 0;
            continue;
        }
        entry.watcher_backstop_terminal_streak =
            entry.watcher_backstop_terminal_streak.saturating_add(1);
        if entry.watcher_backstop_terminal_streak == WATCHER_BACKSTOP_TERMINAL_STREAK {
            entry.watcher_backstop_deadline = Some(now + GATE_BACKSTOP);
            entry.watcher_backstop_deadline_pulled = true;
            tracing::warn!(
                channel_id = channel_id.get(),
                provider = %provider.as_str(),
                streak = entry.watcher_backstop_terminal_streak,
                "#3277: watcher-owned turn is provably terminal but no terminal was ever \
                 submitted — pulling the far-backstop deadline in (finalize after a final \
                 at-deadline liveness re-check)"
            );
        }
    }

    // #3016 phase-5a — the watcher-owned `register_start` FAR backstop. Collect
    // watcher-owned Pending entries whose generous `watcher_backstop_deadline`
    // elapsed (those the watcher fresh-idle finalize never caught — the
    // under-finalize gap the `placeholder_sweeper` SKIPS once content was
    // delivered). Snapshot first so no `&mut` borrow is held across the awaits.
    let watcher_due: Vec<(LedgerKey, TurnKey, ProviderKind, bool)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            if entry.phase == Phase::Pending
                && entry.relay_owner == RelayOwnerKind::Watcher
                && let Some(deadline) = entry.watcher_backstop_deadline
                && now >= deadline
            {
                let pulled = entry.watcher_backstop_deadline_pulled;
                Some((*ledger_key, entry.turn_key, entry.provider.clone(), pulled))
            } else {
                None
            }
        })
        .collect();

    for (ledger_key, turn_key, provider, pulled) in watcher_due {
        // Liveness re-check: NEVER finalize a paused-live / still-busy turn at
        // the deadline; a still-live one EXTENDS its backstop a full horizon.
        // A fast-path-PULLED deadline stays STRICT (codex r1) so a transiently
        // absent/stale handle cannot smuggle a busy turn past the third check.
        match watcher_backstop_check(shared, turn_key, &provider, !pulled) {
            Ok(()) => {
                run_backstop_finalize(ledger, ledger_key, turn_key, provider, shared, now).await;
            }
            Err(reason) => {
                let Some(entry) = ledger
                    .get_mut(&ledger_key)
                    .filter(|entry| entry.phase == Phase::Pending)
                else {
                    continue;
                };
                entry.watcher_backstop_deadline = Some(now + WATCHER_REGISTER_BACKSTOP);
                // #3277: re-prove from scratch on the restored generous horizon.
                entry.watcher_backstop_deadline_pulled = false;
                entry.watcher_backstop_terminal_streak = 0;
                if pulled {
                    tracing::info!(
                        channel_id = ledger_key.channel_id.get(),
                        user_msg_id = ledger_key.user_msg_id,
                        generation = ledger_key.generation,
                        provider = provider.as_str(),
                        reason,
                        event = "watcher_backstop_deadline_extended",
                        extension_secs = WATCHER_REGISTER_BACKSTOP.as_secs(),
                        "watcher backstop strict re-check deferred and extended the deadline"
                    );
                }
            }
        }
    }

    // A Codex completion whose turn confirmation was refused retries here until it is confirmed.
    for entry in ledger.values_mut() {
        let channel = entry.turn_key.channel_id.get();
        if entry.provider == ProviderKind::Codex
            && entry.completion_admission.claimable()
            && crate::services::tui_o::cutover::codex_turn_awaited(channel).is_some()
        {
            publish_claimed_queue_eligible(shared, entry);
        }
    }

    // GC admission authority only after the longest bounded late-edge window, and never while it
    // still waits to confirm a Codex channel's turn mode.
    ledger.retain(|_, entry| {
        !(entry.phase == Phase::Finalized
            && !entry.native_debt_pending()
            && entry
                .finalized_at
                .is_some_and(|t| now.duration_since(t) >= COMPLETION_ADMISSION_TTL)
            && !(entry.provider == ProviderKind::Codex
                && crate::services::tui_o::cutover::codex_turn_awaited(
                    entry.turn_key.channel_id.get(),
                )
                .is_some()))
    });
    pending_admission
        .retain(|_, pending| now.duration_since(pending.updated_at) < COMPLETION_ADMISSION_TTL);
}
