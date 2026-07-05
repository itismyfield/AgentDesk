//! #3089 A0 — behavior-preserving disposition policy for the
//! `ReplaceLongMessageOutcome` returned by `replace_long_message_raw_with_outcome`.
//!
//! The terminal-delivery surfaces (tmux_watcher, session_relay_sink,
//! standby_relay) each `match` on that outcome and pick three load-bearing
//! decisions that the #3089 controller cutover must preserve exactly:
//!
//!   1. relay_ok          — is the response considered delivered/committed?
//!   2. retry_from_offset — must the watcher reset the offset and retry the
//!                          SAME range after a retry-safe transient failure?
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

pub(super) const WATCHER_FULL_SEND_REWIND_ATTEMPT_CAP: u8 = 3;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatcherSendFailureClass {
    Transient,
    Permanent,
    RollbackIncomplete,
}

impl WatcherSendFailureClass {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::RollbackIncomplete => "rollback_incomplete",
        }
    }
}

/// Pure status classifier for watcher terminal send failures. Retry is allowed
/// only for Discord rate limits, server errors, and request-timeout style
/// failures; ordinary 4xx responses are permanent for this turn.
pub(super) fn classify_watcher_send_failure_status(status: u16) -> WatcherSendFailureClass {
    if status == 429 || status == 408 || (500..=599).contains(&status) {
        WatcherSendFailureClass::Transient
    } else if (400..=499).contains(&status) {
        WatcherSendFailureClass::Permanent
    } else {
        WatcherSendFailureClass::Transient
    }
}

/// Pure string classifier for flattened send errors. Serenity often drops the
/// HTTP status from Display, so match the durable Discord error text first and
/// keep known network/timeout/server strings retryable.
pub(super) fn classify_watcher_send_failure_message(message: &str) -> WatcherSendFailureClass {
    let lower = message.to_ascii_lowercase();
    if lower.contains("cleanup incomplete")
        || lower.contains("cleanup in progress")
        || lower.contains("rollback state was not durable")
        || lower.contains("rollback state was not cleared")
    {
        return WatcherSendFailureClass::RollbackIncomplete;
    }
    if lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("ratelimit")
        || lower.contains("rate limited")
        || lower.contains("too many requests")
        || lower.contains("429")
    {
        return WatcherSendFailureClass::Transient;
    }
    const TRANSIENT_PATTERNS: &[&str] = &[
        "5xx",
        "500",
        "502",
        "503",
        "504",
        "bad gateway",
        "gateway timeout",
        "internal server error",
        "server error",
        "service unavailable",
        "timeout",
        "timed out",
        "temporary",
        "temporarily",
        "connection",
        "network",
        "transport",
        "error while sending http request",
    ];
    if TRANSIENT_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower.trim().is_empty()
    {
        return WatcherSendFailureClass::Transient;
    }
    const PERMANENT_PATTERNS: &[&str] = &[
        "400",
        "401",
        "403",
        "404",
        "405",
        "410",
        "422",
        "unknown channel",
        "unknown message",
        "missing access",
        "missing permissions",
        "invalid form body",
        "invalid webhook",
        "cannot send messages",
        "cannot edit a message authored by another user",
        "base_type_max_length",
        "2000 or fewer in length",
        "you are not allowed",
    ];
    if PERMANENT_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return WatcherSendFailureClass::Permanent;
    }
    WatcherSendFailureClass::Permanent
}

/// Classify a boxed send error: prefer typed serenity HTTP status when the
/// source chain still has it, then fall back to the flattened Display string.
pub(super) fn classify_watcher_send_failure(
    error: &(dyn std::error::Error + 'static),
) -> WatcherSendFailureClass {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = current {
        if let Some(poise::serenity_prelude::Error::Http(http_err)) =
            err.downcast_ref::<poise::serenity_prelude::Error>()
        {
            if let Some(status) = http_err.status_code() {
                return classify_watcher_send_failure_status(status.as_u16());
            }
        }
        current = err.source();
    }
    classify_watcher_send_failure_message(&error.to_string())
}

pub(super) fn watcher_send_failure_retry_plan(
    class: WatcherSendFailureClass,
) -> WatcherTerminalRelayPlan {
    match class {
        WatcherSendFailureClass::Transient => watcher_full_send_failure_retry_plan(),
        WatcherSendFailureClass::Permanent | WatcherSendFailureClass::RollbackIncomplete => {
            WatcherTerminalRelayPlan {
                relay_ok: false,
                retry_offset: false,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatcherRewindAttemptDisposition {
    Retry,
    GiveUp,
}

pub(super) fn watcher_rewind_attempt_disposition(attempts: u8) -> WatcherRewindAttemptDisposition {
    if attempts <= WATCHER_FULL_SEND_REWIND_ATTEMPT_CAP {
        WatcherRewindAttemptDisposition::Retry
    } else {
        WatcherRewindAttemptDisposition::GiveUp
    }
}

/// The watcher's base post-edit disposition for each outcome of
/// `replace_long_message_raw_with_outcome`. Committed edit/fallback outcomes do
/// not retry; uncommitted failures still need concrete error classification
/// before production code rewinds.
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
        // Partial continuation failures are retry-safe only after their sender
        // reports successful cleanup. TransportError alone is not proof that no
        // bytes landed; callers must classify the concrete error surface before
        // applying this retry plan.
        ReplaceOutcomeKind::PartialContinuationFailure | ReplaceOutcomeKind::TransportError => {
            WatcherTerminalRelayPlan {
                relay_ok: false,
                retry_offset: true,
            }
        }
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

/// The watcher's plan for failed full-send paths where no body is confirmed
/// delivered by the caller's rollback-safe send surface. TransportError by
/// itself is ambiguous; use [`watcher_send_failure_retry_plan`] after
/// classification for real send errors.
pub(super) fn watcher_full_send_failure_retry_plan() -> WatcherTerminalRelayPlan {
    watcher_terminal_relay_plan(ReplaceOutcomeKind::TransportError)
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
        EditFailFallbackDisposition, ReplaceOutcomeKind, WatcherRewindAttemptDisposition,
        WatcherSendFailureClass, classify_watcher_send_failure_message,
        classify_watcher_send_failure_status, edit_fail_fallback_disposition,
        relay_outcome_is_committed, watcher_rewind_attempt_disposition,
        watcher_send_failure_retry_plan, watcher_terminal_relay_plan,
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
                replacement_anchor: None,
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
    fn a0_uncommitted_failures_are_offset_retry_signals() {
        // Not committed, retry the SAME range from the reset offset. Deleting
        // `retry_... = true` from the production plan makes these assertions fail.
        let plan = watcher_terminal_relay_plan(ReplaceOutcomeKind::PartialContinuationFailure);
        assert!(!plan.relay_ok, "partial send is NOT a terminal commit");
        assert!(
            plan.retry_offset,
            "partial send MUST reset the offset and retry the same range"
        );
        let transport = watcher_terminal_relay_plan(ReplaceOutcomeKind::TransportError);
        assert!(!transport.relay_ok, "a transport error is not committed");
        assert!(
            transport.retry_offset,
            "a transport error MUST reset the offset and retry the same range"
        );
    }

    #[test]
    fn a0_committed_arms_never_request_offset_retry() {
        // If a mutation made a committed arm retry, or made it not-ok, these fail.
        let edited = watcher_terminal_relay_plan(ReplaceOutcomeKind::EditedOriginal);
        assert!(edited.relay_ok, "an edited original is committed");
        assert!(!edited.retry_offset);

        let fallback =
            watcher_terminal_relay_plan(ReplaceOutcomeKind::SentFallbackAfterEditFailure);
        assert!(fallback.relay_ok, "a preserved fallback is committed");
        assert!(!fallback.retry_offset);
    }

    #[test]
    fn watcher_send_failure_status_classifies_transient_vs_permanent_4154() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert_eq!(
                classify_watcher_send_failure_status(status),
                WatcherSendFailureClass::Transient,
                "{status} should rewind"
            );
        }
        for status in [400, 401, 403, 404, 410, 422] {
            assert_eq!(
                classify_watcher_send_failure_status(status),
                WatcherSendFailureClass::Permanent,
                "{status} should not rewind"
            );
        }
    }

    #[test]
    fn watcher_send_failure_message_classifies_flattened_discord_errors_4154() {
        for message in [
            "Missing Access",
            "Unknown Channel",
            "Invalid Form Body",
            "403 Forbidden",
            "404 Not Found",
        ] {
            assert_eq!(
                classify_watcher_send_failure_message(message),
                WatcherSendFailureClass::Permanent,
                "{message:?} should not rewind"
            );
        }
        for message in [
            "Error while sending HTTP request.",
            "operation timed out",
            "discord 5xx",
            "Internal Server Error",
            "503 Service Unavailable",
            "You are being rate limited.",
            "",
        ] {
            assert_eq!(
                classify_watcher_send_failure_message(message),
                WatcherSendFailureClass::Transient,
                "{message:?} should rewind"
            );
        }
        assert_eq!(
            classify_watcher_send_failure_message("previous chunk cleanup incomplete"),
            WatcherSendFailureClass::RollbackIncomplete
        );
    }

    #[test]
    fn watcher_send_failure_plan_rewinds_only_transient_4154() {
        let transient = watcher_send_failure_retry_plan(WatcherSendFailureClass::Transient);
        assert!(!transient.relay_ok);
        assert!(transient.retry_offset);

        for class in [
            WatcherSendFailureClass::Permanent,
            WatcherSendFailureClass::RollbackIncomplete,
        ] {
            let plan = watcher_send_failure_retry_plan(class);
            assert!(!plan.relay_ok);
            assert!(
                !plan.retry_offset,
                "{class:?} must fall through without rewind"
            );
        }
    }

    #[test]
    fn watcher_rewind_attempt_cap_gives_up_after_three_4154() {
        for attempts in [1, 2, 3] {
            assert_eq!(
                watcher_rewind_attempt_disposition(attempts),
                WatcherRewindAttemptDisposition::Retry
            );
        }
        assert_eq!(
            watcher_rewind_attempt_disposition(4),
            WatcherRewindAttemptDisposition::GiveUp
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
