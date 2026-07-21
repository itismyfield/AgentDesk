//! #3038 S1 tmux watcher turn identity and explicit cleanup helpers.

use super::*;

/// #2427 D/A wires — emit an explicit-signal inflight cleanup attempt.
///
/// Used by the TurnCompleted broadcast and the dead-pane post-mortem
/// path. The on-disk inflight is guarded so that:
///   * stale signals arriving after a new turn has written its own
///     inflight do not delete the new turn's file (Pitfall #1);
///   * planned-restart markers (`restart_mode = Some(_)`) survive across
///     the dcserver restart they were saved for;
///   * `rebind_origin` rows owned by the rebind API are not touched
///     (Pitfall #5).
///
/// All outcomes are logged at trace/info level so the sweeper safety-net
/// strikes are easy to spot when this hook misses.
pub(in crate::services::discord) fn emit_explicit_inflight_cleanup_signal(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: u64,
    reason: &'static str,
) {
    let outcome = crate::services::discord::inflight::clear_inflight_state_if_matches(
        provider,
        channel_id.get(),
        expected_user_msg_id,
    );
    log_explicit_inflight_cleanup_outcome(
        provider,
        channel_id,
        expected_user_msg_id,
        reason,
        outcome,
    );
}

pub(super) fn log_explicit_inflight_cleanup_outcome(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: u64,
    reason: &'static str,
    outcome: crate::services::discord::inflight::GuardedClearOutcome,
) {
    match outcome {
        crate::services::discord::inflight::GuardedClearOutcome::Cleared => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                user_msg_id = expected_user_msg_id,
                reason = reason,
                "[{ts}] 🧹 inflight cleared via explicit completion signal (#2427)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::Missing => {
            tracing::trace!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                reason = reason,
                "inflight already absent — explicit signal redundant (#2427)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                expected_user_msg_id = expected_user_msg_id,
                reason = reason,
                "[{ts}] ⚠ inflight user_msg_id mismatch — stale explicit signal ignored (#2427 Pitfall #1)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::PlannedRestartSkipped => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                reason = reason,
                "skipping explicit inflight cleanup — planned-restart marker present (#2427)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::RebindOriginSkipped => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                reason = reason,
                "skipping explicit inflight cleanup — rebind_origin row (#2427 Pitfall #5)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::IoError => {
            // Surfaces filesystem failures explicitly so the operator can
            // see the sweeper's 1800s safety-net is the only thing
            // catching the failed cleanup. Caller does not clear watcher.
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                reason = reason,
                "explicit inflight cleanup failed with IO error — sweeper safety-net will retry"
            );
        }
    }
}

/// #2427 A wire — synchronous variant for the dead-pane post-mortem,
/// which runs on a `spawn_blocking` thread.
///
/// Codex round-2 HIGH-1: a naïve "load → re-feed user_msg_id" guard is
/// self-authenticating (a new turn's inflight matches itself). To make
/// the guard meaningful for the pane-death path, we also require the
/// loaded inflight to point at the *same dead tmux session* the caller
/// witnessed. If a fresh `start_claude` respawn already replaced the
/// inflight with one tied to a new (live) tmux name, we leave it alone
/// — the new turn's pane is alive, and its inflight does not belong to
/// us to clear.
pub(in crate::services::discord) fn emit_explicit_inflight_cleanup_signal_pane_dead(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_tmux_session_name: &str,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) {
    let Some(state) =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if state.tmux_session_name.as_deref() != Some(expected_tmux_session_name) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            on_disk = ?state.tmux_session_name,
            expected = expected_tmux_session_name,
            "[{ts}] skipping pane-dead explicit cleanup — inflight points at a different tmux session (#2427 A self-auth guard)"
        );
        return;
    }
    let Some(identity) = expected_identity else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            expected_tmux_session_name,
            "pane-dead inflight cleanup skipped because watcher attach identity is unavailable (#2450)"
        );
        return;
    };
    let outcome = crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        identity,
    );
    log_explicit_inflight_cleanup_outcome(
        provider,
        channel_id,
        state.user_msg_id,
        "pane_dead",
        outcome,
    );
}

pub(super) fn matching_watcher_turn_identity(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<crate::services::discord::inflight::InflightTurnIdentity> {
    state
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .map(crate::services::discord::inflight::InflightTurnIdentity::from_state)
}

pub(super) fn matching_watcher_turn_nonce(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<String> {
    state
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .and_then(|state| state.turn_nonce.clone())
}

/// #3016 (codex R2): pick the `user_msg_id` handed to the normal-completion
/// finalize, gated on the OUTPUT-RANGE relationship so we only ever finalize
/// the turn whose output THIS completion actually is.
///
/// Offset-aliasing hazard: the watcher loop is not turn-scoped, and the
/// watcher-yield guard `watcher_should_yield_to_inflight_state`
/// (tmux.rs ~2083-2112) lets the watcher PROCEED on this old range in the
/// `RelayOwnerKind::None` arm whenever it does NOT satisfy
/// `data_start_offset <= turn_start_offset && turn_start_offset < current_offset`
/// (tmux.rs:2110-2111). One such non-yield case is a FOLLOW-UP turn started on
/// the SAME tmux session whose `turn_start_offset >= current_offset` — i.e. it
/// begins AFTER the range this completion covers. In that case
/// `inflight_before_relay` already holds the NEWER turn's `user_msg_id`; handing
/// that id to the finalizer would `mailbox_finish_turn_if_matches` and release
/// the WRONG (newer, still-running) turn.
///
/// Binding rule (mirrors the guard's exact offset semantics so the two cannot
/// disagree): only return the pinned id when the pinned inflight turn has
/// actually produced output by this completion point — its effective start
/// offset `turn_start_offset.unwrap_or(last_offset)` is `< current_offset`. A
/// newer turn (start offset `>= current_offset`) does NOT satisfy this → return
/// `0` (no exact ledger match; the finalizer refuses to release a mismatched
/// live turn). The session-match + `user_msg_id != 0` checks are kept too.
///
/// Note: `InflightTurnIdentity` (inflight.rs:665) does NOT carry
/// `turn_start_offset`, so this reads it from the `InflightTurnState` directly.
pub(super) fn pinned_finalize_user_msg_id(
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> u64 {
    inflight_before_relay
        .filter(|state| {
            state.user_msg_id != 0
                && state.tmux_session_name.as_deref().map(str::trim)
                    == Some(tmux_session_name.trim())
                // Mirror the guard at tmux.rs:2110-2111: effective turn start =
                // `turn_start_offset.unwrap_or(last_offset)`. Only this turn's
                // output reaches `current_offset` when its start precedes it.
                && state.turn_start_offset.unwrap_or(state.last_offset) < current_offset
        })
        .map(|state| state.user_msg_id)
        .unwrap_or(0)
}

pub(super) fn pinned_finalizer_turn_id(
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> u64 {
    inflight_before_relay
        .filter(|state| {
            state.tmux_session_name.as_deref().map(str::trim) == Some(tmux_session_name.trim())
                && state.turn_start_offset.unwrap_or(state.last_offset) < current_offset
        })
        .map(|state| state.effective_finalizer_turn_id())
        .unwrap_or(0)
}

pub(super) fn pinned_delivery_lease_key(
    channel_id: poise::serenity_prelude::ChannelId,
    generation: u64,
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> crate::services::discord::DeliveryLeaseKey {
    if let Some(state) = inflight_before_relay.filter(|state| {
        state.tmux_session_name.as_deref().map(str::trim) == Some(tmux_session_name.trim())
            && state.turn_start_offset.unwrap_or(state.last_offset) < current_offset
    }) {
        crate::services::discord::DeliveryLeaseKey::from_inflight_state_for_site(
            channel_id, generation, state, "watcher",
        )
    } else {
        crate::services::discord::DeliveryLeaseKey::new_for_site(
            channel_id, generation, 0, None, None, "watcher",
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherDirectTerminalResponseDecision {
    Empty,
    Send,
    RefusedDegenerateDuplicate,
}

impl WatcherDirectTerminalResponseDecision {
    pub(super) fn has_sendable_body(self) -> bool {
        matches!(self, Self::Send)
    }

    pub(super) fn refused_duplicate(self) -> bool {
        matches!(self, Self::RefusedDegenerateDuplicate)
    }
}

/// Pure core of the #4081/#4714 degenerate-key duplicate decision: the guard
/// refuses ONLY a byte-identical content re-post (`duplicate`) that has no fresh
/// in-range assistant text AND no pending user turn awaiting a response. Split
/// out so the refusal logic is unit-tested directly without the global-state /
/// delivery-record I/O the wrapper performs.
pub(super) fn degenerate_duplicate_refuses_delivery(
    duplicate: bool,
    fresh_assistant_text_in_observed_range: bool,
    pending_user_boundary: bool,
) -> bool {
    // #4714: a pending user boundary means a live follow-up turn is awaiting a
    // response — never suppress it. Only a true re-post (no boundary, no fresh
    // text) is a #4081 phantom duplicate.
    duplicate && !fresh_assistant_text_in_observed_range && !pending_user_boundary
}

/// #4081 introduced this degenerate-key content-fingerprint guard to block the
/// phantom RE-RELAY of the immediately-prior (already-delivered) response at a
/// no-inflight soft boundary: with no inflight identity the lease key degenerates
/// to `id-0`, and if the body byte-matches a recently-delivered fingerprint AND
/// no fresh assistant text appears in the observed range, the watcher would
/// re-post the same answer. That refusal is correct ONLY when no user turn is
/// actually awaiting a response.
///
/// #4714 (the over-fire this guard now corrects): when the prior watcher-owned
/// turn reaches terminal but its terminal submission / inflight / dispatch
/// identity are lost (#3277 backstop path), a genuinely NEW follow-up user turn
/// also runs under the degenerate `id-0` key. If that follow-up's body collides
/// with the prior fingerprint and no fresh in-range assistant text is seen yet,
/// the #4081 guard misjudged it as a duplicate and refused delivery — stranding
/// the live channel with `route="duplicate_guard_refused"` and zero user-visible
/// response (both placeholder and streaming suppressed).
///
/// The discriminator is a PENDING user boundary. A prompt anchor / external-input
/// lease is recorded when a user turn arrives and CLEARED once that turn's
/// response is delivered. Normally a #4081 phantom re-relay has no boundary,
/// while a #4714 follow-up does, so the latter must not be refused.
///
/// Accepted cross-turn edge: the boundary is a single latest-turn slot, not an
/// identity carried by this degenerate response. If turn A is delivered, turn B
/// overwrites the slot, and A's phantom body is reconsidered while B is pending,
/// A inherits B's boundary and can be sent once more. Neither the content
/// fingerprint (which intentionally matches both a phantom and a legitimate
/// byte-identical follow-up) nor the slot's message id / lease generation can
/// identify the response after this path has lost its turn identity. Refusing
/// that ambiguous overlap would recreate #4714 and strand B, so delivery wins.
/// The exposure is bounded to a #4081 phantom overlapping a concurrently pending
/// second message; ordinary re-posts with no pending boundary remain refused.
///
/// The boundary is read here (not threaded from the caller) to keep the hot
/// `tmux_watcher.rs` relay-loop call site byte-identical (#3016 hot-file rule);
/// this function already performs delivery-record I/O, so the extra
/// `tui_prompt_dedupe` read is consistent with its impurity.
pub(super) fn watcher_direct_terminal_response_decision(
    provider: &ProviderKind,
    channel_id: ChannelId,
    generation: u64,
    tmux_session_name: &str,
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    current_offset: u64,
    fresh_assistant_text_in_observed_range: bool,
    response: &str,
) -> WatcherDirectTerminalResponseDecision {
    if response.trim().is_empty() {
        return WatcherDirectTerminalResponseDecision::Empty;
    }
    let key = pinned_delivery_lease_key(
        channel_id,
        generation,
        inflight_before_relay,
        tmux_session_name,
        current_offset,
    );
    let duplicate = key.is_degenerate_legacy()
        && crate::services::discord::outbound::delivery_record::recent_delivered_content_matches(
            provider,
            channel_id,
            tmux_session_name,
            response,
        );
    // #4714: a pending prompt anchor or external-input lease marks a live
    // follow-up turn awaiting a response — mirrors the caller's
    // `prompt_anchor_present_before_relay || external_input_lease_before_relay`.
    let pending_user_boundary = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
    )
    .is_some()
        || crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
            provider.as_str(),
            tmux_session_name,
            channel_id.get(),
        );
    if degenerate_duplicate_refuses_delivery(
        duplicate,
        fresh_assistant_text_in_observed_range,
        pending_user_boundary,
    ) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            response_len = response.len(),
            fresh_assistant_text_in_observed_range,
            pending_user_boundary,
            "watcher: suppressed degenerate-key duplicate terminal response by content fingerprint"
        );
        return WatcherDirectTerminalResponseDecision::RefusedDegenerateDuplicate;
    }
    WatcherDirectTerminalResponseDecision::Send
}

pub(super) fn pinned_watcher_delivery_lease_identity(
    channel_id: ChannelId,
    generation: u64,
    watcher_instance_id: u64,
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> (
    crate::services::discord::turn_finalizer::TurnKey,
    crate::services::discord::DeliveryLeaseKey,
    crate::services::discord::LeaseHolder,
) {
    (
        crate::services::discord::turn_finalizer::TurnKey::new(
            channel_id,
            pinned_finalizer_turn_id(inflight_before_relay, tmux_session_name, current_offset),
            generation,
        ),
        pinned_delivery_lease_key(
            channel_id,
            generation,
            inflight_before_relay,
            tmux_session_name,
            current_offset,
        ),
        crate::services::discord::LeaseHolder::Watcher {
            instance_id: watcher_instance_id,
        },
    )
}

pub(super) fn try_acquire_watcher_delivery_lease(
    cell: &crate::services::discord::DeliveryLeaseCell,
    holder: crate::services::discord::LeaseHolder,
    key: &crate::services::discord::DeliveryLeaseKey,
    start: u64,
    end: u64,
) -> bool {
    cell.reclaim_if_expired(crate::services::discord::lease_now_ms());
    cell.try_acquire(
        key.clone(),
        holder,
        start,
        end,
        crate::services::discord::lease_now_ms().saturating_add(WATCHER_DELIVERY_LEASE_DEADLINE_MS),
    )
}

pub(super) fn watcher_delivery_lease_heartbeat(
    acquired: bool,
    cell: std::sync::Arc<crate::services::discord::DeliveryLeaseCell>,
    holder: crate::services::discord::LeaseHolder,
    key: &crate::services::discord::DeliveryLeaseKey,
) -> Option<DeliveryLeaseHeartbeat> {
    acquired.then(|| DeliveryLeaseHeartbeat::spawn(cell, holder, key.clone()))
}

pub(super) fn should_submit_restored_watcher_finalize(
    completion_is_stale_for_newer_turn: bool,
    restored_finalizer_turn_id: u64,
) -> bool {
    !completion_is_stale_for_newer_turn && restored_finalizer_turn_id != 0
}

/// #3016 (codex R3): the watcher's `terminal_output_committed &&
/// !lifecycle_stage_paused` block runs MORE destructive side-effects than the
/// finalize on a LATE re-read `inflight_state` (loaded AFTER the relay, NOT
/// turn-pinned): the `⏳ → ✅` reaction + `session_transcript` + `turn_analytics`
/// write (targets the late read's `user_msg_id`) and `clear_inflight_state`
/// (deletes the on-disk inflight). In the R2/R3 aliasing scenario a FOLLOW-UP
/// turn on the SAME tmux session has `turn_start_offset >= current_offset` (it
/// begins AFTER the output range this completion covers), so the watcher-yield
/// guard (tmux.rs:2110-2111: yields only when
/// `data_start_offset <= turn_start_offset && turn_start_offset < current_offset`)
/// does NOT yield and the watcher processes this OLD range — yet the late
/// `inflight_state` (and possibly the pre-relay snapshot) already holds the
/// NEWER turn's id. Running those side-effects would ✅ the newer (still-running)
/// turn's message, write its transcript/analytics prematurely, and delete its
/// inflight — wrong-turn lifecycle corruption.
///
/// This pure gate returns TRUE iff EITHER snapshot is a real NEWER turn on the
/// SAME session that this committed range does not belong to: for that snapshot
/// `user_msg_id != 0` AND trimmed session match AND effective start
/// `turn_start_offset.unwrap_or(last_offset) >= current_offset`. This is the
/// EXACT complement of `pinned_finalize_user_msg_id`'s `< current_offset` range
/// test (and mirrors the same offset/fallback semantics as the yield guard), so
/// the two decisions cannot disagree: when the finalize helper returns 0 because
/// the snapshot is a newer turn, this gate returns TRUE and the call site skips
/// the reaction/transcript/analytics/clear too.
///
/// Narrow by construction: for a normal completion where the inflight is THIS
/// turn or an OLDER turn (`turn_start_offset < current_offset`), or there is no
/// inflight, or it is `rebind_origin`/`user_msg_id == 0`, this returns FALSE and
/// all existing behavior is preserved.
pub(super) fn committed_completion_is_stale_for_newer_turn(
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    inflight_state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> bool {
    let snapshot_is_newer_turn =
        |snapshot: Option<&crate::services::discord::inflight::InflightTurnState>| {
            snapshot.is_some_and(|state| {
                state.user_msg_id != 0
                    && state.tmux_session_name.as_deref().map(str::trim)
                        == Some(tmux_session_name.trim())
                    // Complement of `pinned_finalize_user_msg_id`'s
                    // `< current_offset`: a newer turn starts AT/AFTER this
                    // committed range. Same `turn_start_offset.unwrap_or(last_offset)`
                    // fallback as the finalize helper and the yield guard.
                    && state.turn_start_offset.unwrap_or(state.last_offset) >= current_offset
            })
        };
    snapshot_is_newer_turn(inflight_before_relay) || snapshot_is_newer_turn(inflight_state)
}

/// #3142: sibling of [`committed_completion_is_stale_for_newer_turn`] for the
/// ANCHOR-CLEANUP branches of the committed-output block (the
/// `should_complete_tui_direct_anchor_lifecycle` first branch and the
/// `injected_prompt_message_id` task-notification branch). Those branches act on
/// an anchor identity that can belong to a `user_msg_id == 0` external-input /
/// injected-anchor newer turn — a case the id!=0 sibling DELIBERATELY excludes
/// (its `user_msg_id != 0` filter protects the finalize/clear contract against
/// the id-0 channel-collapse warned about at the clear site). So this helper
/// must be the id==0-INCLUSIVE variant for the anchor branches ONLY.
///
/// Detection rule (mirrors the watcher-yield guard tmux.rs:2110-2111 offset test
/// exactly, like the sibling): a snapshot is a stale NEWER turn iff trimmed
/// session match AND effective start
/// `turn_start_offset.unwrap_or(last_offset) >= current_offset` AND the snapshot
/// is anchor-relevant — it carries a real anchor/external identity
/// (`user_msg_id != 0` OR `injected_prompt_message_id.is_some()` OR it represents
/// external input). The anchor-relevance disjunct ensures a truly empty/no-anchor
/// row is NOT spuriously treated as a stale newer turn (such a row also fails the
/// branch's own `watcher_inflight_needs_anchor_lifecycle_cleanup` precondition).
/// OR-ed across both snapshots exactly like the sibling so the pre-relay snapshot
/// and the late re-read are both checked.
///
/// Narrow by construction: for a normal anchor completion (the inflight is THIS
/// or an OLDER turn with `start < current_offset`, absent, or on a different
/// session) this returns FALSE → the anchor cleanup runs as today.
pub(super) fn committed_anchor_cleanup_is_stale_for_newer_turn(
    inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
    inflight_state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> bool {
    let snapshot_is_newer_anchor_turn =
        |snapshot: Option<&crate::services::discord::inflight::InflightTurnState>| {
            snapshot.is_some_and(|state| {
                (state.user_msg_id != 0
                    || state.injected_prompt_message_id.is_some()
                    || watcher_inflight_represents_external_input(Some(state)))
                    && state.tmux_session_name.as_deref().map(str::trim)
                        == Some(tmux_session_name.trim())
                    // Same `turn_start_offset.unwrap_or(last_offset)` fallback as
                    // the sibling helper and the yield guard.
                    && state.turn_start_offset.unwrap_or(state.last_offset) >= current_offset
            })
        };
    snapshot_is_newer_anchor_turn(inflight_before_relay)
        || snapshot_is_newer_anchor_turn(inflight_state)
}

pub(super) fn refresh_watcher_turn_identity(
    current: &mut Option<crate::services::discord::inflight::InflightTurnIdentity>,
    current_turn_nonce: &mut Option<String>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_offset: u64,
) {
    let inflight =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
    *current = matching_watcher_turn_identity(inflight.as_ref(), tmux_session_name);
    let Some(state) = inflight.as_ref().filter(|state| {
        state.tmux_session_name.as_deref().map(str::trim) == Some(tmux_session_name.trim())
    }) else {
        *current_turn_nonce = None;
        return;
    };
    let row_start_offset = state.turn_start_offset.unwrap_or(state.last_offset);
    let fresh_bind_at_start = current_turn_nonce.is_none() && row_start_offset == current_offset;
    if row_start_offset < current_offset || fresh_bind_at_start {
        // `current_offset` is the watcher loop's already-consumed transcript byte
        // offset. Keep the prior nonce for rows that start at/after that point:
        // they are follow-ups whose output this watcher has not consumed yet.
        // A fresh watcher binding at the exact start boundary has no prior nonce,
        // so adopting that row is the initial observed-turn bind.
        *current_turn_nonce = state.turn_nonce.clone();
    }
}

#[cfg(test)]
#[path = "turn_identity_tests.rs"]
mod pane_dead_identity_tests;
