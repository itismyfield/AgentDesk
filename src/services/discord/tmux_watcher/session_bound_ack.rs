//! #3479 Phase-1 rank-1 extraction (2/2): the tmux watcher's session-bound
//! terminal-ACK half — the ACK-outcome enum + fold, the per-sequence ACK
//! snapshot resolvers, the watcher-direct-send gate, the terminal re-send
//! decision (`WatcherTerminalResendAction` + the in-flight-sink-marker gate),
//! the cross-watcher emission-slot RAII guard (`RelaySlotGuard`), and the ACK
//! delivery wait. PURE MOVE from `tmux_watcher.rs` (zero logic change) to shrink
//! the frozen root file below its maintainability baseline.
//!
//! The relay-forward half (incl. the shared `SessionBoundRelayAckTarget` type
//! this module borrows) lives in the sibling `supervisor_relay` module; the
//! split is only to keep each child within the
//! `src/services/discord/tmux_watcher/**` 700-line namespace cap. ZERO coupling
//! to `shared`/`http`/`InflightTurnState`. Items are `pub(super)` so the parent
//! watcher loop keeps calling them by their original names.

use super::supervisor_relay::SessionBoundRelayAckTarget;

/// #3041 P1-5: the watcher's view of the session-bound terminal ACK. The
/// non-failure arms fold 1:1 onto the cross-actor 3-way `DeliveryOutcome`:
///   * `Delivered`      ← ring `DeliveryOutcome::Delivered`
///   * `NotDelivered`   ← ring `DeliveryOutcome::NotDelivered` (the former
///                        `TerminalSkipped`; a deterministic sink decline)
///   * the failure/unconfirmed arms (`Unknown`-class) — `RingUnknown` (the ring
///     recorded an explicit `Unknown`: sink POSTed without confirming),
///     `Dropped`, `SinkError`, `TimedOut`, `MissingTarget` — ALL collapse to
///     `DeliveryOutcome::Unknown` for the resend DECISION (see
///     [`session_bound_ack_delivery_outcome`]). They stay DISTINCT variants here
///     so the flight-recorder / metrics keep their exact provenance.
///
/// #3579: `NotAttempted` is the watcher-owned NON-attempt sentinel. It is the
/// INIT value of `session_bound_ack_outcome` (tmux_watcher.rs) and is what the
/// flight recorder logs as `frame_ack_outcome` when the session-bound ack-wait
/// block was SKIPPED entirely — i.e. `session_bound_relay_should_own_terminal_delivery`
/// returned false (typically `relay_owner=Watcher`, so the WATCHER itself owns the
/// terminal delivery and the sink-delegated ack path is intentionally not taken).
/// This is a BENIGN, expected steady-state — NOT a relay loss. Before #3579 the
/// init was `MissingTarget`, which conflated this watcher-owned non-attempt with
/// the genuine `wait_for_session_bound_relay_delivery_ack` `target.is_none()`
/// failure path, inflating operator/audit relay-loss tallies. `NotAttempted` is
/// distinct precisely so the recorder/metrics can EXCLUDE it as benign while
/// `MissingTarget` keeps meaning "ack-wait ran but had no target" (a real
/// unconfirmed). For the resend DECISION it folds to `DeliveryOutcome::Unknown`
/// IDENTICALLY to `MissingTarget` — behavior is unchanged; only the provenance
/// label/aggregation classification differs.
///
/// §3.2 SAFETY INVARIANT: BOTH `NotDelivered` AND every `Unknown`-class arm route
/// through `watcher_terminal_resend_action` (committed-offset reconciliation).
/// There is NO blind skip for `NotDelivered` and NO blind 10s re-send for any
/// `Unknown`-class arm.
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
    /// #3579: the session-bound ack-wait was never attempted (watcher-owned
    /// terminal delivery). Benign steady-state, distinct from `MissingTarget`.
    NotAttempted,
}

/// #3041 P1-5: collapse the watcher ACK onto the canonical cross-actor 3-way
/// `DeliveryOutcome` for the resend DECISION. `Delivered` → delivered (no resend);
/// `NotDelivered` → not-delivered (reconcile); every failure/unconfirmed arm →
/// `Unknown` (reconcile). The §3.2 reconciliation treats `NotDelivered` and
/// `Unknown` IDENTICALLY (both consult the committed offset → SendFull-or-Skip),
/// so this fold is what guarantees neither gets a blind fast-path.
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
        // #3579: the watcher-owned non-attempt folds to `Unknown` IDENTICALLY to
        // `MissingTarget` for the resend DECISION (committed-offset reconciliation),
        // so behavior is preserved exactly — only the provenance label differs.
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
    // #3041 P1-3 R5 (per-sequence terminal-ACK correlation): resolve the terminal
    // ACK on THIS watcher's OWN terminal frame (`target.sequence`) EXACT outcome,
    // NOT the `>=` high-water-mark. When two turns share a physical chunk (turn A
    // frame seq N, turn B tail seq N+1), B committing bumps the high-water-mark to
    // N+1; the old `committed >= N` test would then falsely report A as Delivered
    // even when A's own terminal frame was SKIPPED — black-holing A. Keying the
    // ACK to A's exact sequence decouples A from B: A reads outcome[N] (its own
    // result), B reads outcome[N+1]. `None` (not yet resolved / dropped / evicted)
    // falls through so a dropped/lagging frame keeps waiting → eventually TimedOut
    // → the watcher reconciles against the committed offset (no false ACK).
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
        // #3041 P1-5: an explicit ring `Unknown` (sink POSTed but could not confirm
        // the commit) RESOLVES the per-sequence ACK immediately to a `RingUnknown`
        // — the watcher reconciles against the committed offset NOW instead of
        // waiting out the 10s ACK timeout. `RingUnknown` folds to
        // `DeliveryOutcome::Unknown`, which §3.2 treats exactly like `NotDelivered`
        // (committed-offset SendFull-or-Skip), so this is a faster path to the SAME
        // safe reconciliation — never a blind re-send.
        Some(DeliveryOutcome::Unknown) => {
            return Some(SessionBoundRelayAckOutcome::RingUnknown);
        }
        None => {}
    }
    // Sink-error / drop remain high-water-mark signals (terminal outcome was never
    // recorded for this sequence in those paths): they are per-sequence-monotonic
    // failure markers, not a co-chunked-turn confusion vector.
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
    // #3042 (relay-stability P1, OBSOLETE band-aid — removed by #3041 P1-5): #3042
    // early-`return false`d an ownerless (post-restart restore_inflight gap)
    // `TimedOut` to blanket-suppress the watcher-direct fallback (rationale: the sink
    // "may have posted but failed to advance the committed metric" → blind re-send
    // → 3× duplicate). No longer holds: #3041 P1-3 Part (a)
    // (`advance_offset_for_confirmed_delegated_terminal`, session_relay_sink.rs ~459)
    // now couples a CONFIRMED sink POST to advancing `confirmed_end_offset` to the
    // fenced `end`; a `TimedOut` (not `MissingTarget`) is ONLY produced for a FENCED
    // forwarded frame (~2038/2053), the same fence the sink advances on, so the
    // committed offset reflects a confirmed post even ownerless (owner-independent
    // atomic). The blanket suppression is thus obsolete and HARMFUL — it returned
    // before §3.2 committed-offset reconciliation, so an ownerless `TimedOut` with
    // committed < end neither reconciled nor resent (black-hole). Routing through
    // §3.2 instead: committed >= end → `SkipAlreadyCommitted` (the 3× duplicate is
    // prevented PRINCIPALLY); committed < end → `SendFull` (black-hole closed). This
    // completes the P1-5 §3.2 invariant: EVERY non-`Delivered` outcome routes through
    // committed-offset reconciliation — none blind-skips/resends. (`relay_owner_present`
    // stays in the signature for the telemetry call site though the gate ignores it.)
    let _ = relay_owner_present;
    // #3041 P1-5: decide on the cross-actor 3-way `DeliveryOutcome` instead of the
    // implicit `ack_outcome != Delivered` bit. `Delivered` → no watcher re-send.
    // `NotDelivered` AND `Unknown` (every failure/unconfirmed arm) BOTH intend a
    // re-send here — but that intent is only the PRECONDITION GATE; the actual send
    // is masked downstream by `watcher_terminal_resend_action` (committed-offset
    // reconciliation), so neither gets a blind skip (NotDelivered) nor a blind
    // re-send (Unknown). §3.2 SAFETY INVARIANT.
    should_direct_send && !session_bound_ack_confirms_transport(ack_outcome)
}

/// #3041 P1-3 (Part b, §3.2): the watcher's terminal re-send DECISION after a
/// non-`Delivered` session-bound ACK, reconciled against the offset authority
/// (`committed_relay_offset`) instead of BLINDLY re-sending (removes the 10s
/// `relay_terminal_ack_timeout` duplicate vector). `committed >= end` → already
/// delivered (ACK merely lagged) → SKIP; `committed < end` → re-send the FULL
/// response (no black-hole).
///
/// codex BLOCKER 2 (no SendSuffix for the watcher path): the watcher delivers
/// RESPONSE TEXT sliced by `response_sent_offset` — a DIFFERENT coordinate
/// system from the JSONL byte `committed`/`start`/`end` — so a suffix slice
/// cannot be mapped correctly. The sink-delegated terminal delivery is also
/// ALL-OR-NOTHING (the sink advances to the FULL `end` only after one confirmed
/// `replace_message_with_outcome`), so the partial-overlap middle case
/// effectively does not occur and SendFull on `committed < end` is SAFE. Only
/// the watcher response-text path is restricted to Skip/Full; the idle-relay
/// path keeps its real JSONL suffix re-read.
///
/// #3041 P1-3 issue 4 (DEFERRED, #3151): the 10s ACK wait can elapse while the
/// sink's POST is still IN FLIGHT → `committed < end` → SendFull → a duplicate
/// when the in-flight POST later succeeds. Not a regression (the pre-P1-3 path
/// also re-sent on timeout; SendFull IS the retry, no black-hole); the full fix
/// is the in-flight sink-delivery marker tracked by #3151.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum WatcherTerminalResendAction {
    /// `committed >= end`: the whole range is already delivered. Do NOT re-send.
    SkipAlreadyCommitted,
    /// `committed < end`: the range is not (fully) covered — re-send the full
    /// response. See the type doc for why no partial-suffix variant exists for
    /// the watcher response-text path (coordinate mismatch + all-or-nothing sink).
    ///
    /// #3041 P1-3 (codex P1-3 issue 4 — DEFERRED, #3151): this arm also fires when
    /// the sink's POST is still IN FLIGHT at the 10s ACK timeout (committed has not
    /// advanced yet) → a duplicate once that POST succeeds. No regression (the
    /// pre-P1-3 path re-sent on timeout too) and no black-hole (SendFull is the
    /// retry). The remaining slow-sink-in-flight duplicate is closed by the future
    /// in-flight sink-delivery marker tracked in #3151.
    SendFull,
    /// #3151: a sink POST is genuinely IN FLIGHT for this range (the per-channel
    /// `DeliveryLeaseCell` is `Leased{Sink, fresh}`). The watcher must NOT re-send
    /// this pass — neither SendFull nor a Skip-log — and let its NEXT terminal pass
    /// re-evaluate. This is a BOUNDED wait: each pass re-reads the cell, and within
    /// at most one `DELIVERY_LEASE_DEADLINE_MS` the sink either commits+releases
    /// (→ committed >= end → Skip) or dies (→ deadline lapses → reclaim + SendFull).
    /// No busy-loop is introduced — it rides the existing watcher iteration cadence.
    WaitInFlight,
}

/// #3151: gate the watcher terminal re-send on the in-flight sink-delivery marker
/// BEFORE deferring to [`watcher_terminal_resend_action`]. The marker is a
/// `Leased{Sink, ..}` state on the per-channel `DeliveryLeaseCell`; this reads a
/// coherent `snapshot` (materialized under the cell's payload mutex) and decides:
///
/// - `Leased{Sink}` AND `now_ms < deadline_ms` → [`WatcherTerminalResendAction::WaitInFlight`]
///   (a sink POST is genuinely in flight — do not re-send this pass).
/// - `Leased{Sink}` AND `now_ms >= deadline_ms` → RECLAIM (the caller force-clears
///   the dead sink's marker via `reclaim_if_expired`) then fall through to
///   `watcher_terminal_resend_action` → `SendFull` (committed < end). No black-hole.
/// - `Committed{Sink}` (sink committed its terminal decision, not yet released) →
///   route through the committed-offset reconciliation: `committed >= end` (a real
///   Delivered commit) → Skip, `committed < end` (a NotDelivered / refused-advance
///   commit) → SendFull (re-send; no black-hole). #3159 BUG 1: the marker is no
///   longer blindly treated as delivered.
/// - ANY non-Sink holder / `Unleased` / committed-covered → behave EXACTLY as today:
///   defer to `watcher_terminal_resend_action`. The gate ONLY interposes for a
///   Sink-held lease, so the watcher-direct B2 path is untouched.
///
/// Returns `(action, reclaim_expired_sink)`. When `reclaim_expired_sink` is true
/// the caller MUST call `reclaim_if_expired(now_ms)` on the cell before sending
/// (the side effect is kept out of this pure decision fn so it stays unit-testable).
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
                // Live, in-flight sink POST — wait this pass (bounded by deadline).
                (WatcherTerminalResendAction::WaitInFlight, false)
            } else {
                // Dead/stalled sink — reclaim its marker and re-send (no black-hole).
                (watcher_terminal_resend_action(committed, start, end), true)
            }
        }
        LeaseSnapshot::Committed {
            holder: LeaseHolder::Sink,
            ..
        } => {
            // #3159 BUG 1: a Committed{Sink} marker is NO LONGER assumed delivered.
            // Route through the committed-offset reconciliation; committed >= end
            // (a real Delivered commit, which advanced the offset BEFORE committing)
            // → SkipAlreadyCommitted (unchanged), committed < end (a NotDelivered /
            // refused-advance commit) → SendFull (the range was NOT delivered, so
            // re-send; no black-hole). This also subsumes the Drop-release fallback
            // (Unleased + committed < end → SendFull). The committed offset is the
            // sole delivered-test, so a genuinely-delivered range is never re-sent.
            (watcher_terminal_resend_action(committed, start, end), false)
        }
        // Unleased, or held/committed by a non-Sink holder (Watcher/Bridge): the
        // #3151 marker does not apply — behave exactly as the pre-#3151 path.
        _ => (watcher_terminal_resend_action(committed, start, end), false),
    }
}

/// Reconcile a watcher terminal re-send against the committed offset authority.
/// Only ever consulted when the watcher WOULD have re-sent (a non-`Delivered`
/// ACK and a real body); the caller still applies the existing `relay_owner`
/// suppression. A zero/inverted range (`end <= start`) yields `SendFull` so the
/// existing zero-range guards (which never lease/advance) stay in control — the
/// reconciliation never manufactures a skip for a range it cannot reason about.
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
        // committed < end (incl. the partial `start < committed < end` case which
        // the all-or-nothing sink delegation does not actually produce): re-send
        // the FULL response. No black-hole; no mis-offset suffix (codex BLOCKER 2).
        WatcherTerminalResendAction::SendFull
    }
}

pub(super) fn watcher_terminal_response_for_direct_send<'a>(
    full_response: &'a str,
    response_sent_offset: usize,
    session_bound_fallback_uses_full_body: bool,
) -> &'a str {
    // Reconciled re-sends use the full body because committed-offset authority is
    // byte-range based while sink delegation is all-or-nothing; a suffix would
    // splice unrelated coordinates and risk a black hole.
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
        && relay_text.len() > crate::services::discord::DISCORD_MSG_LIMIT
}

/// #2840 (relay-stability P1): RAII guard for the cross-watcher emission slot
/// (`relay_coord.relay_slot`, an `Arc<AtomicU64>`: 0 = free, non-zero = a
/// watcher is mid-emission with that start offset). The slot is shared across
/// every watcher instance for a channel/session, so if the holding watcher
/// early-returns, hits a `?`, panics, or is task-aborted between CAS-acquire
/// and the manual `store(0)`, the slot stays non-zero forever and every
/// replacement watcher's relay is skipped — a permanent channel wedge until
/// process restart.
///
/// The guard releases the slot on Drop so ANY exit path frees it. The two
/// intended in-loop release points still call `release()` explicitly to
/// preserve their exact timing (site 1 releases *before* a 500ms backoff sleep,
/// so scope-end Drop alone would hold the slot across that sleep); the
/// idempotent `released` flag makes the trailing Drop a no-op after an explicit
/// release.
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
            // #2841 (codex review): reaching Drop without a prior explicit
            // release() means an abnormal exit (panic / `?` / task
            // cancellation) BEFORE the turn recorded its relayed offset /
            // advanced confirmed-end — so the delivery outcome of any in-flight
            // Discord send is UNKNOWN. Freeing the slot prevents a permanent
            // channel wedge, but a replacement watcher MAY then re-emit the same
            // range (a bounded duplicate window). This is strictly better than a
            // permanent wedge; the (channel, turn, byte-range) delivery lease
            // (P1) closes the window by recording delivery BEFORE the slot
            // frees. Surface it so the window is measurable until the lease lands.
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
