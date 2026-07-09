use super::*;

pub(super) const STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS: i64 = 120;

#[derive(Clone, Copy)]
struct StaleMailboxRelease {
    had_pending_queue: bool,
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
            super::super::super::turn_finalizer::FinalizeContext::watcher(),
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
///     This is the row-ABSENT reclaim: liveness is uncertain (no row to inspect),
///     so we only steal a long-stuck mailbox. For a real owner, an absent row is
///     reclaimable ONLY when the in-memory ledger records this exact re-adopted
///     mailbox (owner + `active_user_message_id`) — see
///     `classify_reclaimable_mailbox_owner`.
///   - `OwnerInflightFinalized` — NO age gate, reclaimed immediately. The reason
///     requires `terminal_delivery_committed == true`, which means the owner's
///     prose AND its completion UI (`⏳→✅` reaction, footer, transcript/analytics)
///     were ALREADY emitted by the watcher terminal-commit pass — before any
///     finalizer handoff — so reclaiming (releasing) the stuck mailbox cannot lose
///     output or suppress the footer. (The reclaim submits `Cancel` through
///     `FinalizeContext::watcher()`, whose `backstop_cleanup` is false, so it
///     schedules no reaction change; #4370 F3(b).) A 120s gate here would defeat
///     the fix — the observed task-notification loss occurred ~79s after restart.
///   - `OwnerInflightReplaced`  — age `>= 120s` REQUIRED. NOTE (#4370 F5): this
///     reason is UNREACHABLE for a re-adopted real owner. A superseding turn writes
///     a FRESH row (marker `false`, and it is not in the ledger under this id), so
///     `classify_reclaimable_mailbox_owner` returns `None` before this reason is
///     consulted. It stays reachable only for the #4018 synthetic owner.
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
///         THIS `(provider, channel_id)` as re-adopted with the SAME owner AND the
///         SAME `active_user_message_id`. The on-disk marker cannot be used here
///         (the row is gone, and on a DrainRestart row it may never have persisted
///         — the identity-refresh save refuses `restart_mode` rows), so the ledger
///         is the authority. A NEW/live turn owns a different `active_user_message_id`
///         and therefore can never match the ledger entry — the live-turn-theft
///         guard — and the resulting `OwnerInflightAbsent` reason still enforces the
///         `>= 120s` age gate.
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
    match state {
        Some(state) => (state.readopted_from_inflight
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
