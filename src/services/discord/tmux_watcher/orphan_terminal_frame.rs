//! #5941: durable preservation for a terminal frame that ended with NO delivery
//! owner — the sink did not deliver it and the watcher's soft-terminal authority
//! was denied, so the body was dropped with only a WARN and a counter no alert
//! table read. Split out of `terminal_relay_plan.rs` for that module's size cap.

use super::*;

/// #5941 invariant I17 (`docs/relay-state-contract.md`): a terminal frame
/// carrying a body must end with a delivery owner or a durable record.
pub(super) const TERMINAL_FRAME_OWNER_OR_RECORD_INVARIANT: &str =
    "terminal_frame_has_a_delivery_owner_or_a_record";

/// The facts the #5175 denial seam already holds, named so the record decision is a pure
/// function of them. Two coordinate systems, kept apart deliberately:
/// `response_sent_offset`/`full_response_len` index the in-memory response String;
/// `data_start_offset`/`current_offset`/`terminal_event_consumed_offset` are JSONL offsets.
pub(super) struct OrphanTerminalFrameFacts<'a> {
    pub(super) denial: Option<SoftTerminalAuthorityDenial>,
    pub(super) watcher_direct_fallback_requested: bool,
    pub(super) watcher_direct_fallback_authorized: bool,
    pub(super) session_bound_relay_owns_terminal_delivery: bool,
    pub(super) direct_terminal_response_refused_duplicate: bool,
    /// The unsent tail, `full_response[response_sent_offset..]`.
    pub(super) current_response: &'a str,
    pub(super) response_sent_offset: usize,
    pub(super) full_response_len: usize,
    pub(super) data_start_offset: u64,
    pub(super) current_offset: u64,
    pub(super) terminal_event_consumed_offset: u64,
    /// The resend-dedup committed floor the sibling `SkipAlreadyCommitted` arm
    /// already consults over this exact range, so the record decision asks that
    /// authority too and not only the watcher's own refusal.
    pub(super) watcher_resend_committed: u64,
    pub(super) terminal_kind: Option<WatcherTerminalKind>,
    pub(super) session_bound_ack_outcome: SessionBoundRelayAckOutcome,
    pub(super) inflight_present: bool,
    pub(super) inflight_relay_owner: &'a str,
    pub(super) startup_snapshot_authority: bool,
    pub(super) tmux_session_name: &'a str,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) request_owner_user_id: Option<u64>,
}

impl OrphanTerminalFrameFacts<'_> {
    /// Did this frame end with no delivery owner AND a body worth preserving?
    ///
    /// `denial.is_some()` is implied today by the two fallback flags, kept
    /// because a hard-terminal veto can deny authority without them. Empty
    /// bodies are excluded: 18 of the 33 denials in the 2026-09-16 incident
    /// carried one. The last two conjuncts ask the SINK side, which the six
    /// above never do — a watcher that refused to send is no evidence the sink
    /// did not send. `RingUnknown` (`SentButUncommitted`: the POST landed, only
    /// its commit PROOF was lost) and `TimedOut` (the deadline elapsed with the
    /// POST still IN FLIGHT) are the ONLY arms that are not evidence of
    /// non-delivery; the other five stay recordable. The range conjunct is
    /// `> 0`, not the pre-r1 `> data_start_offset`, which excluded every turn
    /// served out of the #1216 leftover buffer, whose `data_start_offset` is the
    /// CARRIED buffer's turn start and can sit at or above this consumed range.
    pub(super) fn record_required(&self) -> bool {
        self.denial.is_some()
            && self.watcher_direct_fallback_requested
            && !self.watcher_direct_fallback_authorized
            && !self.session_bound_relay_owns_terminal_delivery
            && !self.direct_terminal_response_refused_duplicate
            && !self.current_response.is_empty()
            && self.terminal_event_consumed_offset > 0
            && !matches!(
                self.session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::RingUnknown | SessionBoundRelayAckOutcome::TimedOut
            )
            && !dr::range_already_committed(
                self.terminal_event_consumed_offset,
                self.watcher_resend_committed,
            )
    }

    /// The row itself, extracted so the payload mapping is pinned by running it
    /// rather than by grepping this file: `content` is the UNSENT tail (a
    /// `full_response` here re-publishes the prefix the user already read).
    pub(super) fn dead_letter_record(
        &self,
        channel_id: serenity::ChannelId,
        reason: String,
    ) -> crate::db::relay_dead_letter::RelayDeadLetterRecord {
        crate::db::relay_dead_letter::RelayDeadLetterRecord {
            kind: crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER.to_string(),
            channel_id: channel_id.to_string(),
            author_id: self.request_owner_user_id.map(|id| id.to_string()),
            message_id: self.placeholder_msg_id.map(|id| id.get().to_string()),
            content: self.current_response.to_string(),
            reason,
        }
    }

    /// One line an operator can read back into both coordinate systems: the response
    /// indices bounding `content`, and the JSONL range the frontier refused to advance
    /// past. `generation_mtime_ns` fences the row to the transcript generation it was cut
    /// from, so a later `/compact` cannot repoint the offsets at another file.
    pub(super) fn reason(
        &self,
        denial: SoftTerminalAuthorityDenial,
        provider: &ProviderKind,
        generation_mtime_ns: i64,
    ) -> String {
        format!(
            "{kind} denial={denial} terminal_kind={terminal_kind} \
             response_sent_offset={response_sent_offset} full_response_len={full_response_len} \
             jsonl_start={jsonl_start} jsonl_end={jsonl_end} current_offset={current_offset} \
             generation_mtime_ns={generation_mtime_ns} tmux_session={tmux_session} \
             provider={provider} inflight_relay_owner={inflight_relay_owner} \
             frame_ack_outcome={frame_ack_outcome:?}",
            kind = crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER,
            denial = denial.as_str(),
            terminal_kind = self
                .terminal_kind
                .map(WatcherTerminalKind::as_str)
                .unwrap_or("unknown"),
            response_sent_offset = self.response_sent_offset,
            full_response_len = self.full_response_len,
            jsonl_start = self.data_start_offset,
            jsonl_end = self.terminal_event_consumed_offset,
            current_offset = self.current_offset,
            generation_mtime_ns = generation_mtime_ns,
            tmux_session = self.tmux_session_name,
            provider = provider.as_str(),
            inflight_relay_owner = self.inflight_relay_owner,
            frame_ack_outcome = self.session_bound_ack_outcome,
        )
    }
}

/// #5175's WARN and per-conjunct counter (unchanged), followed by the #5941
/// durable record that turns a traceless loss into a recoverable row.
///
/// Returns whether invariant I17 is intact as of the SYNCHRONOUS decision; a pool-backed
/// write that fails later reports the same violation from its own detached task. Production
/// drops the value — the frame is already lost by then — and the regression test reads it,
/// because "there is no record" must not be readable as "there is no problem".
pub(super) fn observe_orphan_terminal_frame(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    facts: &OrphanTerminalFrameFacts<'_>,
) -> bool {
    let Some(denial) = facts.denial.filter(|_| {
        facts.watcher_direct_fallback_requested && !facts.watcher_direct_fallback_authorized
    }) else {
        return true;
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %facts.tmux_session_name,
        data_start_offset = facts.data_start_offset,
        current_offset = facts.current_offset,
        terminal_kind = facts.terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
        soft_terminal_denial = denial.as_str(),
        inflight_present = facts.inflight_present,
        inflight_relay_owner = facts.inflight_relay_owner,
        startup_snapshot_authority = facts.startup_snapshot_authority,
        full_response_len = facts.current_response.len(),
        session_bound_ack_outcome = ?facts.session_bound_ack_outcome,
        "  [{ts}] ⚠ #5175: terminal frame has NO delivery owner — sink did not deliver and the soft terminal is unauthorized; body dropped and the delivery frontier will not advance"
    );
    if !facts.record_required() {
        return true;
    }
    // Counted AFTER the admission: the alert table reads this counter at
    // threshold 1 and most denials lose nothing. The WARN stays unconditional.
    crate::services::observability::metrics::record_relay_terminal_authority_denied(
        channel_id.get(),
        provider.as_str(),
        denial.metric_name(),
    );
    let reason = facts.reason(
        denial,
        provider,
        dr::current_generation_mtime_ns(facts.tmux_session_name),
    );
    // Decided by the WRITE, not by the pool: an unconfigured sink reports
    // synchronously, a failed INSERT from the detached task.
    let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let observed = std::sync::Arc::clone(&held);
    let provider_name = provider.as_str().to_string();
    let session_key = facts.tmux_session_name.to_string();
    let violation_details = serde_json::json!({ "reason": reason.as_str() });
    crate::db::relay_dead_letter::record_detached_reporting(
        shared.pg_pool.as_ref(),
        facts.dead_letter_record(channel_id, reason),
        move |recorded| {
            if !crate::services::observability::record_invariant_check(
                recorded,
                crate::services::observability::InvariantViolation {
                    provider: Some(provider_name.as_str()),
                    channel_id: Some(channel_id.get()),
                    dispatch_id: None,
                    session_key: Some(session_key.as_str()),
                    turn_id: None,
                    invariant: TERMINAL_FRAME_OWNER_OR_RECORD_INVARIANT,
                    code_location: "src/services/discord/tmux_watcher/orphan_terminal_frame.rs:observe_orphan_terminal_frame",
                    message: "terminal frame body dropped with no delivery owner and no durable record",
                    details: violation_details,
                },
            ) {
                observed.store(false, std::sync::atomic::Ordering::Release);
            }
        },
    );
    held.load(std::sync::atomic::Ordering::Acquire)
}
