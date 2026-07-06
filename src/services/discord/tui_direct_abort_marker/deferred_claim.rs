//! #3303 — deferred-claim (`origin == DeferredClaim`) marker logic.
//!
//! ## Why this exists
//! When a deferred synthetic turn-start CLAIM succeeds
//! (`tui_direct_pending_start::run_worker`, `claimed == true`), the normal
//! `⏳ → ✅` flip is owned by the watcher relay's terminal commit path for the
//! synthetic turn. The observed #3303 failure modes break that ownership:
//! the commit pass never runs for the synthetic turn (the claim seeded the
//! relay cursor at EOF after a prior drain already consumed the response
//! bytes), or the relay fails and a watchdog clears the inflight row without
//! any terminal commit. Either way the anchor's `⏳` was ETERNAL — no
//! reconciler owned it.
//!
//! ## Mechanism
//! The worker records a [`MarkerOrigin::DeferredClaim`] marker (BEFORE it
//! deletes the durable pending-start record, so a crash in between re-records
//! idempotently on the restart re-claim) pinning its OWN synthetic turn
//! identity: `user_msg_id == anchor_message_id` plus the freshly-claimed
//! row's `started_at`. The existing reconcilers then converge it:
//!
//! * the watcher chokepoint's terminal-commit drain covers it when the OWN
//!   turn commits (`⏳ → ✅` — the happy path adds one idempotent, flock-
//!   serialized reaction attempt next to the normal G3 completion);
//! * the sweep's [`decide_deferred_claim_marker_disposition`] bounds the
//!   never-committed case with the TTL `⚠` (the observed bug modes converge
//!   to a bounded `⚠` instead of an eternal `⏳`).
//!
//! ## What it deliberately does NOT do (#3303 adversarial self-checks)
//! * **SC1 — never pin/cover the FOREIGN prior turn**: at the claim instant
//!   the prior turn's commit tombstone is definitionally already durable
//!   (the claim ran right after the prior finalize), so a foreign pin would
//!   stamp `covered` immediately and false-`✅` a still-streaming unanswered
//!   turn. Mode-1 samples (the response already relayed under the PRIOR
//!   turn's relay) therefore converge to the bounded `⚠`, not `✅` —
//!   safety > accuracy, by design.
//! * **No hard cap while the pinned OWN row is live**: unlike the Abort kind,
//!   the pinned identity here cannot churn (one row per `(provider,
//!   channel)`, and the identity IS this row) — the hold ends naturally with
//!   the row's commit/clear/watchdog, so a 1h+ streaming own turn is never
//!   false-`⚠`'d (the r1-finding-2 eternal-hold risk does not apply).
//! * **No hold for name-only successor rows**: once the own row is gone no
//!   later commit can ever cover this marker, so holding for a same-session
//!   successor would only delay the bounded `⚠`.

use super::{AbortedAnchorMarker, MarkerDisposition, MarkerOrigin};

/// One sweep pass's live-inflight observation for a marker, computed from a
/// SINGLE `load_inflight_state` read (#3303). `defers` is the Abort-kind hold
/// predicate ([`super::inflight_defers_sweep`]); `is_pinned_turn` is the
/// DeferredClaim-kind hold predicate: the live row IS the marker's pinned own
/// turn (`user_msg_id` / `started_at` / tmux session name all match).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct LiveInflightProbe {
    pub defers: bool,
    pub is_pinned_turn: bool,
}

/// Abort-kind shorthand (also the existing tests' closure form): a bare bool
/// is "a deferring inflight exists", which can never be the pinned own turn.
impl From<bool> for LiveInflightProbe {
    fn from(defers: bool) -> Self {
        Self {
            defers,
            is_pinned_turn: false,
        }
    }
}

impl AbortedAnchorMarker {
    /// Build the marker a SUCCESSFUL deferred claim records (#3303).
    /// `own_identity` is the synthetic turn's `(user_msg_id, started_at)` —
    /// `user_msg_id == anchor_message_id` by the synthetic-turn convention,
    /// and `started_at` is re-read from the freshly-claimed inflight row at
    /// the record instant. `own_turn_start_offset` is the durable id-0
    /// discriminator from the same row. Always uncovered at build time;
    /// coverage needs the drain or a commit-tombstone 대조, exactly like the
    /// Abort kind.
    pub(super) fn for_deferred_claim(
        provider: String,
        channel_id: u64,
        anchor_message_id: u64,
        tmux_session_name: String,
        claimed_at_ms: u64,
        own_identity: (u64, String),
        own_turn_start_offset: Option<u64>,
    ) -> Self {
        Self {
            provider,
            channel_id,
            anchor_message_id,
            tmux_session_name,
            aborted_at_ms: claimed_at_ms,
            covered_at_ms: None,
            foreign_user_msg_id: Some(own_identity.0),
            foreign_started_at: Some(own_identity.1),
            foreign_turn_start_offset: own_turn_start_offset,
            origin: MarkerOrigin::DeferredClaim,
        }
    }
}

/// The deferred-claim path's one-call record (mirrors
/// [`super::record_for_abort`]): build the marker pinned to the OWN synthetic
/// turn identity, run the record-instant tombstone 대조, persist. The 대조
/// reuses [`super::cover_from_commit_tombstone`] unchanged: it can only match
/// the OWN identity, which is possible solely on a restart re-claim that
/// re-records after the own turn already terminal-committed — a match IS
/// genuine own-commit evidence, never the SC1 foreign-tombstone false-`✅`.
/// `record()`'s stem overwrite (one file per `(provider, channel, anchor)`)
/// also REPLACES a stale Abort marker left by an abnormal restart: the turn
/// is now claimed and live, so the refreshed own-identity pin is the truth.
pub(in crate::services::discord) fn record_for_deferred_claim(
    provider: String,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: String,
    own_identity: (u64, String),
    own_turn_start_offset: Option<u64>,
) -> Result<AbortedAnchorMarker, String> {
    let mut marker = AbortedAnchorMarker::for_deferred_claim(
        provider,
        channel_id,
        anchor_message_id,
        tmux_session_name,
        super::now_ms(),
        own_identity,
        own_turn_start_offset,
    );
    super::cover_from_commit_tombstone(&mut marker);
    super::record(&marker)?;
    Ok(marker)
}

/// The sweep's per-marker verdict for `DeferredClaim` markers (#3303). Differs
/// from the Abort kind ([`super::decide_marker_disposition`], deliberately
/// UNTOUCHED including its hard cap) in exactly two ways:
///
/// * while the live row IS the pinned own turn the hold is UNCAPPED — the
///   pinned identity cannot churn (it is this very row), so the hold ends
///   naturally with the row's commit/clear/watchdog and a 1h+ streaming own
///   turn is never false-`⚠`'d;
/// * any OTHER live row (name-only successor) holds NOTHING — once the own
///   row is gone no commit can ever cover this marker, so deferring would
///   only delay the bounded `⚠`.
pub(super) fn decide_deferred_claim_marker_disposition(
    now_ms: u64,
    marker: &AbortedAnchorMarker,
    probe: LiveInflightProbe,
    ttl: std::time::Duration,
    http_available: bool,
) -> MarkerDisposition {
    if !http_available {
        return MarkerDisposition::LeftIntactHttpUnavailable;
    }
    if marker.covered_at_ms.is_some() {
        return MarkerDisposition::DeliverCompletion;
    }
    if probe.is_pinned_turn {
        return MarkerDisposition::KeepWaiting;
    }
    let elapsed_ms = now_ms.saturating_sub(marker.aborted_at_ms);
    if elapsed_ms < ttl.as_millis() as u64 {
        return MarkerDisposition::KeepWaiting;
    }
    MarkerDisposition::DeliverFailureWarn
}

/// Outcome of [`ensure_marker_for_own_synthetic_turn`] (#3350 ②). Purely
/// informational for the caller — every variant is fail-open (the finalize
/// proceeds identically).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum EnsureClaimMarkerOutcome {
    /// An own-pin `DeferredClaim` marker — or ANY already-covered (tombstoned)
    /// marker — exists for this anchor. NEVER overwritten: a stamped
    /// `covered_at_ms` or the original TTL clock must survive (I6 — an
    /// overwrite would demote a covered anchor to the TTL `⚠`). An UNCOVERED
    /// stale Abort marker does NOT yield this variant (#3350 codex r1-3): the
    /// #3303 contract replaces it with the own pin, see the ensure docs.
    AlreadyPresent,
    /// No marker existed; one was recorded. `covered: true` means the
    /// record-instant tombstone 대조 found the own turn already committed, so
    /// the sweep delivers an idempotent `✅`.
    Recorded { covered: bool },
    /// I5: a zero anchor id could never be reconciled.
    SkippedZeroAnchor,
    /// The durable write failed — warn only (I6 fail-open).
    Failed,
}

/// #3350 ②: idempotent marker guarantee at TERMINAL FINALIZE time. Whatever
/// submitter (watcher / bridge / monitor / backstop) finalizes a watcher-owned
/// TUI-direct synthetic turn, a durable `DeferredClaim` marker must exist for
/// its anchor — this is the forward-fix for turns claimed before the #3350
/// inline-claim record existed, and the safety net when that record failed.
///
/// * An existing own-pin `DeferredClaim` marker — or ANY already-covered
///   (tombstoned) marker — is NEVER overwritten
///   ([`EnsureClaimMarkerOutcome::AlreadyPresent`]): the `covered_at_ms`
///   stamp / TTL clock are preserved.
/// * An existing UNCOVERED stale Abort marker is REPLACED (#3350 codex r1-3),
///   exactly like the claim-time record (#3303 contract: `record()`'s stem
///   overwrite — see the module docs at [`record_for_deferred_claim`] and the
///   `tui_direct_pending_start` R8 reclaim test). Left in place it would block
///   the own pin forever: no commit can cover the foreign pin, so the TTL
///   would false-`⚠` the successfully claimed anchor.
/// * Otherwise [`record_for_deferred_claim`] is reused unchanged, including
///   the record-instant tombstone 대조: a turn that already terminal-committed
///   yields a `covered` marker the sweep resolves with an idempotent `✅`.
/// * NO reaction is performed here — delivery belongs exclusively to the
///   #3303 reconcilers (drain `✅` / sweep TTL `⚠`), so there is no false-`⚠`
///   race against output that commits late after a Stopped event.
pub(in crate::services::discord) fn ensure_marker_for_own_synthetic_turn(
    provider: &str,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: &str,
    own_started_at: &str,
    own_turn_start_offset: Option<u64>,
) -> EnsureClaimMarkerOutcome {
    if anchor_message_id == 0 {
        return EnsureClaimMarkerOutcome::SkippedZeroAnchor; // I5
    }
    let replaces_stale_abort = match super::load_for_channel(provider, channel_id)
        .into_iter()
        .find(|m| m.anchor_message_id == anchor_message_id)
    {
        Some(existing)
            if existing.covered_at_ms.is_some()
                || (existing.origin == MarkerOrigin::DeferredClaim
                    && existing.foreign_user_msg_id == Some(anchor_message_id)) =>
        {
            return EnsureClaimMarkerOutcome::AlreadyPresent;
        }
        // Uncovered non-own-pin marker (stale Abort): fall through and let
        // `record()`'s stem overwrite replace it with the own pin (r1-3).
        Some(_) => true,
        None => false,
    };
    match record_for_deferred_claim(
        provider.to_string(),
        channel_id,
        anchor_message_id,
        tmux_session_name.to_string(),
        (anchor_message_id, own_started_at.to_string()),
        own_turn_start_offset,
    ) {
        Ok(marker) => {
            tracing::info!(
                provider = %provider,
                channel_id,
                tmux_session_name = %tmux_session_name,
                anchor_message_id,
                tombstone_covered = marker.covered_at_ms.is_some(),
                replaces_stale_abort,
                "tui_direct_abort_marker: finalize-time ensure recorded the missing DeferredClaim marker — the anchor ⏳ now converges via drain ✅ / sweep TTL ⚠ (#3350)"
            );
            EnsureClaimMarkerOutcome::Recorded {
                covered: marker.covered_at_ms.is_some(),
            }
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider,
                channel_id,
                anchor_message_id,
                error = %error,
                "tui_direct_abort_marker: finalize-time ensure failed to persist the DeferredClaim marker; finalize proceeds without it (I6 fail-open — the anchor ⏳ may linger) (#3350)"
            );
            EnsureClaimMarkerOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER, ABORT_MARKER_TTL};
    use super::*;

    const TTL_MS: u64 = ABORT_MARKER_TTL.as_millis() as u64;
    const OWN_STARTED_AT: &str = "2026-06-10 13:00:00";

    fn deferred_marker(claimed_at_ms: u64) -> AbortedAnchorMarker {
        AbortedAnchorMarker::for_deferred_claim(
            "claude".into(),
            1,
            10,
            "tmux-1".into(),
            claimed_at_ms,
            (10, OWN_STARTED_AT.into()),
            None,
        )
    }

    /// #3303: the constructor pins the OWN synthetic identity, kind
    /// `DeferredClaim`, and never pre-covers.
    #[test]
    fn for_deferred_claim_pins_own_identity_uncovered() {
        let m = deferred_marker(1_000);
        assert_eq!(m.origin, MarkerOrigin::DeferredClaim);
        assert_eq!(m.foreign_user_msg_id, Some(10));
        assert_eq!(m.foreign_started_at.as_deref(), Some(OWN_STARTED_AT));
        assert_eq!(m.covered_at_ms, None);
        assert_eq!(m.aborted_at_ms, 1_000);
        assert!(
            m.matches_foreign_identity(10, OWN_STARTED_AT),
            "the drain's identity cover test must accept the own turn's commit"
        );
    }

    /// R5 (#3303 kind branch): while the live row IS the pinned own turn the
    /// hold is UNCAPPED — even past the Abort kind's 6×TTL hard cap. RED if
    /// the DeferredClaim disposition reuses the Abort hard cap (a 1h+
    /// streaming own turn would be false-`⚠`'d): the contrast assertion pins
    /// that the Abort disposition WOULD warn at the same instant.
    #[test]
    fn pinned_own_row_holds_uncapped_past_abort_hard_cap() {
        let m = deferred_marker(1_000);
        let past_cap = 1_000 + TTL_MS * ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER + 1;
        let pinned = LiveInflightProbe {
            defers: true,
            is_pinned_turn: true,
        };
        assert_eq!(
            decide_deferred_claim_marker_disposition(past_cap, &m, pinned, ABORT_MARKER_TTL, true),
            MarkerDisposition::KeepWaiting,
            "the pinned own row holds without a cap (identity cannot churn)"
        );
        assert_eq!(
            super::super::decide_marker_disposition(past_cap, &m, true, ABORT_MARKER_TTL, true),
            MarkerDisposition::DeliverFailureWarn,
            "contrast: the Abort disposition hard-caps here — proves the kind branch matters"
        );
    }

    /// R6 (#3303): a name-only successor row (defers for the Abort kind) holds
    /// NOTHING for a DeferredClaim marker — past the TTL the bounded `⚠`
    /// fires. RED if the DeferredClaim disposition reuses the Abort kind's
    /// live-inflight hold (the successor would delay the `⚠` indefinitely
    /// while no commit could ever cover the dead own turn).
    #[test]
    fn name_only_successor_does_not_hold_deferred_claim_marker() {
        let m = deferred_marker(1_000);
        let successor = LiveInflightProbe {
            defers: true,
            is_pinned_turn: false,
        };
        assert_eq!(
            decide_deferred_claim_marker_disposition(
                1_000 + TTL_MS,
                &m,
                successor,
                ABORT_MARKER_TTL,
                true,
            ),
            MarkerDisposition::DeliverFailureWarn,
        );
        assert_eq!(
            super::super::decide_marker_disposition(
                1_000 + TTL_MS,
                &m,
                true,
                ABORT_MARKER_TTL,
                true
            ),
            MarkerDisposition::KeepWaiting,
            "contrast: the Abort disposition holds for any deferring inflight"
        );
    }

    /// R9 (#3303 invariant: no `✅`/`⚠` without evidence or TTL): an uncovered
    /// marker with the own row already gone KEEPS WAITING before the TTL —
    /// row-absence is never commit evidence, and the `⚠` must wait for the
    /// bound.
    #[test]
    fn row_absence_before_ttl_keeps_waiting() {
        let m = deferred_marker(1_000);
        let absent = LiveInflightProbe {
            defers: false,
            is_pinned_turn: false,
        };
        assert_eq!(
            decide_deferred_claim_marker_disposition(
                1_000 + TTL_MS - 1,
                &m,
                absent,
                ABORT_MARKER_TTL,
                true,
            ),
            MarkerDisposition::KeepWaiting,
        );
        assert_eq!(
            decide_deferred_claim_marker_disposition(
                1_000 + TTL_MS,
                &m,
                absent,
                ABORT_MARKER_TTL,
                true,
            ),
            MarkerDisposition::DeliverFailureWarn,
            "R4 bound: TTL elapsed with no row and no cover → bounded ⚠"
        );
    }

    /// Covered and http-unavailable arms mirror the Abort kind exactly:
    /// completion always wins (pinned row or not), and no-http leaves every
    /// marker intact (I6 fail-open).
    #[test]
    fn covered_and_http_arms_match_abort_kind() {
        let mut covered = deferred_marker(1_000);
        covered.covered_at_ms = Some(2_000);
        for probe in [
            LiveInflightProbe {
                defers: false,
                is_pinned_turn: false,
            },
            LiveInflightProbe {
                defers: true,
                is_pinned_turn: true,
            },
        ] {
            assert_eq!(
                decide_deferred_claim_marker_disposition(
                    1_000 + TTL_MS * 10,
                    &covered,
                    probe,
                    ABORT_MARKER_TTL,
                    true,
                ),
                MarkerDisposition::DeliverCompletion,
            );
            assert_eq!(
                decide_deferred_claim_marker_disposition(
                    1_000 + TTL_MS * 10,
                    &deferred_marker(1_000),
                    probe,
                    ABORT_MARKER_TTL,
                    false,
                ),
                MarkerDisposition::LeftIntactHttpUnavailable,
            );
        }
    }

    // ====================================================================
    // #3350 ② — `ensure_marker_for_own_synthetic_turn` (finalize-time
    // idempotent ensure). Durable-store tests use the mod.rs `TestRoot`
    // pattern: a per-test tempdir via the THREAD-LOCAL store override.
    // ====================================================================

    struct TestRoot {
        _temp: tempfile::TempDir,
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            super::super::set_test_root_override(None);
        }
    }

    fn test_root() -> TestRoot {
        let temp = tempfile::tempdir().unwrap();
        super::super::set_test_root_override(Some(temp.path().to_path_buf()));
        TestRoot { _temp: temp }
    }

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// (a) #3350: with no marker present the ensure records one — uncovered,
    /// pinned to the OWN identity, kind `DeferredClaim` (exactly what the
    /// claim-time record would have written).
    #[test]
    fn ensure_records_uncovered_own_pin_when_absent() {
        let _root = test_root();
        let outcome =
            ensure_marker_for_own_synthetic_turn("claude", 1, 900, "tmux-1", OWN_STARTED_AT, None);
        assert_eq!(
            outcome,
            EnsureClaimMarkerOutcome::Recorded { covered: false }
        );
        let loaded = super::super::load_for_channel("claude", 1);
        assert_eq!(loaded.len(), 1);
        let m = &loaded[0];
        assert_eq!(m.origin, MarkerOrigin::DeferredClaim);
        assert_eq!(m.anchor_message_id, 900);
        assert_eq!(m.foreign_user_msg_id, Some(900), "OWN pin only (SC1)");
        assert_eq!(m.foreign_started_at.as_deref(), Some(OWN_STARTED_AT));
        assert_eq!(m.covered_at_ms, None);
    }

    /// (b) #3350 / I6: an EXISTING marker is never overwritten — RED if the
    /// ensure re-records (the covered_at stamp would vanish, demoting an
    /// answered anchor to the TTL `⚠`, and the aborted_at TTL clock would
    /// reset, deferring the bounded `⚠` forever under repeated finalizes).
    #[test]
    fn ensure_never_overwrites_existing_marker() {
        let _root = test_root();
        let mut existing = deferred_marker(1_000);
        existing.covered_at_ms = Some(2_000);
        super::super::record(&existing).unwrap();
        let outcome = ensure_marker_for_own_synthetic_turn(
            "claude",
            1,
            10,
            "tmux-1",
            "2026-06-11 09:00:00", // a DIFFERENT started_at must not re-pin
            None,
        );
        assert_eq!(outcome, EnsureClaimMarkerOutcome::AlreadyPresent);
        assert_eq!(
            super::super::load_for_channel("claude", 1),
            vec![existing],
            "covered_at / aborted_at / pin must survive the ensure untouched"
        );
    }

    /// (b2) #3350 codex r1-3: an existing UNCOVERED own-pin DeferredClaim
    /// marker also blocks the ensure — its TTL clock must not reset under
    /// repeated finalizes (the bounded `⚠` would otherwise defer forever).
    #[test]
    fn ensure_keeps_uncovered_own_pin_deferred_claim_marker() {
        let _root = test_root();
        let existing = deferred_marker(1_000);
        super::super::record(&existing).unwrap();
        let outcome = ensure_marker_for_own_synthetic_turn(
            "claude",
            1,
            10,
            "tmux-1",
            "2026-06-11 09:00:00", // a DIFFERENT started_at must not re-pin
            None,
        );
        assert_eq!(outcome, EnsureClaimMarkerOutcome::AlreadyPresent);
        assert_eq!(
            super::super::load_for_channel("claude", 1),
            vec![existing],
            "the aborted_at TTL clock and own pin survive untouched"
        );
    }

    /// (b3) #3350 codex r1-3: an UNCOVERED stale Abort marker (foreign pin,
    /// e.g. left by an abnormal restart) is REPLACED with the own-pin
    /// DeferredClaim marker, mirroring the claim-time `record()` stem
    /// overwrite (#3303 contract / the pending-start R8 reclaim test). RED
    /// pre-fix: `AlreadyPresent` left the foreign pin in place — no commit
    /// could ever cover it, so the TTL false-`⚠`'d the claimed anchor.
    #[test]
    fn ensure_replaces_uncovered_stale_abort_marker_with_own_pin() {
        let _root = test_root();
        super::super::record_for_abort(
            "claude".into(),
            1,
            10,
            "tmux-1".into(),
            Some((999, "2026-06-09 08:00:00".into())),
        )
        .unwrap();
        let outcome =
            ensure_marker_for_own_synthetic_turn("claude", 1, 10, "tmux-1", OWN_STARTED_AT, None);
        assert_eq!(
            outcome,
            EnsureClaimMarkerOutcome::Recorded { covered: false },
            "RED pre-r1-3: the stale Abort marker blocked the ensure as AlreadyPresent"
        );
        let loaded = super::super::load_for_channel("claude", 1);
        assert_eq!(loaded.len(), 1, "one stem, one marker (SC2)");
        let m = &loaded[0];
        assert_eq!(m.origin, MarkerOrigin::DeferredClaim);
        assert_eq!(
            m.foreign_user_msg_id,
            Some(10),
            "OWN pin replaces the foreign pin"
        );
        assert_eq!(m.foreign_started_at.as_deref(), Some(OWN_STARTED_AT));
        assert_eq!(m.covered_at_ms, None);
    }

    /// (b4) #3350 codex r1-3 boundary: a COVERED (tombstoned) Abort marker is
    /// never overwritten — its `✅` evidence must survive (I6: an overwrite
    /// would demote the covered anchor back to the TTL `⚠`).
    #[test]
    fn ensure_preserves_covered_abort_marker() {
        let _root = test_root();
        let mut covered_abort = super::super::record_for_abort(
            "claude".into(),
            1,
            10,
            "tmux-1".into(),
            Some((999, "2026-06-09 08:00:00".into())),
        )
        .unwrap();
        covered_abort.covered_at_ms = Some(2_000);
        super::super::record(&covered_abort).unwrap();
        assert_eq!(
            ensure_marker_for_own_synthetic_turn("claude", 1, 10, "tmux-1", OWN_STARTED_AT, None),
            EnsureClaimMarkerOutcome::AlreadyPresent
        );
        assert_eq!(
            super::super::load_for_channel("claude", 1),
            vec![covered_abort],
            "tombstoned (covered) evidence survives — never overwritten"
        );
    }

    /// (c) #3350: when the own turn already terminal-committed (durable
    /// tombstone), the ensure records a COVERED marker and the sweep delivers
    /// the idempotent `✅` — never the TTL `⚠`.
    #[test]
    fn ensure_covers_from_own_tombstone_and_sweep_delivers_completion() {
        let _root = test_root();
        super::super::record_commit_tombstone_at(5_000, "claude", "tmux-1", 1, 901, OWN_STARTED_AT);
        let outcome =
            ensure_marker_for_own_synthetic_turn("claude", 1, 901, "tmux-1", OWN_STARTED_AT, None);
        assert_eq!(
            outcome,
            EnsureClaimMarkerOutcome::Recorded { covered: true }
        );

        let calls: std::sync::Arc<std::sync::Mutex<Vec<(u64, super::super::ReactionOp)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_for_fn = calls.clone();
        let applier: super::super::ReactionApplierFn = Box::new(move |marker, op| {
            let calls = calls_for_fn.clone();
            let anchor = marker.anchor_message_id;
            Box::pin(async move {
                calls.lock().unwrap().push((anchor, op));
                super::super::ReactionDelivery::Delivered
            })
        });
        let resolved = test_rt().block_on(super::super::sweep_expired_with_applier(
            "claude",
            5_001,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(901, super::super::ReactionOp::Complete)],
            "an already-committed turn converges to the idempotent ✅ (never ⚠)"
        );
        assert!(super::super::load_for_channel("claude", 1).is_empty());
    }

    /// (d) #3350 / I5: a zero anchor id is rejected and nothing is persisted.
    #[test]
    fn ensure_skips_zero_anchor() {
        let _root = test_root();
        assert_eq!(
            ensure_marker_for_own_synthetic_turn("claude", 1, 0, "tmux-1", OWN_STARTED_AT, None),
            EnsureClaimMarkerOutcome::SkippedZeroAnchor
        );
        assert!(super::super::load_for_channel("claude", 1).is_empty());
    }

    /// The bool shorthand (existing test closures) maps to a probe that can
    /// never be the pinned own turn.
    #[test]
    fn bool_probe_shorthand_is_never_pinned() {
        assert_eq!(
            LiveInflightProbe::from(true),
            LiveInflightProbe {
                defers: true,
                is_pinned_turn: false
            }
        );
        assert_eq!(
            LiveInflightProbe::from(false),
            LiveInflightProbe {
                defers: false,
                is_pinned_turn: false
            }
        );
    }
}
