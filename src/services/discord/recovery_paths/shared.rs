//! Cross-path recovery helpers (issue #1074).
//!
//! This module collects helpers that the three recovery paths (restart /
//! runtime / manual rebind) all need. It intentionally starts very small —
//! the goal of issue #1074's first landing is to create the SSoT surface and
//! migration target, not to relocate every helper at once.
//!
//! Helpers that live here must be:
//!   - pure or nearly pure (no lifecycle state mutation),
//!   - used by at least two of the three paths, or
//!   - explicitly documented as the canonical owner.
//!
//! See `docs/recovery-paths.md` for the path contract.

/// #3293: outcome of relaying a recovered terminal text/notice to Discord.
///
/// Replaces the prior `bool` so the restart path can distinguish a Discord
/// "this destination is permanently gone" verdict (HTTP 404/403/410, the
/// `placeholder_sweeper::is_permanent_message_gone_status` allowlist) from a
/// transient failure (5xx / 429 / network, where retrying is correct).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoveryRelayOutcome {
    /// The assistant response actually reached Discord.
    Delivered,
    /// Discord said the channel/message can never come back (404/403/410).
    /// Retrying on every future boot would WARN-loop forever.
    PermanentFailure,
    /// Anything else: no HTTP status, 5xx, 429 rate-limit, transport error.
    /// Must stay retryable — never escalate these to a destructive verdict.
    TransientFailure,
}

impl RecoveryRelayOutcome {
    /// Adapter for the pre-#3293 `bool` call sites (dispatch-flow branches):
    /// `true` only when the response actually reached Discord.
    pub(in crate::services::discord) fn delivered(self) -> bool {
        matches!(self, RecoveryRelayOutcome::Delivered)
    }
}

/// Status-code half of the relay-error classification, split out (same
/// pattern as `placeholder_sweeper::is_permanent_message_gone_status`) so it
/// can be table-tested without constructing the `#[non_exhaustive]`
/// `serenity::http::ErrorResponse`.
pub(in crate::services::discord) fn classify_recovery_relay_status(
    status: Option<u16>,
) -> RecoveryRelayOutcome {
    match status {
        Some(code) if super::super::placeholder_sweeper::is_permanent_message_gone_status(code) => {
            RecoveryRelayOutcome::PermanentFailure
        }
        _ => RecoveryRelayOutcome::TransientFailure,
    }
}

/// Classify a boxed relay error (`formatting::replace_long_message_raw` /
/// `send_long_message_raw` return `Box<dyn Error>`): walk the source chain
/// for a `serenity::Error::Http` carrying a status code and feed it through
/// the conservative allowlist above. Anything unrecognized is transient.
pub(in crate::services::discord) fn classify_recovery_relay_error(
    error: &(dyn std::error::Error + 'static),
) -> RecoveryRelayOutcome {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = current {
        if let Some(poise::serenity_prelude::Error::Http(http_err)) =
            err.downcast_ref::<poise::serenity_prelude::Error>()
        {
            return classify_recovery_relay_status(
                http_err.status_code().map(|status| status.as_u16()),
            );
        }
        current = err.source();
    }
    RecoveryRelayOutcome::TransientFailure
}

/// #3297 finding 2: verdict of the post-failure channel-liveness probe.
///
/// The placeholder-anchor relay path (`replace_long_message_raw` →
/// `send_long_message_raw_with_rollback`) flattens its error chain into
/// `String`s, so the typed-chain walk above classifies a dead channel's
/// 404/403/410 as `TransientFailure` and the permanent verdict was
/// unreachable on the common (anchored) path. Instead of rebuilding the
/// formatting error chain, callers actively probe the channel with a direct
/// Discord HTTP `get_channel` AFTER a transient-looking relay failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ChannelProbeVerdict {
    /// The probe itself got an authoritative 404/403/410 for the CHANNEL —
    /// the destination is permanently gone.
    Gone,
    /// The channel exists, or the probe failed transiently (5xx / 429 /
    /// transport). Conservative: never escalates a relay failure.
    Inconclusive,
}

/// Status-code half of the channel probe, sharing the permanent allowlist
/// with the relay classification so the two can never drift.
pub(in crate::services::discord) fn classify_channel_probe_status(
    status: Option<u16>,
) -> ChannelProbeVerdict {
    match classify_recovery_relay_status(status) {
        RecoveryRelayOutcome::PermanentFailure => ChannelProbeVerdict::Gone,
        _ => ChannelProbeVerdict::Inconclusive,
    }
}

/// Second-opinion escalation for an already-classified relay failure. The
/// probe runs ONLY for a transient classification (a typed permanent verdict
/// needs no probe; `Delivered` never reaches here), and only an authoritative
/// [`ChannelProbeVerdict::Gone`] upgrades the outcome — probe failures keep
/// the conservative transient verdict. The probe is a closure so tests can
/// inject verdicts without a live Discord client (the #3297 finding-2 test
/// seam). Takes the pre-classified outcome (not the error) so callers'
/// futures stay `Send` — `&dyn Error` is not `Sync`.
pub(in crate::services::discord) async fn escalate_transient_relay_outcome_with_probe<F, Fut>(
    classified: RecoveryRelayOutcome,
    probe: F,
) -> RecoveryRelayOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ChannelProbeVerdict>,
{
    match classified {
        RecoveryRelayOutcome::TransientFailure => match probe().await {
            ChannelProbeVerdict::Gone => RecoveryRelayOutcome::PermanentFailure,
            ChannelProbeVerdict::Inconclusive => RecoveryRelayOutcome::TransientFailure,
        },
        verdict => verdict,
    }
}

/// #3293: what the restart path should do with the on-disk inflight row after
/// a terminal-relay attempt. Pure decision so the safety matrix is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RowDisposition {
    /// Relay delivered — run the branch's normal finish + clear epilogue.
    FinishAndClear,
    /// Discord permanently rejected the destination — force-clear now (with
    /// on-disk force-clear report + audit) regardless of the attempt counter.
    ClearPermanent,
    /// Transient failures exhausted the restart budget on a row whose tmux is
    /// already confirmed gone — force-clear (with on-disk force-clear report
    /// + audit).
    ClearBudgetExhausted,
    /// Preserve the row for the next boot and persist `attempts + 1`.
    PreserveAndCount,
}

/// Decision matrix for the post-relay row disposition.
///
/// `attempts` is the row's persisted `recovery_relay_attempts` BEFORE this
/// boot's failure is counted, so the budget trips when `attempts + 1 >=
/// budget`. `tmux_alive == true` (the ready-without-output branch) must NEVER
/// budget-clear: a live pane can still produce/own the answer (#1446 /
/// 2026-05-26 incident class) — only a permanent Discord verdict may clear it.
pub(in crate::services::discord) fn unrecoverable_relay_disposition(
    outcome: RecoveryRelayOutcome,
    attempts: u32,
    budget: u32,
    tmux_alive: bool,
) -> RowDisposition {
    match outcome {
        RecoveryRelayOutcome::Delivered => RowDisposition::FinishAndClear,
        RecoveryRelayOutcome::PermanentFailure => RowDisposition::ClearPermanent,
        RecoveryRelayOutcome::TransientFailure => {
            if !tmux_alive && attempts.saturating_add(1) >= budget {
                RowDisposition::ClearBudgetExhausted
            } else {
                RowDisposition::PreserveAndCount
            }
        }
    }
}

/// `termination_audit` reason code for a force-clear disposition; `None` for
/// the non-clearing dispositions. Extracted so the wire-visible codes are
/// pinned by tests (the audit insert itself is skipped when PG is absent).
pub(in crate::services::discord) fn disposition_reason_code(
    disposition: RowDisposition,
) -> Option<&'static str> {
    match disposition {
        RowDisposition::ClearPermanent => Some("recovery_permanent_relay_failure"),
        RowDisposition::ClearBudgetExhausted => Some("recovery_retry_budget_exhausted"),
        RowDisposition::FinishAndClear | RowDisposition::PreserveAndCount => None,
    }
}

/// #3918: may the committed-then-gone anchor-repost fallback send a NEW message
/// for this turn? The send-new path (`try_recover_anchor_repost`) is the #3607
/// data-loss backstop, but it is NOT a transaction with the on-disk row
/// retirement: Discord can accept the new message and the process can then crash
/// (or `clear_inflight_state` can silently fail) before the row is cleared,
/// which re-enters the committed branch on the next boot and would re-post the
/// SAME answer. This pure guard makes the repost fire AT MOST ONCE per logical
/// turn from two orthogonal durable inputs (both persisted ON the carrier row):
///   * `already_reposted` — the `anchor_reposted` marker, written right AFTER a
///     `Delivered` send and BEFORE the clear. `true` ⇒ refuse: this turn was
///     already reposted, so a persisted-row re-run (failed clear / crash after
///     the marker) must NOT post a duplicate. Bounds the realistic unbounded
///     loop (a silently failing clear) to a single post.
///   * `attempts >= budget` — the pre-send `anchor_repost_attempts` counter,
///     bumped BEFORE each send. Refuse: even in the narrow crash window BEFORE
///     the marker is recorded, the total number of send-new posts for a turn is
///     hard-bounded, so duplication can never be UNBOUNDED.
/// `budget` is `RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET`. `anchor_repost_attempts`
/// is deliberately DISTINCT from `recovery_relay_attempts` so this bound never
/// double-counts against the `PreserveAndCount` bump — a premature force-clear
/// there would re-introduce the #3607 data loss.
pub(in crate::services::discord) fn anchor_repost_send_new_permitted(
    already_reposted: bool,
    attempts: u32,
    budget: u32,
) -> bool {
    !already_reposted && attempts < budget
}

/// #3610 PR-2: gate for the recovery anchor-repost fallback (`AGENTDESK_RECOVERY_ANCHOR_REPOST`).
///
/// DEFAULT OFF — intentionally gated, with a tracked rollout plan (#3862 / #3918).
/// The #3607/#3610 committed-then-gone fallback is fully wired and storm-guarded
/// (G1–G5 + the transient send-new budget bound in `unrecoverable_relay_disposition`),
/// but a naked default-ON flip is BLOCKED on two send-new idempotency gaps that only
/// surface once this path is live (tracked in #3918):
///   1. a successful send-new is not durably idempotent — a crash (or a silently
///      failing `inflight::clear_inflight_state`, whose `bool` is ignored) after
///      Discord accepts the new message lets the next restart re-post the same
///      answer, and `Delivered` is not budget-counted, so the duplicate is unbounded;
///   2. a multi-chunk send-new partial failure leaves earlier chunks posted with no
///      rollback (`formatting::send_long_message_raw`, not the existing
///      `send_long_message_raw_with_rollback`), so the budget-bounded retry re-sends
///      the whole body and duplicates them.
/// Activation criteria + rollout live in #3918; until they are met this stays OFF.
/// The env var remains a staging opt-in ("1"/"true") for verification under those
/// guards. When OFF this is the outermost guard of
/// [`super::restart::try_recover_anchor_repost`], which short-circuits to `None`
/// before reading any record / probing / relaying, so the recovery loop is a
/// byte-for-byte no-op (the committed-branch call site is skipped entirely).
/// Telemetry is emitted ONLY when ENABLED, matching the A3 standby / recovery
/// controller cutovers — the default-OFF first evaluation must have NO observable
/// side effect.
pub(in crate::services::discord) fn recovery_anchor_repost_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let on = std::env::var("AGENTDESK_RECOVERY_ANCHOR_REPOST")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| v == "1" || v == "true");
        if on {
            tracing::info!("  ✓ recovery_anchor_repost: enabled");
        }
        on
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelProbeVerdict, RecoveryRelayOutcome, RowDisposition,
        anchor_repost_send_new_permitted, classify_channel_probe_status,
        classify_recovery_relay_error, classify_recovery_relay_status, disposition_reason_code,
        escalate_transient_relay_outcome_with_probe, unrecoverable_relay_disposition,
    };

    const BUDGET: u32 = crate::services::discord::inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET;

    #[test]
    fn classify_status_treats_message_gone_codes_as_permanent() {
        for code in [404, 403, 410] {
            assert_eq!(
                classify_recovery_relay_status(Some(code)),
                RecoveryRelayOutcome::PermanentFailure,
                "status {code} must be a permanent relay failure"
            );
        }
    }

    #[test]
    fn classify_status_keeps_everything_else_transient() {
        // 429 rate-limit, 5xx, odd client errors, and "no status at all"
        // (gateway not connected / transport error) must all stay retryable.
        for code in [400, 401, 408, 429, 500, 502, 503, 504] {
            assert_eq!(
                classify_recovery_relay_status(Some(code)),
                RecoveryRelayOutcome::TransientFailure,
                "status {code} must stay transient"
            );
        }
        assert_eq!(
            classify_recovery_relay_status(None),
            RecoveryRelayOutcome::TransientFailure
        );
    }

    /// #3297 finding-2 red-green: the placeholder-anchor relay path
    /// (`send_long_message_raw_with_rollback` & co.) flattens the serenity
    /// 404 into a `String` (`format!(...).into()`), so the typed-chain walk
    /// alone misclassifies a dead channel as transient (the RED half,
    /// asserted explicitly). The active channel probe restores the permanent
    /// verdict (GREEN) — with the probe injected through the test seam.
    #[tokio::test]
    async fn string_flattened_error_with_dead_channel_probe_is_permanent() {
        let flattened: Box<dyn std::error::Error + Send + Sync> =
            "failed to edit message for replace: 404 Not Found (Unknown Channel)"
                .to_string()
                .into();
        let classified = classify_recovery_relay_error(flattened.as_ref());
        assert_eq!(
            classified,
            RecoveryRelayOutcome::TransientFailure,
            "RED: the String-flattened chain hides the typed 404 from the chain walk"
        );
        assert_eq!(
            escalate_transient_relay_outcome_with_probe(classified, || async {
                ChannelProbeVerdict::Gone
            })
            .await,
            RecoveryRelayOutcome::PermanentFailure,
            "GREEN: a dead-channel probe must upgrade the flattened failure to permanent"
        );
    }

    /// Conservative direction: an inconclusive probe (alive channel, probe
    /// transport error, 5xx, 429) must keep the transient verdict.
    #[tokio::test]
    async fn inconclusive_probe_keeps_transient_verdict() {
        let flattened: Box<dyn std::error::Error + Send + Sync> =
            "edit failed: connection reset by peer".to_string().into();
        let classified = classify_recovery_relay_error(flattened.as_ref());
        assert_eq!(
            escalate_transient_relay_outcome_with_probe(classified, || async {
                ChannelProbeVerdict::Inconclusive
            })
            .await,
            RecoveryRelayOutcome::TransientFailure
        );
    }

    /// A pre-classified permanent verdict passes through without consulting
    /// the probe (the closure panics if invoked).
    #[tokio::test]
    async fn permanent_classification_skips_the_probe() {
        assert_eq!(
            escalate_transient_relay_outcome_with_probe(
                RecoveryRelayOutcome::PermanentFailure,
                || async { panic!("probe must not run for a typed permanent verdict") }
            )
            .await,
            RecoveryRelayOutcome::PermanentFailure
        );
    }

    #[test]
    fn probe_status_shares_the_permanent_allowlist() {
        for code in [404, 403, 410] {
            assert_eq!(
                classify_channel_probe_status(Some(code)),
                ChannelProbeVerdict::Gone,
                "channel-gone status {code} must be authoritative"
            );
        }
        for code in [200, 400, 401, 408, 429, 500, 502, 503, 504] {
            assert_eq!(
                classify_channel_probe_status(Some(code)),
                ChannelProbeVerdict::Inconclusive,
                "status {code} must stay inconclusive"
            );
        }
        assert_eq!(
            classify_channel_probe_status(None),
            ChannelProbeVerdict::Inconclusive
        );
    }

    #[test]
    fn delivered_adapter_matches_legacy_bool_contract() {
        assert!(RecoveryRelayOutcome::Delivered.delivered());
        assert!(!RecoveryRelayOutcome::PermanentFailure.delivered());
        assert!(!RecoveryRelayOutcome::TransientFailure.delivered());
    }

    #[test]
    fn delivered_outcome_always_finishes_and_clears() {
        for (attempts, tmux_alive) in [(0, false), (99, false), (0, true), (99, true)] {
            assert_eq!(
                unrecoverable_relay_disposition(
                    RecoveryRelayOutcome::Delivered,
                    attempts,
                    BUDGET,
                    tmux_alive
                ),
                RowDisposition::FinishAndClear
            );
        }
    }

    #[test]
    fn permanent_failure_clears_immediately_regardless_of_attempts() {
        for (attempts, tmux_alive) in [(0, false), (0, true), (99, false)] {
            assert_eq!(
                unrecoverable_relay_disposition(
                    RecoveryRelayOutcome::PermanentFailure,
                    attempts,
                    BUDGET,
                    tmux_alive
                ),
                RowDisposition::ClearPermanent
            );
        }
    }

    #[test]
    fn transient_failure_counts_until_budget_then_clears_when_tmux_gone() {
        for attempts in 0..(BUDGET - 1) {
            assert_eq!(
                unrecoverable_relay_disposition(
                    RecoveryRelayOutcome::TransientFailure,
                    attempts,
                    BUDGET,
                    false
                ),
                RowDisposition::PreserveAndCount,
                "attempt {attempts} must still preserve the row"
            );
        }
        assert_eq!(
            unrecoverable_relay_disposition(
                RecoveryRelayOutcome::TransientFailure,
                BUDGET - 1,
                BUDGET,
                false
            ),
            RowDisposition::ClearBudgetExhausted,
            "the budget'th failed restart must force-clear a tmux-gone row"
        );
    }

    #[test]
    fn pane_alive_row_is_never_budget_cleared() {
        // Adversarial scenario 1: repeated deploys during a Discord outage
        // with a live pane — even an absurd attempt count must preserve.
        for attempts in [0, BUDGET - 1, BUDGET, 99] {
            assert_eq!(
                unrecoverable_relay_disposition(
                    RecoveryRelayOutcome::TransientFailure,
                    attempts,
                    BUDGET,
                    true
                ),
                RowDisposition::PreserveAndCount,
                "pane-alive row must never be budget-cleared (attempts={attempts})"
            );
        }
    }

    /// #3610 PR-2 (codex r2 Issue-2, storm guard): the committed-branch
    /// anchor-repost dispose call passes `tmux_alive = false` ON PURPOSE
    /// (recovery_engine.rs anchor_repost branch), so a send-new that keeps
    /// failing transiently is BUDGET-BOUNDED rather than preserved forever.
    /// This pins the property the call site relies on: with `tmux_alive = false`,
    /// a TransientFailure ALWAYS terminates at the budget — even if the pane is
    /// (or would be) alive. Contrast `pane_alive_row_is_never_budget_cleared`,
    /// which proves the OPPOSITE for the normal-turn callers that pass the real
    /// `tmux_alive` (a live pane may still own a not-yet-committed answer). A
    /// committed row's answer is already on the wire, so pane liveness is
    /// irrelevant to the repost and the bound must hold.
    #[test]
    fn committed_repost_transient_is_budget_bounded_no_infinite_preserve() {
        // Below budget: preserve+count (bounded retry across restarts) ...
        for attempts in 0..(BUDGET - 1) {
            assert_eq!(
                unrecoverable_relay_disposition(
                    RecoveryRelayOutcome::TransientFailure,
                    attempts,
                    BUDGET,
                    // committed-repost ALWAYS passes false (pane liveness moot)
                    false,
                ),
                RowDisposition::PreserveAndCount,
                "attempt {attempts}: bounded retry must still preserve"
            );
        }
        // ... and at the budget the loop TERMINATES — it cannot preserve+retry
        // forever (the storm the codex review flagged). This is the key
        // anti-infinite-loop guarantee for an already-committed row.
        assert_eq!(
            unrecoverable_relay_disposition(
                RecoveryRelayOutcome::TransientFailure,
                BUDGET - 1,
                BUDGET,
                false,
            ),
            RowDisposition::ClearBudgetExhausted,
            "committed-repost transient MUST clear at the budget — no infinite preserve"
        );
        // Belt-and-suspenders: even an absurd attempt count past the budget
        // never reverts to preserve (the failure is permanently bounded).
        assert_eq!(
            unrecoverable_relay_disposition(
                RecoveryRelayOutcome::TransientFailure,
                BUDGET + 100,
                BUDGET,
                false,
            ),
            RowDisposition::ClearBudgetExhausted,
            "past-budget transient stays cleared (bound is monotone)"
        );
    }

    #[test]
    fn audit_reason_codes_are_pinned_for_clearing_dispositions() {
        assert_eq!(
            disposition_reason_code(RowDisposition::ClearPermanent),
            Some("recovery_permanent_relay_failure")
        );
        assert_eq!(
            disposition_reason_code(RowDisposition::ClearBudgetExhausted),
            Some("recovery_retry_budget_exhausted")
        );
        assert_eq!(
            disposition_reason_code(RowDisposition::FinishAndClear),
            None
        );
        assert_eq!(
            disposition_reason_code(RowDisposition::PreserveAndCount),
            None
        );
    }

    /// #3918: a fresh committed-then-gone row (never reposted, 0 attempts) is
    /// permitted to send-new exactly ONCE.
    #[test]
    fn fresh_row_is_permitted_to_repost_once() {
        assert!(
            anchor_repost_send_new_permitted(false, 0, BUDGET),
            "a never-reposted row under budget must be permitted to repost"
        );
    }

    /// #3918 idempotency: once the durable `anchor_reposted` marker is set, the
    /// send-new is REFUSED regardless of the attempt count — the repost fires at
    /// most once per turn even if a re-run still sees attempts under budget.
    #[test]
    fn marked_row_is_never_reposted_again() {
        for attempts in [0, 1, BUDGET - 1, BUDGET, BUDGET + 99] {
            assert!(
                !anchor_repost_send_new_permitted(true, attempts, BUDGET),
                "a row already marked reposted must never re-send (attempts={attempts})"
            );
        }
    }

    /// #3918 bound: even WITHOUT the marker (the narrow crash window before it is
    /// recorded), the pre-send attempt counter caps the repost at the budget so
    /// duplication can never be unbounded.
    #[test]
    fn pre_send_attempts_are_capped_at_budget() {
        for attempts in 0..BUDGET {
            assert!(
                anchor_repost_send_new_permitted(false, attempts, BUDGET),
                "attempt {attempts} is under budget and must be permitted"
            );
        }
        for attempts in [BUDGET, BUDGET + 1, BUDGET + 100] {
            assert!(
                !anchor_repost_send_new_permitted(false, attempts, BUDGET),
                "attempts at/over budget must be refused (attempts={attempts})"
            );
        }
    }
}
