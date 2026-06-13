//! #3089 A0 — behavior-preserving disposition policy for the
//! `ReplaceLongMessageOutcome` returned by `replace_long_message_raw_with_outcome`.
//!
//! The terminal-delivery surfaces (tmux_watcher, session_relay_sink,
//! standby_relay) each `match` on that outcome and pick three load-bearing
//! decisions that the #3089 controller cutover must preserve exactly:
//!
//!   1. relay_ok          — is the response considered delivered/committed?
//!   2. retry_from_offset — must the watcher reset the offset and retry the
//!                          SAME range (the 5th of 5 failure representations)?
//!   3. preserve_original — after `SentFallbackAfterEditFailure` the original
//!                          msg_id is NEVER deleted (#2757): a transient edit
//!                          failure must not vacuum a message that may already
//!                          hold streamed assistant content.
//!
//! Before #3089 these decisions were inline literals at each call site, so a
//! cutover could silently drop `retry_from_offset = true` or re-introduce the
//! #2757 deletion with every test staying green. Folding them into these pure,
//! production-CALLED functions makes the A0 characterization tests pin the real
//! behavior. The extraction is strictly behavior-preserving — every returned
//! value equals the literal the call site previously assigned.

use super::formatting::ReplaceLongMessageOutcome;

/// Variant discriminant of the `Result<ReplaceLongMessageOutcome, _>` the
/// surfaces match on. Lets the pure policy decide disposition from the variant
/// alone, while the call sites keep destructuring the payload fields they log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplaceOutcomeKind {
    EditedOriginal,
    SentFallbackAfterEditFailure,
    PartialContinuationFailure,
    TransportError,
}

impl ReplaceOutcomeKind {
    /// Classify the `Result<ReplaceLongMessageOutcome, _>` exactly as the
    /// surfaces' `match` arms do. Production calls this so the watcher's
    /// relay-plan decision is derived from the real outcome, not a hardcoded
    /// variant.
    pub(super) fn of<E>(result: &Result<ReplaceLongMessageOutcome, E>) -> Self {
        match result {
            Ok(ReplaceLongMessageOutcome::EditedOriginal) => Self::EditedOriginal,
            Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. }) => {
                Self::SentFallbackAfterEditFailure
            }
            Ok(ReplaceLongMessageOutcome::PartialContinuationFailure { .. }) => {
                Self::PartialContinuationFailure
            }
            Err(_) => Self::TransportError,
        }
    }
}

/// Watcher relay disposition: the two flags the tmux_watcher relay loop sets
/// before its post-relay `if relay_ok` / `if retry_terminal_delivery_from_offset`
/// branches. `retry_offset` maps to the loop's
/// `retry_terminal_delivery_from_offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WatcherTerminalRelayPlan {
    pub(super) relay_ok: bool,
    pub(super) retry_offset: bool,
}

/// The watcher's post-edit disposition for each outcome of
/// `replace_long_message_raw_with_outcome`. Only `PartialContinuationFailure`
/// is the offset-retry signal; everything else either committed already or is a
/// plain failure handled by the `relay_ok = false` paths.
pub(super) fn watcher_terminal_relay_plan(kind: ReplaceOutcomeKind) -> WatcherTerminalRelayPlan {
    match kind {
        // Committed in their own arms (edit consumed / fallback preserved the
        // original): the watcher does not touch the two flags there, so the plan
        // is the loop's initial state.
        ReplaceOutcomeKind::EditedOriginal | ReplaceOutcomeKind::SentFallbackAfterEditFailure => {
            WatcherTerminalRelayPlan {
                relay_ok: true,
                retry_offset: false,
            }
        }
        // The 5th failure representation: partial send => reset offset and retry
        // the SAME range next loop; no terminal commit.
        ReplaceOutcomeKind::PartialContinuationFailure => WatcherTerminalRelayPlan {
            relay_ok: false,
            retry_offset: true,
        },
        // Transport failure: not committed, but no offset retry either.
        ReplaceOutcomeKind::TransportError => WatcherTerminalRelayPlan {
            relay_ok: false,
            retry_offset: false,
        },
    }
}

/// Did the relay COMMIT the response for this outcome? `EditedOriginal` and the
/// preserved `SentFallbackAfterEditFailure` are committed; the partial-send and
/// transport failures are not. Shared by the surfaces' "delivered?" decisions
/// (standby's bool return, the watcher's `relay_ok`).
pub(super) fn relay_outcome_is_committed(kind: ReplaceOutcomeKind) -> bool {
    watcher_terminal_relay_plan(kind).relay_ok
}

/// The watcher's plan for the `PartialContinuationFailure` arm specifically —
/// the 5th of 5 failure representations (relay_ok=false, retry offset). A thin
/// alias so the deeply-nested watcher arm can read the plan in one short line.
pub(super) fn watcher_partial_continuation_retry_plan() -> WatcherTerminalRelayPlan {
    watcher_terminal_relay_plan(ReplaceOutcomeKind::PartialContinuationFailure)
}

/// #2757 disposition for the `SentFallbackAfterEditFailure` arm shared by
/// session_relay_sink and standby_relay: after a fallback send the original
/// msg_id is preserved (never deleted) and the turn is still treated as
/// delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditFailFallbackDisposition {
    /// Fallback send succeeded — treat as delivered AND keep the original
    /// message (the #2757 preserve arm).
    DeliveredPreserveOriginal,
}

/// The sink/standby disposition for `SentFallbackAfterEditFailure`. Centralizes
/// the #2757 rule so a cutover that re-introduces a delete is caught: this must
/// stay `DeliveredPreserveOriginal` (delivered = true, delete_original = false).
pub(super) fn edit_fail_fallback_disposition() -> EditFailFallbackDisposition {
    EditFailFallbackDisposition::DeliveredPreserveOriginal
}

impl EditFailFallbackDisposition {
    /// Must the original placeholder be deleted? (#2757: NEVER for the fallback
    /// arm.) The committed/delivered status of the fallback is decided by
    /// [`relay_outcome_is_committed`]; this disposition's sole job is the
    /// preserve-vs-delete datum.
    pub(super) fn deletes_original(self) -> bool {
        match self {
            Self::DeliveredPreserveOriginal => false,
        }
    }
}

#[cfg(test)]
mod a0_replace_outcome_policy_tests {
    //! #3089 A0 — characterization of the production disposition policy that
    //! tmux_watcher, session_relay_sink and standby_relay all now CALL. These
    //! assertions exercise the real functions (not a re-stated branch), so the
    //! codex-listed mutations break them:
    //!   * deleting `retry_offset = true` (now sourced
    //!     from `watcher_terminal_relay_plan`) flips the High-1 assertion;
    //!   * re-introducing the #2757 delete (`deletes_original` -> true) or
    //!     dropping the delivered status flips the High-2 assertions.
    use super::super::formatting::ReplaceLongMessageOutcome;
    use super::{
        EditFailFallbackDisposition, ReplaceOutcomeKind, edit_fail_fallback_disposition,
        relay_outcome_is_committed, watcher_terminal_relay_plan,
    };

    fn partial_continuation() -> ReplaceLongMessageOutcome {
        ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks: 1,
            total_chunks: 3,
            failed_chunk_index: 1,
            sent_continuation_message_ids: vec![10],
            cleanup_errors: vec![],
            error: "boom".to_string(),
        }
    }

    // -- High 1: tmux_watcher relay disposition -------------------------------

    #[test]
    fn a0_of_classifies_each_result_arm_like_the_watcher_match() {
        let edited: Result<ReplaceLongMessageOutcome, String> =
            Ok(ReplaceLongMessageOutcome::EditedOriginal);
        let fallback: Result<ReplaceLongMessageOutcome, String> =
            Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "e".to_string(),
            });
        let partial: Result<ReplaceLongMessageOutcome, String> = Ok(partial_continuation());
        let err: Result<ReplaceLongMessageOutcome, String> = Err("transport".to_string());

        assert_eq!(
            ReplaceOutcomeKind::of(&edited),
            ReplaceOutcomeKind::EditedOriginal
        );
        assert_eq!(
            ReplaceOutcomeKind::of(&fallback),
            ReplaceOutcomeKind::SentFallbackAfterEditFailure
        );
        assert_eq!(
            ReplaceOutcomeKind::of(&partial),
            ReplaceOutcomeKind::PartialContinuationFailure
        );
        assert_eq!(
            ReplaceOutcomeKind::of(&err),
            ReplaceOutcomeKind::TransportError
        );
    }

    #[test]
    fn a0_partial_continuation_is_the_offset_retry_failure_signal() {
        // The 5th of 5 failure representations: not committed, retry the SAME
        // range from the reset offset. Deleting `retry_... = true` from the
        // production plan (the codex mutation) makes this assertion fail.
        let plan = watcher_terminal_relay_plan(ReplaceOutcomeKind::PartialContinuationFailure);
        assert!(!plan.relay_ok, "partial send is NOT a terminal commit");
        assert!(
            plan.retry_offset,
            "partial send MUST reset the offset and retry the same range"
        );
    }

    #[test]
    fn a0_committed_and_transport_arms_never_request_offset_retry() {
        // Only PartialContinuationFailure retries. If a mutation made e.g.
        // EditedOriginal retry, or made a committed arm not-ok, these fail.
        let edited = watcher_terminal_relay_plan(ReplaceOutcomeKind::EditedOriginal);
        assert!(edited.relay_ok, "an edited original is committed");
        assert!(!edited.retry_offset);

        let fallback =
            watcher_terminal_relay_plan(ReplaceOutcomeKind::SentFallbackAfterEditFailure);
        assert!(fallback.relay_ok, "a preserved fallback is committed");
        assert!(!fallback.retry_offset);

        let transport = watcher_terminal_relay_plan(ReplaceOutcomeKind::TransportError);
        assert!(!transport.relay_ok, "a transport error is not committed");
        assert!(
            !transport.retry_offset,
            "a transport error is a plain failure, NOT the offset-retry signal"
        );
    }

    // -- High 2: sink/standby #2757 preserve arm ------------------------------

    #[test]
    fn a0_edit_fail_fallback_preserves_original_and_reports_delivered() {
        let disposition = edit_fail_fallback_disposition();
        assert_eq!(
            disposition,
            EditFailFallbackDisposition::DeliveredPreserveOriginal
        );
        assert!(
            !disposition.deletes_original(),
            "#2757: a cutover MUST NOT re-introduce the original-msg delete"
        );
        // The fallback IS a committed delivery (sink returns Delivered, standby
        // returns true). Both surfaces read this via `relay_outcome_is_committed`.
        assert!(
            relay_outcome_is_committed(ReplaceOutcomeKind::SentFallbackAfterEditFailure),
            "#2757: the preserved fallback copy is the successful delivery"
        );
        assert!(
            relay_outcome_is_committed(ReplaceOutcomeKind::EditedOriginal),
            "an edited original is committed"
        );
        assert!(
            !relay_outcome_is_committed(ReplaceOutcomeKind::PartialContinuationFailure),
            "a partial send is not committed"
        );
        assert!(
            !relay_outcome_is_committed(ReplaceOutcomeKind::TransportError),
            "a transport error is not committed"
        );
    }
}
