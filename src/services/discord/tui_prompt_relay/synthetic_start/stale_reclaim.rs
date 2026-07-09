use super::*;

/// #4370 R3-4 — the positive-staleness age gate, and the timing tradeoff it buys.
///
/// Which stuck-mailbox shape a starved follow-up frees, and WHEN:
///   - PRESENT row + `terminal_delivery_committed` → `OwnerInflightFinalized`,
///     reclaimed IMMEDIATELY (no age gate). This is the shape the #4370 incident
///     took, so the sub-120s follow-up loss (the ~79s task-notification drop) is
///     freed at once.
///   - ABSENT row + ledger `finished` (#4370 R3-1) → `OwnerInflightAbsent`,
///     reclaimed only after `>= 120s`.
///
/// Note a re-adopted turn's `turn_started_at` is RESET to the re-adopt time
/// (`turn_orchestrator.rs`), so this gate measures age-since-re-adopt, not the
/// turn's true wall age — a follow-up arriving <120s after re-adopt does not free
/// an ABSENT-row mailbox. That is acceptable because the incident shape is the
/// PRESENT-row `Finalized` path above, which has no gate.
///
/// Why the ABSENT-row path keeps the gate even though R3-1's `finished` bit now
/// makes it a POSITIVE liveness proof (so the gate is, strictly, no longer
/// required for correctness — a `finished` absent row is as safe to reclaim as a
/// present `Finalized` row): we keep it as DEFENSE-IN-DEPTH. The `finished` bit is
/// a brand-new invariant set on one production path (the watcher terminal-commit
/// clear); until it has soaked, the age gate is a cheap second barrier against a
/// mis-set bit stealing a live turn (safety > speed). The cost is at most 120s of
/// extra latency on the RARE absent-row-finished shape — and never on the incident
/// shape, which the present-row `Finalized` path already frees immediately. If the
/// `finished` invariant proves out, a later change may drop this gate for a
/// `finished` absent row (matching the present-row `Finalized` rule) and cite this
/// note.
pub(super) const STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS: i64 = 120;

#[derive(Clone, Copy)]
struct StaleMailboxRelease {
    had_pending_queue: bool,
}

/// #4370 R3-3: the SINGLE finalize context the stale-owner reclaim submits.
/// Production (`finalize_stale_mailbox_owner_if_current`) and the F3(b)
/// chrome-survival test both read THIS, so the test observes the context the
/// reclaim ACTUALLY passes instead of re-fabricating a standalone `watcher()`
/// (which would pass even if the production call site were re-pointed at a
/// backstop-cleanup context). Its shape — `clear_inflight == false`,
/// `kickoff_queue == false` — is exactly what makes `finalized_reaction_lifecycle`'s
/// `backstop_cleanup` false (`turn_finalizer/cleanup.rs:54-62`), so a `Cancel`
/// reclaim schedules NO reaction change and cannot suppress the re-adopted turn's
/// already-pending completion footer / `✅`.
pub(super) fn reclaim_finalize_context() -> crate::services::discord::turn_finalizer::FinalizeContext
{
    crate::services::discord::turn_finalizer::FinalizeContext::watcher()
}

async fn finalize_stale_mailbox_owner_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    active_user_message_id: MessageId,
) -> Option<StaleMailboxRelease> {
    let outcome = shared
        .turn_finalizer
        .submit_terminal(
            super::super::super::turn_finalizer::TurnKey::new(
                channel_id,
                active_user_message_id.get(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::super::turn_finalizer::TerminalEvent::Cancel,
            reclaim_finalize_context(),
            shared.clone(),
        )
        .await;

    let super::super::super::turn_finalizer::FinalizeOutcome::Finalized {
        removed_token: Some(token),
        has_pending,
        ..
    } = outcome
    else {
        return None;
    };
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Some(StaleMailboxRelease {
        had_pending_queue: has_pending,
    })
}

pub(in crate::services::discord::tui_prompt_relay) async fn release_stale_ownerless_tui_direct_mailbox_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    anchor_message_id: MessageId,
) -> bool {
    let Some(state) =
        super::super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return false;
    };
    if state.user_msg_id != active_user_message_id.get()
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || !super::super::super::inflight::ownerless_external_input_inflight_is_stale(&state)
    {
        return false;
    }

    let Some(release) = finalize_stale_mailbox_owner_if_current(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await
    else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            "TUI-direct stale ownerless mailbox release skipped because mailbox identity changed"
        );
        return false;
    };
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        global_active_decremented = true,
        had_pending_queue = release.had_pending_queue,
        "released stale ownerless TUI-direct mailbox before claiming new synthetic inflight"
    );
    true
}

#[derive(Clone, Copy)]
enum StaleSyntheticReclaimReason {
    OwnerInflightAbsent,
    OwnerInflightReplaced,
    OwnerInflightFinalized,
}

impl StaleSyntheticReclaimReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::OwnerInflightAbsent => "owner_inflight_absent",
            Self::OwnerInflightReplaced => "owner_inflight_replaced",
            Self::OwnerInflightFinalized => "owner_inflight_finalized",
        }
    }

    fn requires_positive_owner_age(self) -> bool {
        matches!(
            self,
            Self::OwnerInflightAbsent | Self::OwnerInflightReplaced
        )
    }
}

fn owner_age_permits_positive_stale_reclaim(
    turn_started_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    let Some(turn_started_at) = turn_started_at else {
        return false;
    };
    chrono::Utc::now()
        .signed_duration_since(turn_started_at)
        .num_seconds()
        >= STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS
}

fn stale_synthetic_mailbox_owner_reclaim_reason(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
) -> Option<StaleSyntheticReclaimReason> {
    let Some(state) = state else {
        return Some(StaleSyntheticReclaimReason::OwnerInflightAbsent);
    };
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    };
    if state.user_msg_id != active_user_message_id.get() {
        return Some(StaleSyntheticReclaimReason::OwnerInflightReplaced);
    }
    state
        .terminal_delivery_committed
        .then_some(StaleSyntheticReclaimReason::OwnerInflightFinalized)
}

/// #4370: which class of mailbox owner is eligible for the stale-owner reclaim.
///
/// #4018 keyed reclaim to the well-known synthetic relay owner. Restart recovery,
/// however, re-adopts the REAL user turn (mailbox owner == `request_owner_user_id`)
/// from persisted inflight, so that path was unreachable and the follow-up
/// injection / task-notification synthetic turns starved for relay ownership. This
/// widens eligibility to a re-adopted-from-inflight real-user owner.
///
/// The two classes share the SAME reclaim reasons and the SAME positive-staleness
/// gate, but note precisely how the gate applies per reason:
///   - `OwnerInflightAbsent`   — age `>= 120s` REQUIRED (`requires_positive_owner_age`).
///     This is the row-ABSENT reclaim: with no row to inspect, liveness is proven
///     ONLY by the in-memory ledger. For a real owner an absent row is reclaimable
///     ONLY when the ledger records this exact re-adopted mailbox (owner +
///     `active_user_message_id`) AND that entry is `finished` — i.e. the turn's
///     terminal delivery committed (#4370 R3-1; stamped at the watcher
///     terminal-commit clear in `tmux_watcher/terminal_commit_epilogue.rs`, the
///     same production path that produces the absent-row shape). The `finished`
///     bit is the row-ABSENT analogue of the present row's
///     `terminal_delivery_committed`, so this arm can no longer steal a LIVE
///     re-adopted turn whose row is merely absent. The `>= 120s` gate is kept on
///     top as defense-in-depth (see R3-4) — `classify_reclaimable_mailbox_owner`.
///   - `OwnerInflightFinalized` — NO age gate, reclaimed immediately. The reason
///     requires `terminal_delivery_committed == true`, which means ONLY that the
///     owner's assistant PROSE was already relayed. It does NOT mean the completion
///     CHROME has rendered: the watcher stamps `terminal_delivery_committed`
///     (`tmux_watcher.rs:2352`) BEFORE it edits the completion footer / status
///     panel (`tmux_watcher.rs:2628`) and BEFORE it emits the `✅` reaction +
///     transcript + analytics (`tmux_watcher.rs:3102`), so a reclaim CAN fire in
///     the window between the commit and that chrome. It still cannot SUPPRESS the
///     chrome: the reclaim submits `Cancel` through the shared
///     `reclaim_finalize_context()` (== `FinalizeContext::watcher()`,
///     `finalize_context.rs:60-68`, with `clear_inflight:false` +
///     `kickoff_queue:false`), so in `finalized_reaction_lifecycle`
///     (`turn_finalizer/cleanup.rs:54-62`) `backstop_cleanup` is false and — the
///     owner being Watcher-kind, not `StandbyRelay` — the helper early-returns
///     without scheduling ANY reaction change; the still-pending footer / `✅`
///     render on the watcher's own pass (#4370 F3(b)). A 120s gate here would
///     defeat the fix — the observed task-notification loss occurred ~79s after
///     restart (this present-row `Finalized` shape is what covers the sub-120s
///     follow-up loss; see R3-4).
///   - `OwnerInflightReplaced`  — age `>= 120s` REQUIRED. UNREACHABLE for a
///     re-adopted real owner, for TWO independent reasons — state both, because an
///     earlier revision of this note cited only the first and it is NOT the
///     load-bearing one (fresh-Claude r3 #1):
///       (a) a superseding turn writes a FRESH row (marker `false`, absent from the
///           ledger under this id), so `classify_reclaimable_mailbox_owner` returns
///           `None` before this reason is consulted; and
///       (b) — the one that actually matters — this reason fires on
///           `state.user_msg_id != active_user_message_id`, and
///           `active_user_message_id` IS `effective_finalizer_turn_id()`, which
///           equals `user_msg_id` whenever that id is non-zero. An id-0 row would
///           make the two diverge and misfire this reason on a LIVE turn, so id-0
///           rows are refused at BOTH ends: recovery never records them
///           (`readopted_ledger_record_allowed`) and `classify_reclaimable_mailbox_owner`
///           never classifies them.
///     It stays reachable only for the #4018 synthetic owner.
#[derive(Clone, Copy)]
enum ReclaimableMailboxOwner {
    /// #4018 — the TUI-direct synthetic relay owner.
    Synthetic,
    /// #4370 — a real-user turn re-adopted from persisted inflight (restart /
    /// mid-execution reattach).
    ReadoptedFromInflight,
}

impl ReclaimableMailboxOwner {
    fn as_str(self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic_owner",
            Self::ReadoptedFromInflight => "readopted_from_inflight_real_owner",
        }
    }
}

/// Classify the CURRENT mailbox owner. Reclaim is only ever considered for:
///   - the synthetic relay owner (#4018), or
///   - a real-user turn this process re-adopted from persisted inflight (#4370),
///     proven per row shape:
///       * PRESENT row → the on-disk `readopted_from_inflight` marker AND a request
///         owner matching the live mailbox owner.
///       * ABSENT row (Path B) → the in-memory `readopted_mailbox_ledger` records
///         THIS `(provider, channel_id)` as re-adopted with the SAME owner, the
///         SAME `active_user_message_id`, AND `finished == true` (its terminal
///         delivery committed; #4370 R3-1). The on-disk marker cannot be used here
///         (the row is gone, and on a DrainRestart row it may never have persisted
///         — the identity-refresh save refuses `restart_mode` rows), so the ledger
///         is the authority. Two independent guards keep this from stealing a live
///         turn: a NEW/live turn owns a different `active_user_message_id` and can
///         never match the entry, and a re-adopted turn that is still LIVE (terminal
///         delivery not committed) is `finished == false` and is refused. The
///         resulting `OwnerInflightAbsent` reason then still enforces the `>= 120s`
///         age gate on top.
///
/// An arbitrary real-user turn (no marker, not in the ledger) is NEVER reclaimable.
fn classify_reclaimable_mailbox_owner(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    active_request_owner: Option<serenity::UserId>,
    active_user_message_id: MessageId,
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> Option<ReclaimableMailboxOwner> {
    let owner = active_request_owner?;
    if owner == serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID) {
        return Some(ReclaimableMailboxOwner::Synthetic);
    }
    // A real-user owner: eligibility depends on the row shape.
    //
    // `user_msg_id != 0` is load-bearing, not cosmetic (fresh-Claude r3 #1). The
    // mailbox's `active_user_message_id` is the turn's `effective_finalizer_turn_id()`,
    // which equals `user_msg_id` ONLY when that id is non-zero. For an id-0 row the
    // two diverge, so `stale_synthetic_mailbox_owner_reclaim_reason` would read
    // `state.user_msg_id != active_user_message_id` and misfire `OwnerInflightReplaced`
    // on a turn that is still LIVE — reclaiming it once it aged past 120s. Recovery
    // already refuses to record such rows (`readopted_ledger_record_allowed`); this is
    // the matching refusal at the consumption site, so the invariant is enforced at
    // both ends instead of being assumed.
    match state {
        Some(state) => (state.readopted_from_inflight
            && state.user_msg_id != 0
            && state.request_owner_user_id == owner.get())
        .then_some(ReclaimableMailboxOwner::ReadoptedFromInflight),
        None => shared
            .is_readopted_mailbox_owner(
                provider,
                channel_id.get(),
                owner.get(),
                active_user_message_id.get(),
            )
            .then_some(ReclaimableMailboxOwner::ReadoptedFromInflight),
    }
}

pub(super) async fn release_reclaimable_stale_synthetic_mailbox_owner_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    active_request_owner: Option<serenity::UserId>,
    active_turn_kind: crate::services::turn_orchestrator::ActiveTurnKind,
    turn_started_at: Option<chrono::DateTime<chrono::Utc>>,
    anchor_message_id: MessageId,
) -> bool {
    if active_turn_kind.is_monitor_auto_turn() {
        return false;
    }
    let state = super::super::super::inflight::load_inflight_state(provider, channel_id.get());
    let Some(owner_kind) = classify_reclaimable_mailbox_owner(
        shared,
        provider,
        channel_id,
        active_request_owner,
        active_user_message_id,
        state.as_ref(),
    ) else {
        return false;
    };
    let Some(reason) = stale_synthetic_mailbox_owner_reclaim_reason(
        state.as_ref(),
        tmux_session_name,
        active_user_message_id,
    ) else {
        // #4370: a re-adopted-from-inflight real-user turn still owns the mailbox
        // and looks live (matching id, not `terminal_delivery_committed`).
        // Deferring is correct — we must not steal a live turn — but record it so a
        // long-lived stuck owner is not silent (#4260-style; upgraded from the
        // caller's per-attempt trace).
        if matches!(owner_kind, ReclaimableMailboxOwner::ReadoptedFromInflight) {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                stale_user_message_id = active_user_message_id.get(),
                anchor_message_id = anchor_message_id.get(),
                "re-adopted-from-inflight real-user turn still owns the mailbox and is not stale; deferring synthetic relay turn (#4370)"
            );
        }
        return false;
    };
    if reason.requires_positive_owner_age()
        && !owner_age_permits_positive_stale_reclaim(turn_started_at)
    {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            reclaim_reason = reason.as_str(),
            reclaimable_owner = owner_kind.as_str(),
            min_owner_age_secs = STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS,
            "skipping TUI-direct synthetic mailbox reclaim; owner age has not positively crossed the stale threshold"
        );
        return false;
    }

    let Some(release) = finalize_stale_mailbox_owner_if_current(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await
    else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            reclaim_reason = reason.as_str(),
            reclaimable_owner = owner_kind.as_str(),
            "TUI-direct stale synthetic mailbox reclaim skipped because mailbox identity changed"
        );
        return false;
    };
    // #4370: the mailbox is now freed, so the ledger entry for this re-adopted
    // owner can no longer be correct — drop it (stale entries are already inert
    // because a successor turn owns a different id, but eviction keeps the map
    // bounded and makes the reclaim edge explicit). A no-op for the #4018
    // synthetic owner, which was never recorded.
    shared.evict_readopted_mailbox_owner(provider, channel_id.get());
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        reclaim_reason = reason.as_str(),
        reclaimable_owner = owner_kind.as_str(),
        global_active_decremented = true,
        had_pending_queue = release.had_pending_queue,
        "reclaimed stale TUI-direct mailbox owner before claiming new synthetic inflight (#4370: covers re-adopted-from-inflight real-user owners)"
    );
    true
}
