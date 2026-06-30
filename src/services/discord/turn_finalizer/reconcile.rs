//! #3894 — reconcile/backstop finalize cluster split out of `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the timer-driven `reconcile` pass (expired
//! delivery-lease reclaim, deadline-armed gate-timeout finalize, the #3277
//! proven-terminal fast-path probe, the #3016 phase-5a watcher far-backstop, and
//! TTL GC) plus its `run_backstop_finalize` helper, lifted verbatim. The parent
//! re-imports `reconcile` (`use self::reconcile::reconcile`) so the actor loop's
//! `select!` arm stays byte-identical. The lone discord-module reference
//! (`super::lease_now_ms()`) is rewritten to its absolute path; the bodies are
//! otherwise identical.

use super::*;

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
    match ledger.get_mut(&ledger_key) {
        Some(entry) if entry.phase == Phase::Pending => entry.phase = Phase::Finalizing,
        _ => return,
    }
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
    if let Err(payload) = AssertUnwindSafe(do_finalize(
        turn_key,
        provider,
        &TerminalEvent::GateTimeout {
            pane_quiescent: Some(true),
        },
        FinalizeContext::gate_backstop(),
        shared,
    ))
    .catch_unwind()
    .await
    {
        tracing::error!(
            panic = %panic_payload_summary(payload.as_ref()),
            channel = ledger_key.channel_id.get(),
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

/// The one reconciler. Finalizes deadline-armed gate-timeouts whose backstop
/// elapsed, GUARANTEES the phase-5a watcher far-backstop for watcher-owned
/// `register_start` Pending entries that never received a terminal (re-checking
/// liveness so a paused-live turn is deferred, never over-finalized), and
/// garbage-collects `Finalized` entries past their TTL so the ledger stays
/// bounded.
pub(super) async fn reconcile(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    shared: &Arc<SharedData>,
) {
    let now = Instant::now();

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
    let probe_due: Vec<(LedgerKey, ChannelId, ProviderKind)> = ledger
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
                Some((
                    *ledger_key,
                    entry.turn_key.channel_id,
                    entry.provider.clone(),
                ))
            } else {
                None
            }
        })
        .collect();
    for (ledger_key, channel_id, provider) in probe_due {
        let terminal = watcher_backstop_turn_is_terminal(shared, channel_id, &provider, false);
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
                channel = channel_id.get(),
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
        if watcher_backstop_turn_is_terminal(shared, turn_key.channel_id, &provider, !pulled) {
            run_backstop_finalize(ledger, ledger_key, turn_key, provider, shared, now).await;
        } else if let Some(entry) = ledger.get_mut(&ledger_key) {
            if entry.phase == Phase::Pending {
                entry.watcher_backstop_deadline = Some(now + WATCHER_REGISTER_BACKSTOP);
                // #3277: re-prove from scratch on the restored generous horizon.
                entry.watcher_backstop_deadline_pulled = false;
                entry.watcher_backstop_terminal_streak = 0;
            }
        }
    }

    // GC finalized entries past their TTL.
    ledger.retain(|_, entry| {
        !(entry.phase == Phase::Finalized
            && entry
                .finalized_at
                .is_some_and(|t| now.duration_since(t) >= FINALIZED_TTL))
    });
}
