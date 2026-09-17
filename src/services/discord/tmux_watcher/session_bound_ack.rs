//! #3479: the tmux watcher's session-bound terminal-ACK half — the ACK-outcome
//! enum + fold, the ACK snapshot resolvers, the watcher-direct-send gate, the
//! terminal re-send decision, the cross-watcher emission-slot RAII guard
//! (`RelaySlotGuard`), and the ACK delivery wait. The relay-forward half (incl.
//! the shared `SessionBoundRelayAckTarget` type this module borrows) lives in
//! the sibling `supervisor_relay` module; zero coupling to
//! `shared`/`http`/`InflightTurnState`. Items are `pub(super)` so the parent
//! watcher loop keeps calling them by their original names.

use super::super::WatcherTerminalKind;
use super::supervisor_relay::SessionBoundRelayAckTarget;

/// #3041 P1-5: the watcher's view of the session-bound terminal ACK. The
/// non-failure arms fold 1:1 onto the cross-actor 3-way `DeliveryOutcome`
/// (`Delivered`, `NotDelivered`); every failure/unconfirmed arm collapses to
/// `DeliveryOutcome::Unknown` for the resend decision (see
/// [`session_bound_ack_delivery_outcome`]) but stays distinct here so the
/// flight-recorder / metrics keep exact provenance. `NotAttempted` (#3579) is
/// the benign watcher-owned non-attempt sentinel — distinct from
/// `MissingTarget` (ack-wait ran but had no target) so metrics can exclude it,
/// though both fold to `Unknown` for the resend decision.
///
/// §3.2 SAFETY INVARIANT: `NotDelivered` and every `Unknown`-class arm route
/// through `watcher_terminal_resend_action` — no blind skip, no blind re-send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionBoundRelayAckOutcome {
    Delivered,
    FreshDelivered {
        committed_to: Option<u64>,
        persistence_recorded: bool,
    },
    NotDelivered,
    RingUnknown,
    Dropped,
    SinkError,
    TimedOut,
    MissingTarget,
    /// #3579: watcher-owned non-attempt sentinel (see type doc).
    NotAttempted,
}

/// #3041 P1-5: collapse the watcher ACK onto the canonical cross-actor 3-way
/// `DeliveryOutcome` for the resend DECISION — §3.2 reconciliation treats
/// `NotDelivered` and `Unknown` identically, so this fold guarantees neither
/// gets a blind fast-path.
pub(super) fn session_bound_ack_delivery_outcome(
    ack_outcome: SessionBoundRelayAckOutcome,
) -> crate::services::cluster::stream_relay::DeliveryOutcome {
    use crate::services::cluster::stream_relay::DeliveryOutcome;
    match ack_outcome {
        SessionBoundRelayAckOutcome::Delivered => DeliveryOutcome::Delivered,
        SessionBoundRelayAckOutcome::FreshDelivered {
            committed_to,
            persistence_recorded,
        } => DeliveryOutcome::FreshDelivered {
            committed_to,
            persistence_recorded,
        },
        SessionBoundRelayAckOutcome::NotDelivered => DeliveryOutcome::NotDelivered,
        SessionBoundRelayAckOutcome::RingUnknown
        | SessionBoundRelayAckOutcome::Dropped
        | SessionBoundRelayAckOutcome::SinkError
        | SessionBoundRelayAckOutcome::TimedOut
        | SessionBoundRelayAckOutcome::MissingTarget
        // #3579: folds like `MissingTarget` — only the provenance label differs.
        | SessionBoundRelayAckOutcome::NotAttempted => DeliveryOutcome::Unknown,
    }
}

pub(super) fn sequence_reached(latest: Option<u64>, target: u64) -> bool {
    latest.is_some_and(|sequence| sequence >= target)
}

pub(super) fn session_bound_relay_ack_snapshot_outcome(
    target: Option<&SessionBoundRelayAckTarget>,
) -> Option<SessionBoundRelayAckOutcome> {
    use crate::services::cluster::stream_relay::DeliveryOutcome;
    let target = target?;
    // #3041 P1-3 R5: resolve the ACK on THIS frame's own sequence, not the `>=`
    // high-water-mark — two turns sharing a physical chunk would otherwise let
    // the later one's commit falsely mark the earlier one Delivered.
    match target
        .metrics
        .terminal_outcome_for_sequence(target.sequence)
    {
        Some(DeliveryOutcome::Delivered) => {
            return Some(SessionBoundRelayAckOutcome::Delivered);
        }
        Some(DeliveryOutcome::FreshDelivered {
            committed_to,
            persistence_recorded,
        }) => {
            return Some(SessionBoundRelayAckOutcome::FreshDelivered {
                committed_to,
                persistence_recorded,
            });
        }
        Some(DeliveryOutcome::NotDelivered) => {
            return Some(SessionBoundRelayAckOutcome::NotDelivered);
        }
        // An explicit ring `Unknown` resolves immediately instead of waiting
        // out the ACK timeout — folds the same as `NotDelivered`, just faster.
        Some(DeliveryOutcome::Unknown) => {
            return Some(SessionBoundRelayAckOutcome::RingUnknown);
        }
        None => {}
    }
    // Sink-error / drop remain high-water-mark, per-sequence-monotonic signals.
    let snapshot = target.metrics.snapshot();
    if sequence_reached(snapshot.last_sink_error_sequence, target.sequence) {
        return Some(SessionBoundRelayAckOutcome::SinkError);
    }
    if sequence_reached(snapshot.last_dropped_sequence, target.sequence) {
        return Some(SessionBoundRelayAckOutcome::Dropped);
    }
    None
}

pub(super) fn session_bound_relay_frame_ack_reached(
    target: Option<&SessionBoundRelayAckTarget>,
) -> bool {
    let Some(target) = target else {
        return false;
    };
    let snapshot = target.metrics.snapshot();
    sequence_reached(snapshot.last_delivered_sequence, target.sequence)
}

pub(super) fn session_bound_relay_drop_reached_since_first_forward(
    target: Option<&SessionBoundRelayAckTarget>,
    first_forwarded_sequence: Option<u64>,
) -> bool {
    let (Some(target), Some(first_forwarded_sequence)) = (target, first_forwarded_sequence) else {
        return false;
    };
    let snapshot = target.metrics.snapshot();
    sequence_reached(snapshot.last_dropped_sequence, first_forwarded_sequence)
}

pub(super) fn session_bound_ack_outcome_after_resolve_time_mirror_check(
    ack_outcome: SessionBoundRelayAckOutcome,
    turn_fully_mirrored: &mut bool,
    target: Option<&SessionBoundRelayAckTarget>,
    first_forwarded_sequence: Option<u64>,
) -> SessionBoundRelayAckOutcome {
    if !session_bound_relay_drop_reached_since_first_forward(target, first_forwarded_sequence) {
        return ack_outcome;
    }
    *turn_fully_mirrored = false;
    if matches!(ack_outcome, SessionBoundRelayAckOutcome::Delivered) {
        SessionBoundRelayAckOutcome::Dropped
    } else {
        ack_outcome
    }
}

pub(super) fn session_bound_ack_confirms_transport(
    ack_outcome: SessionBoundRelayAckOutcome,
) -> bool {
    matches!(
        ack_outcome,
        SessionBoundRelayAckOutcome::Delivered | SessionBoundRelayAckOutcome::FreshDelivered { .. }
    )
}

pub(super) fn watcher_should_direct_send_after_session_bound_ack(
    should_direct_send: bool,
    ack_outcome: SessionBoundRelayAckOutcome,
    relay_owner_present: bool,
) -> bool {
    // `relay_owner_present` is unused for the decision (kept for the telemetry
    // call site) — every non-`Delivered` outcome routes through
    // `watcher_terminal_resend_action`'s reconciliation regardless of owner.
    let _ = relay_owner_present;
    // Only the PRECONDITION GATE for a re-send; the actual skip-vs-resend
    // decision is masked downstream by `watcher_terminal_resend_action`.
    should_direct_send && !session_bound_ack_confirms_transport(ack_outcome)
}

/// A soft transcript boundary is not a self-authenticating Discord turn: a
/// `/compact` rewrite can expose historical assistant entries followed by
/// `stop_hook_summary` records to a recovering watcher, so the soft marker
/// alone must not authorize a fresh POST once the durable inflight row is
/// gone or ownerless — it must be authenticated against the pre-frame inflight
/// identity by the caller (anchors/leases are insufficient; a newer turn can
/// create them while backlog is parsed). Hard provider results keep the
/// existing recovery fallback since they are explicit terminal events.
pub(super) fn watcher_direct_fallback_has_turn_authority(
    terminal_kind: Option<WatcherTerminalKind>,
    soft_turn_authority: bool,
) -> bool {
    let soft_terminal = matches!(
        terminal_kind,
        Some(WatcherTerminalKind::SoftStopHookSummary | WatcherTerminalKind::SoftUserBoundary)
    );
    !soft_terminal || soft_turn_authority
}

/// #3041 P1-3 (§3.2): the watcher's terminal re-send decision after a
/// non-`Delivered` session-bound ACK, reconciled against `committed_relay_offset`
/// instead of blindly re-sending: `committed >= end` → already delivered → Skip;
/// `committed < end` → re-send the FULL response (no SendSuffix — the watcher's
/// `response_sent_offset` is a different coordinate system from the JSONL byte
/// `committed`/`start`/`end`, and the all-or-nothing sink delegation means the
/// partial-overlap case doesn't occur, so SendFull is safe).
///
/// #3151 (deferred): a 10s ACK wait can elapse while the sink's POST is still
/// in flight → SendFull → a duplicate if that POST later succeeds. Not a
/// black-hole; the in-flight sink-delivery marker (#3151) closes this window.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum WatcherTerminalResendAction {
    /// `committed >= end`: the whole range is already delivered. Do NOT re-send.
    SkipAlreadyCommitted,
    /// `committed < end`: not (fully) covered — re-send the full response.
    SendFull,
    /// #3151: a sink POST is genuinely in flight (`Leased{Sink, fresh}`) — don't
    /// re-send this pass, let the next terminal pass re-evaluate. Bounded by
    /// `DELIVERY_LEASE_DEADLINE_MS`: the sink either commits+releases (→ Skip)
    /// or dies (→ reclaim + SendFull).
    WaitInFlight,
}

/// #3151: gate the re-send on the in-flight sink-delivery marker before
/// deferring to [`watcher_terminal_resend_action`]. A live `Leased{Sink}`
/// waits this pass (bounded, re-checked next pass); past its deadline, or for
/// `Committed{Sink}`/any other holder, it falls through to committed-offset
/// reconciliation (#3159: `Committed{Sink}` is not assumed delivered).
///
/// Returns `(action, reclaim_expired_sink)`; when true the caller must call
/// `reclaim_if_expired(now_ms)` before sending.
pub(super) fn watcher_terminal_resend_action_gated(
    snapshot: &crate::services::discord::LeaseSnapshot,
    committed: u64,
    start: u64,
    end: u64,
    now_ms: u64,
) -> (WatcherTerminalResendAction, bool) {
    use crate::services::discord::{LeaseHolder, LeaseSnapshot};
    match snapshot {
        LeaseSnapshot::Leased {
            holder: LeaseHolder::Sink,
            deadline_ms,
            ..
        } => {
            if now_ms < *deadline_ms {
                (WatcherTerminalResendAction::WaitInFlight, false)
            } else {
                (watcher_terminal_resend_action(committed, start, end), true)
            }
        }
        LeaseSnapshot::Committed {
            holder: LeaseHolder::Sink,
            ..
        } => {
            // #3159: not assumed delivered — route through the same
            // committed-offset reconciliation as any other fallthrough.
            (watcher_terminal_resend_action(committed, start, end), false)
        }
        // #3151 marker doesn't apply to a non-Sink holder / Unleased.
        _ => (watcher_terminal_resend_action(committed, start, end), false),
    }
}

/// Reconcile a watcher terminal re-send against the committed offset authority.
/// Only consulted when the watcher would have re-sent (non-`Delivered` ACK and
/// a real body); the caller still applies the existing `relay_owner`
/// suppression. A zero/inverted range (`end <= start`) yields `SendFull` — the
/// reconciliation never manufactures a skip for a range it can't reason about.
pub(super) fn watcher_terminal_resend_action(
    committed: u64,
    start: u64,
    end: u64,
) -> WatcherTerminalResendAction {
    if end <= start {
        // Degenerate range: defer to the existing no-range handling downstream.
        return WatcherTerminalResendAction::SendFull;
    }
    if committed >= end {
        WatcherTerminalResendAction::SkipAlreadyCommitted
    } else {
        WatcherTerminalResendAction::SendFull
    }
}

pub(super) fn watcher_terminal_response_for_direct_send<'a>(
    full_response: &'a str,
    response_sent_offset: usize,
    session_bound_fallback_uses_full_body: bool,
) -> &'a str {
    // Full body: committed-offset authority is byte-range based while sink
    // delegation is all-or-nothing; a suffix would splice unrelated coordinates.
    if session_bound_fallback_uses_full_body {
        return full_response;
    }
    full_response.get(response_sent_offset..).unwrap_or("")
}

pub(super) fn watcher_should_send_ordered_new_chunks_for_terminal_fallback(
    session_bound_fallback_uses_full_body: bool,
    relay_text: &str,
) -> bool {
    session_bound_fallback_uses_full_body
        && crate::services::discord::formatting::needs_multiple_messages(relay_text)
}

/// #2840: RAII guard for the cross-watcher emission slot (`relay_coord.relay_slot`,
/// an `Arc<AtomicU64>`: 0 = free, non-zero = a watcher is mid-emission). Shared
/// across every watcher instance for a channel/session, so an early-return /
/// `?` / panic / task-abort between CAS-acquire and `store(0)` would otherwise
/// wedge the slot forever. Releases on Drop so any exit path frees it; the two
/// intended in-loop release points still call `release()` explicitly to
/// preserve exact timing (site 1 releases *before* a 500ms backoff sleep), and
/// the idempotent `released` flag makes the trailing Drop a no-op after that.
pub(super) struct RelaySlotGuard {
    slot: std::sync::Arc<std::sync::atomic::AtomicU64>,
    released: bool,
}

impl RelaySlotGuard {
    pub(super) fn new(slot: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            slot,
            released: false,
        }
    }

    pub(super) fn release(&mut self) {
        if !self.released {
            self.slot.store(0, std::sync::atomic::Ordering::Release);
            self.released = true;
        }
    }
}

impl Drop for RelaySlotGuard {
    fn drop(&mut self) {
        if !self.released {
            // #2841: Drop without an explicit release() means an abnormal exit;
            // a replacement watcher may re-emit the same range (bounded).
            tracing::warn!(
                target: "agentdesk::relay_flight_recorder",
                "relay emission slot freed via Drop on abnormal exit (in-flight send outcome unknown); a replacement watcher may re-emit the same range — resolved by the delivery lease"
            );
        }
        self.release();
    }
}

/// Bound the slot-held watcher terminal emission. Serenity's HTTP client can
/// otherwise sit forever in transport wait / 429 retry, keeping
/// `TmuxRelayCoord::relay_slot` non-zero and wedging later watcher passes.
pub(super) const WATCHER_RELAY_EMISSION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(120);

pub(super) async fn watcher_relay_emission_with_timeout<T>(
    emission: impl std::future::Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(WATCHER_RELAY_EMISSION_TIMEOUT, emission).await
}

/// Bound post-commit chrome while the emission slot is still held. The terminal
/// body is already committed by this phase, so timeout means "skip cosmetic
/// completion work" rather than "rewind and retry terminal delivery".
pub(super) const WATCHER_RELAY_COMPLETION_CHROME_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);

pub(super) async fn watcher_completion_chrome_with_timeout<T>(
    chrome_step: impl std::future::Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(WATCHER_RELAY_COMPLETION_CHROME_TIMEOUT, chrome_step).await
}

pub(super) fn warn_watcher_completion_chrome_timeout(
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: poise::serenity_prelude::ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
    chrome_step: &'static str,
) {
    tracing::warn!(
        provider = %watcher_provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        data_start_offset,
        current_offset,
        chrome_step,
        timeout_secs = WATCHER_RELAY_COMPLETION_CHROME_TIMEOUT.as_secs(),
        "watcher: completion chrome step timed out after terminal body commit; skipping remaining chrome without rewind"
    );
}

pub(super) fn watcher_relay_emission_timeout_failure_plan(
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: poise::serenity_prelude::ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
) -> crate::services::discord::replace_outcome_policy::WatcherTerminalRelayPlan {
    tracing::warn!(
        provider = %watcher_provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        data_start_offset,
        current_offset,
        timeout_secs = WATCHER_RELAY_EMISSION_TIMEOUT.as_secs(),
        "watcher: terminal relay emission timed out; treating as failed-undelivered for retry"
    );
    crate::services::discord::replace_outcome_policy::watcher_send_failure_retry_plan(
        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient,
    )
}

pub(super) async fn wait_for_session_bound_relay_delivery_ack(
    target: Option<&SessionBoundRelayAckTarget>,
    timeout: std::time::Duration,
) -> SessionBoundRelayAckOutcome {
    if target.is_none() {
        return SessionBoundRelayAckOutcome::MissingTarget;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(outcome) = session_bound_relay_ack_snapshot_outcome(target) {
            return outcome;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return SessionBoundRelayAckOutcome::TimedOut;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25).min(deadline - now)).await;
    }
}

#[cfg(test)]
#[path = "session_bound_ack_tests.rs"]
mod tests;
