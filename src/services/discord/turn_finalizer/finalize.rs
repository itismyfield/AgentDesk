//! #3894 — finalize side-effect chokepoint split out of `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): `do_finalize`, the single owner of finalize's
//! side-effects (inflight clear / mailbox token release / `global_active`
//! decrement / trailing channel cleanup / queue kickoff / relay-miss
//! observability, plus the #3866 in-finalize panic-injection test hook), lifted
//! verbatim. Re-imported by the parent (`use self::finalize::do_finalize`) so
//! `handle_terminal` and the reconcile/backstop child call it byte-identically.
//! Bare `super::` references to the discord module are rewritten to the absolute
//! `crate::services::discord::` path (house style, cf. `watcher_backstop.rs`) and
//! the sibling `cleanup` / `test_panic_hook` modules to `super::*` since the
//! module depth changed; the bodies are otherwise identical.

use super::*;

/// The single owner of finalize's side-effects. Reproduces today's exact
/// `clear_inflight (per-site) + mailbox_finish_turn + counter-- + trailing
/// terminal side-effects` sequence so each routed call-site stays
/// behaviourally identical during the incremental landing.
pub(super) async fn do_finalize(
    key: TurnKey,
    provider: ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    let channel_id = key.channel_id;

    // #3866: test-only injection point — fire a panic INSIDE the finalize
    // side-effect surface, AFTER the caller (`handle_terminal` /
    // `run_backstop_finalize`) flipped the entry to `Finalizing`, to prove the
    // caught-panic path still resets the entry to `Finalized` (never stuck
    // Finalizing) on BOTH the terminal and reconcile/backstop paths. No-op in
    // production builds.
    #[cfg(test)]
    super::test_panic_hook::maybe_panic_in_finalize();

    // #3866 residual (KNOWN, intentionally NOT fixed in this panic-guard pass —
    // tracked for a follow-up): (1) a poisoned mutex anywhere in the finalize
    // tree still propagates as a panic; it is now CONTAINED by the catch_unwind
    // guards (the loop survives, the entry is reset), but the underlying lock
    // stays poisoned. (2) The token-removal -> `saturating_decrement_global_active`
    // window below (B)->(C) is not atomic: a panic BETWEEN them caught by the
    // guard leaks one `global_active` count (the saturating decrement bounds the
    // blast radius to a single unit; it never underflows). (3) An id-0 channel-only
    // terminal that finalizes here gets the unguarded channel-scoped finish and no
    // per-submitter compensation, so a caught panic on that path cannot
    // selectively undo a partial finish. None of these block the panic-guard fix.
    //
    // #3350 ②: ensure the #3303 DeferredClaim marker for a watcher-owned TUI-direct
    // synthetic turn BEFORE (A) erases the row evidence. Codex r1-1: watcher
    // submitters cleared the row pre-submit, so for them this row re-load proves
    // nothing — their guarantee runs at submit time from the pre-clear snapshot
    // (`submit_terminal_with_claim_snapshot`); rationale/gates: cleanup.rs.
    super::cleanup::ensure_synthetic_claim_marker_before_clear(key, &provider, None);

    // (A) inflight clear. Only the gate-timeout backstop and the immediate
    //     no-owner restored-watcher path set `clear_inflight` (live bridge /
    //     watcher sites clear inline, pass `false`). They consolidate the
    //     pre-#3016 IDENTITY-GUARDED 1800s sweeper: a real identity clears via
    //     `clear_inflight_state_if_matches` — never a newer turn's inflight,
    //     preserving `PlannedRestartSkipped` / `RebindOriginSkipped`; a true
    //     orphan (id-0, nothing to authenticate) keeps the unguarded clear.
    if ctx.clear_inflight {
        if key.user_msg_id != 0 {
            let _ = crate::services::discord::inflight::clear_inflight_state_if_matches(
                &provider,
                channel_id.get(),
                key.user_msg_id,
            );
        } else {
            crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
        }
    }

    // (B) mailbox cancel_token release — the routed sites' single, idempotent
    //     `mailbox_finish_turn` (`removed_token = None` on a second call).
    //     #3016 root-cause: a real identity uses the IDENTITY-GUARDED finish so
    //     finalize only releases the token it owns — a stale channel-scoped
    //     terminal post-finalize/ledger-GC must not release the NEWER turn's
    //     token or decrement `global_active`. Ambiguous id-0 (recovery/orphan)
    //     keeps the channel-scoped finish (ledger gate + id-0 no-op bound it).
    let finish = if key.user_msg_id != 0 {
        crate::services::discord::mailbox_finish_turn_if_matches(
            shared,
            &provider,
            channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await
    } else {
        crate::services::discord::mailbox_finish_turn(shared, &provider, channel_id).await
    };

    if let Some(token) = finish.removed_token.as_ref() {
        // A normal completion releases lingering token observers via
        // `mark_completion_cleanup` so provider watchdogs don't treat the
        // post-terminal `cancelled` flip as a live mid-stream cancel. A real
        // cancel must NOT mark completion-cleanup; nor does the watcher path
        // (it historically only set `cancelled`).
        if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
            token.mark_completion_cleanup();
        }
        // Stop any lingering watchdog timer from firing on a newer turn's
        // token.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // (C) #3019 active-counter — decrement ONLY here, ONLY when this submission
    //     actually removed the active turn. Gating on `removed_token.is_some()`
    //     is what guarantees no underflow even under a transitional
    //     double-call.
    if finish.removed_token.is_some() {
        crate::services::discord::saturating_decrement_global_active(shared);
    }

    // The CHANNEL-SCOPED trailing side-effects (D)/(E) below mutate per-channel
    // routing/watchdog state that belongs to whatever turn is CURRENTLY active
    // in the channel. They are safe to run when this finalize actually finished
    // the turn (`removed_token.is_some()`), and harmlessly idempotent on the
    // legacy unguarded id-0 path (which always ran them). But when the
    // IDENTITY-GUARDED finish MISSED — a real `user_msg_id` that did NOT match
    // the live active turn, so `removed_token` is `None` — a DIFFERENT (newer)
    // turn owns the channel. Running these would clear the newer turn's
    // watchdog override, drop its `dispatch_thread_parents` / `dispatch_role_
    // overrides`, and drain its voice deferrals, corrupting a turn this stale
    // terminal does not own (Codex P2). So when the guard was used and missed,
    // skip the channel cleanup entirely — exactly as we already skip the token
    // release and counter decrement. (An id-0 orphan keeps today's behaviour.)
    let guarded_finish_missed = key.user_msg_id != 0 && finish.removed_token.is_none();

    let has_pending_after_voice = if guarded_finish_missed {
        // No-op finalize on a stale terminal: leave the live newer turn's
        // channel state untouched. Report NO backlog (Codex P2): the newer turn
        // is still active and owns its queue. Surfacing `finish.has_pending`
        // here would let the bridge propagate `has_queued_turns` and later drain
        // a queued soft message behind the live turn — concurrently dispatching
        // a follow-up this stale terminal does not own. A guarded miss is a true
        // no-op: no queue kickoff, no backlog reporting.
        false
    } else {
        // (D) trailing terminal side-effects that today follow
        //     `mailbox_finish_turn` inline at the bridge/watcher call-sites.
        //     Moved here so they cannot diverge between the routed paths.
        crate::services::discord::clear_watchdog_deadline_override(channel_id.get()).await;
        shared
            .dispatch
            .thread_parents
            .retain(|_, thread| *thread != channel_id);

        let voice_deferred_enqueued = if ctx.drain_voice {
            shared
                .voice_barge_in
                .drain_deferred_after_turn(shared, &provider, channel_id)
                .await
        } else {
            false
        };
        let has_pending_after_voice = finish.has_pending || voice_deferred_enqueued;
        if !has_pending_after_voice {
            shared.dispatch.role_overrides.remove(&channel_id);
        }

        // (E) optional deferred queue kickoff (watcher path), gated exactly as
        //     `finish_restored_watcher_active_turn` did.
        if ctx.kickoff_queue && finish.mailbox_online && has_pending_after_voice {
            // #3005: idle has just been confirmed on this finalize, so let the
            // first kickoff attempt run immediately (skipping the 2s pre-sleep)
            // instead of waiting the full deferred-drain INITIAL_DELAY before a
            // queued follow-up can start. Subsequent retries keep the existing
            // 2s cadence (e.g. if the hosted TUI is still transiently Busy).
            crate::services::discord::schedule_deferred_idle_queue_kickoff_immediate(
                shared.clone(),
                provider.clone(),
                channel_id,
                "turn_finalizer terminal completion with queued backlog",
            );
        }
        has_pending_after_voice
    };

    super::cleanup::finalized_reaction_lifecycle(key, event, ctx, shared, "finalized");

    // (F) relay-miss observability — emitted from inside the finalizer so the
    //     signal fires exactly once per finalize regardless of submitter.
    if matches!(event, TerminalEvent::RelayMiss) {
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            None,
            None,
            None,
            "relay_miss_finalized",
            serde_json::json!({
                "removed_token": finish.removed_token.is_some(),
                "has_pending": has_pending_after_voice,
            }),
        );
    }

    FinalizeOutcome::Finalized {
        removed_token: finish.removed_token,
        has_pending: has_pending_after_voice,
        mailbox_online: finish.mailbox_online,
    }
}
