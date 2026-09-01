use super::*;
pub(super) async fn handle_terminal(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    key: TurnKey,
    provider: ProviderKind,
    event: TerminalEvent,
    ctx: FinalizeContext,
    claim_snapshot: Option<SyntheticClaimSnapshot>,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    // #3866: test-only injection point — lets a test drive a real finalize
    // side-effect panic through the live actor loop to prove the catch_unwind
    // guard keeps the loop alive. No effect in production builds.
    #[cfg(test)]
    test_panic_hook::maybe_panic();

    // Resolve to the entry this terminal acts on: a real id keys exactly; a
    // channel-only id-0 collapses onto the channel's single live entry
    // (recovery/orphan). An unregistered turn (post-restart inflight, no live
    // `Start`) still finalizes below — idempotent, exactly-once.
    let ledger_key = resolve_ledger_key(ledger, key);

    // Codex P1 — ambiguous channel-only terminal: an id-0 submission that fell
    // back to the literal orphan key (a recently-`Finalized` entry exists) is
    // most likely that turn's STALE terminal. With a DIFFERENT live entry on
    // this channel, the channel-scoped finish would release the follow-up's
    // token / decrement `global_active` — treat as no-op. (A genuine orphan
    // with NO live entry still finalizes below; idempotent + counter-gated.)
    if key.user_msg_id == 0 && !ledger.contains_key(&ledger_key) {
        let channel_has_live_turn = ledger.iter().any(|(lk, e)| {
            lk.channel_id == key.channel_id
                && lk.generation == key.generation
                && e.phase != Phase::Finalized
        });
        if channel_has_live_turn {
            return FinalizeOutcome::AlreadyFinalized;
        }
    }

    let pending = take_exact_pending_completion_admission(pending_admission, ledger_key);
    let entry = ledger.entry(ledger_key).or_insert(LedgerEntry {
        phase: Phase::Pending,
        relay_owner: RelayOwnerKind::None,
        provider,
        turn_key: key,
        terminal_deadline: None,
        // Orphan/terminal-first entries (no prior `register_start`) finalize
        // right here, so they never need the watcher far-backstop.
        watcher_backstop_deadline: None,
        watcher_backstop_probe_at: None,
        watcher_backstop_terminal_streak: 0,
        watcher_backstop_deadline_pulled: false,
        completion_admission: CompletionAdmission::new(CompletionAdmissionPlan::Immediate),
        finalized_at: None,
    });
    apply_pending_completion_admission(entry, pending);

    match entry.phase {
        Phase::Finalizing | Phase::Finalized => {
            return FinalizeOutcome::AlreadyFinalized;
        }
        Phase::Pending => {}
    }

    // #3646 OBSERVATION-ONLY (finalizer_ledger_owner companion): emit the
    // actor-owned ledger entry's `relay_owner` — the SECOND owner signal the
    // watcher-side `terminal_body_commit` event cannot read (the ledger lives on
    // this actor task; a synchronous cross-task query from the watcher would be
    // new behaviour). Keyed on the same `discord:<channel>:<user_msg_id>` turn id
    // so the two owner signals JOIN in PG and the #3607 "None-ledger vs
    // Watcher-finalize" ambiguity resolves. Read-only: it neither inspects nor
    // changes the clear_inflight / defer / finalize decision that follows.
    //
    // codex review #3678: key on the RESOLVED entry identity (`entry.turn_key`),
    // NOT the submitted `key`. A channel-only id-0 terminal collapses onto the
    // channel's real registered turn via `resolve_ledger_key`; that entry's
    // `turn_key` carries the real `user_msg_id`. Keying on the submitted `key`
    // would emit `user_msg_id=0` for exactly those collapsed terminals, dropping
    // the turn_id and breaking the JOIN against the watcher event — the #3607
    // cases this signal exists to disambiguate. Genuine orphans (id-0 with no
    // live entry) still carry id-0 here, which is correct (no real turn exists).
    super::super::relay_owner_observability::emit_finalizer_ledger_owner(
        entry.provider.as_str(),
        entry.turn_key.channel_id.get(),
        entry.turn_key.user_msg_id,
        entry.relay_owner.as_str(),
        terminal_event_kind_str(&event),
        ctx.clear_inflight,
    );

    // Gate-timeout with a still-busy pane AND a live relay owner is the only
    // event that defers instead of finalizing now.
    let mut effective_ctx = ctx;
    if let TerminalEvent::GateTimeout {
        pane_quiescent: Some(false),
    } = event
    {
        if entry.relay_owner != RelayOwnerKind::None {
            // Arm the backstop deadline ONCE. The watcher submits a
            // GateTimeout on every pass while the pane stays busy; if each
            // submission pushed the deadline forward by GATE_BACKSTOP the
            // backstop would never fire on a persistently busy pane (exactly
            // the never-finalizing bug). So only set it if not already armed.
            entry
                .terminal_deadline
                .get_or_insert_with(|| Instant::now() + GATE_BACKSTOP);
            return FinalizeOutcome::Deferred;
        }
        // No live relay owner → nothing will drive the pane to quiescence;
        // finalize now. This recovered/orphan watcher case (post-restart
        // inflight, no `register_start`) has no later watcher block to clear
        // inflight — the caller's `watcher()` submit SKIPS its cleanup block and
        // discards this outcome, so reproduce the deadline-armed
        // `gate_backstop()` context shape: clear inflight here (else the file
        // keeps blocking the channel after the mailbox release) and preserve the
        // queue-admission bit; actual drain is the #4048 `do_finalize` event.
        effective_ctx.clear_inflight = true;
        effective_ctx.kickoff_queue = true;
    }

    // Flip Pending → Finalizing, run the side-effects, flip → Finalized.
    entry.phase = Phase::Finalizing;
    let provider = entry.provider.clone();
    // Codex P1 — finalize on the RESOLVED identity: an id-0 terminal that
    // collapsed onto an entry registered with the real `user_msg_id` finalizes
    // under THAT identity, so `do_finalize` takes the guarded if-matches paths
    // instead of the unguarded channel-scoped finish (which could release a
    // newer turn's token when the entry is stale). Otherwise the same key.
    let finalize_key = if key.user_msg_id == 0 && entry.turn_key.user_msg_id != 0 {
        entry.turn_key
    } else {
        key
    };
    // #3866/#4048: `do_finalize` is the single chokepoint for finalize
    // side-effects (inflight clear, mailbox token release, `global_active`
    // decrement, voice drain, completion-event publish). Contain a panic HERE
    // rather than only at the actor loop so the Finalizing->Finalized flip below
    // STILL runs on a caught panic.
    // That matters: the entry was just flipped to `Finalizing`, and reconcile GC
    // reaps only `Finalized` while every backstop/probe gates on `Pending`, so an
    // entry left stuck in `Finalizing` after a panic would leak FOREVER and
    // poison `ledger_has_live_watcher_pending` / `resolve_channel_only` for this
    // channel+generation. Resetting it to `Finalized` (the normal post-finalize
    // flip) lets GC reap it and frees the channel for the next turn.
    let outcome = match AssertUnwindSafe(do_finalize(
        finalize_key,
        provider,
        &event,
        effective_ctx,
        claim_snapshot.as_ref(),
        shared,
    ))
    .catch_unwind()
    .await
    {
        Ok(outcome) => {
            note_mailbox_release_after_finalize(&outcome, entry, shared);
            outcome
        }
        Err(payload) => {
            tracing::error!(
                panic = %panic_payload_summary(payload.as_ref()),
                channel_id = ledger_key.channel_id.get(),
                user_msg_id = ledger_key.user_msg_id,
                "TurnFinalizer do_finalize panicked on the terminal path; contained, the \
                 ledger entry is reset Finalizing->Finalized below so it is never stuck (#3866)"
            );
            FinalizeOutcome::AlreadyFinalized
        }
    };
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(Instant::now());
        entry.terminal_deadline = None;
    }
    outcome
}
