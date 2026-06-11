//! #3296 — durable aborted-anchor markers: reconcile the anchor reaction after
//! a synthetic turn-start ABORT instead of branding it a failure.
//!
//! ## Why this exists
//! When a TUI-direct synthetic turn-start ABORTs on the backstop escalation
//! budget (`backstop_abort_foreign_inflight_live`,
//! [`super::tui_direct_pending_start`]), the input was ALREADY provider-
//! submitted — the abort drops only the synthetic OWNERSHIP claim — and the
//! prior owner then relays the response; the #3282-era `⏳ → ⚠` swap branded
//! that ANSWERED message as failed (#3296).
//!
//! ## Mechanism
//! The ABORT path now KEEPS the `⏳` (it is still true: the provider holds the
//! input) and records a durable [`AbortedAnchorMarker`] here. Two reconcilers
//! own the marker afterwards:
//!
//! 1. **Terminal-commit drain** ([`drain_on_terminal_commit`]) — the tmux
//!    watcher's terminal chokepoint calls this on every body-visible normal
//!    commit. A marker is covered ONLY by a commit whose identity MATCHES the
//!    foreign prior inflight the ABORT recorded (codex r1: positive
//!    correlation — wall-clock alone falsely `✅`'d unanswered anchors), and
//!    covers are deliberately NOT TTL-bounded (verify r1: the sweep defers to
//!    a live same-session inflight, so a foreign turn streaming past the TTL
//!    must still have its eventual commit accepted).
//! 2. **TTL sweep** ([`sweep_expired`]) — the placeholder sweeper's pass: once
//!    [`ABORT_MARKER_TTL`] elapsed with NO live inflight for the session (a
//!    long streaming turn holds the verdict; a NAME-LESS inflight holds only
//!    when it IS the recorded foreign turn — r1 finding 2), nothing ever
//!    covered the anchor → `⏳ → ⚠`: a genuine failure still surfaces in
//!    bounded time (no #3282 eternal hourglass), and the hold itself is capped
//!    by [`ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER`] so a stale/recreated
//!    same-channel inflight can never orphan a `⏳` forever.
//!
//! The two reconcilers are mutually excluded per marker by a non-blocking
//! sidecar flock claim ([`try_claim_marker`], the `inflight.rs` sidecar-lock
//! pattern): claim → re-read → react → delete, so a commit racing the sweep's
//! delivery window can never stack `✅` + `⚠` on one anchor (verify r1).
//!
//! **Commit tombstones** (codex r2) make the RIGHT verdict win those races,
//! not just the first claimant: the watcher chokepoint durably records a
//! [`CommitTombstone`] BEFORE it clears the inflight row and runs the drain.
//! The sweep 대조s uncovered markers against them (post-live-read, so a
//! commit-caused row clear is never mistaken for "nothing covers this"), and
//! the ABORT record path 대조s once at record time — bare row-absence is
//! never commit evidence (force-clears also delete rows).
//!
//! ## Invariants
//! * **I1 (#3164 add≡remove)**: every reaction op (`⏳` remove, `✅`/`⚠` add)
//!   resolves `shared.serenity_http_or_token_fallback()` INSIDE this module —
//!   the same bot identity that added the `⏳`. No caller-provided http is
//!   accepted, so a watcher/sweeper-bootstrap http can never be misused.
//! * **I4 (turn-identity pin)**: every correction targets ONLY the marker's own
//!   `anchor_message_id` — the shared `prompt_anchor_by_tmux` slot is never
//!   re-read (slot aliasing under rapid injection would hit the wrong turn).
//! * **I5 (zero-id guard)**: a zero anchor id is never recorded or reacted on
//!   (`MessageId::new(0)` panics).
//! * **I6 (fail-open)**: when http is unavailable or a delivery fails, the
//!   marker is PRESERVED (a covering commit stamps `covered_at_ms` so the
//!   sweep retries the `✅`) — never silently dropped.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serenity::{ChannelId, MessageId};

use poise::serenity_prelude as serenity;

use super::SharedData;

/// How long an aborted anchor may wait for a covering terminal commit before
/// the sweep declares it a genuine failure (`⏳ → ⚠`). Rationale: the observed
/// ABORT→covered window is ~30-180s (backstop 32s + prior-owner long turns);
/// 600s covers a long streaming prior turn while still bounding a truly-lost
/// input to TTL + sweeper initial delay (180s) + pass interval (30s). Gates
/// ONLY the sweep's `⚠` fallback — the terminal-commit drain's `✅` cover is
/// not TTL-bounded (see [`terminal_commit_covers_marker`]).
pub(super) const ABORT_MARKER_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Absolute bound on the sweep's live-inflight hold (codex r1 finding 2):
/// once `aborted_at + TTL × THIS` (6 × 600s = 1 hour) elapses, an UNCOVERED
/// marker takes the `⚠` fallback regardless of any live inflight — without
/// it, back-to-back turns, a recreated same-name tmux session, or a stale
/// same-channel row could renew the hold forever and orphan the `⏳`.
/// Trade-off: a covering prior turn streaming over an hour past the abort
/// loses its hold — accepted; bounded convergence is the contract (#3282).
pub(super) const ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER: u64 = 6;

/// Which lifecycle event recorded a marker — drives ONLY the sweep's
/// disposition choice (#3303). `#[serde(default)]`'d on the marker so legacy
/// on-disk JSON (and a version DOWNGRADE re-reading a `deferred_claim`
/// marker, since the field round-trips as a plain string) deserializes as
/// [`MarkerOrigin::Abort`] — the conservative pre-#3303 semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MarkerOrigin {
    /// The synthetic turn-start ABORTed on the backstop escalation budget
    /// (#3296): the pinned identity is the FOREIGN prior turn the worker
    /// deferred on.
    #[default]
    Abort,
    /// The synthetic turn-start claim SUCCEEDED (#3303): the pinned identity
    /// is the worker's OWN synthetic turn (`user_msg_id == anchor`, own row
    /// `started_at`), so a relay failure / EOF-consumed commit path that never
    /// flips the anchor's `⏳ → ✅` still converges (own commit → drain `✅`;
    /// nothing ever commits → bounded sweep `⚠` instead of an eternal `⏳`).
    DeferredClaim,
}

/// Durable record for an anchor whose synthetic turn-start ABORTed while the
/// input was already provider-submitted (`origin == Abort`, #3296), or whose
/// deferred synthetic claim succeeded but whose own terminal commit must still
/// be proven (`origin == DeferredClaim`, #3303). All fields are primitives so
/// the JSON survives a dcserver version swap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AbortedAnchorMarker {
    pub provider: String,
    pub channel_id: u64,
    /// Identity pin (I4): the ONLY message any `✅`/`⚠` correction may target.
    pub anchor_message_id: u64,
    pub tmux_session_name: String,
    /// Wall-clock ms of the recording event (the ABORT, or the deferred-claim
    /// success). A covering commit must not be earlier (r3).
    pub aborted_at_ms: u64,
    /// Stamped when a covering terminal commit was seen but the `✅` delivery
    /// failed (or http was unavailable) — the sweep retries the completion
    /// instead of ever degrading a covered anchor to `⚠` (I6). Also stamped at
    /// RECORD time on tombstone evidence ([`AbortedAnchorMarker::for_abort`]).
    #[serde(default)]
    pub covered_at_ms: Option<u64>,
    /// Identity of the turn whose terminal commit COVERS this marker
    /// (`inflight.rs` `InflightTurnIdentity` convention; codex r1: positive
    /// correlation). For `Abort` markers this is the live FOREIGN prior
    /// inflight at the ABORT instant; for `DeferredClaim` markers it is the
    /// worker's OWN synthetic turn — NEVER the prior turn (the prior commit's
    /// tombstone is definitionally already durable at claim time, so pinning
    /// it would false-`✅` a still-streaming unanswered turn; #3303 SC1). The
    /// drain covers this marker ONLY on a terminal commit whose turn identity
    /// matches BOTH fields. `None` on legacy (pre-r1) markers: never
    /// drain-covered — the sweep bound alone resolves them.
    #[serde(default)]
    pub foreign_user_msg_id: Option<u64>,
    #[serde(default)]
    pub foreign_started_at: Option<String>,
    /// See [`MarkerOrigin`] (#3303). Legacy JSON defaults to `Abort`.
    #[serde(default)]
    pub origin: MarkerOrigin,
}

impl AbortedAnchorMarker {
    fn file_stem(&self) -> String {
        format!(
            "{}_{}_{}",
            self.provider, self.channel_id, self.anchor_message_id
        )
    }

    /// Build the marker the ABORT path records. `foreign` is the foreign prior
    /// inflight's `(user_msg_id, started_at)`: the worker's LAST-VIEW identity,
    /// with the cleanup-instant row only as the no-view fallback (codex r3 —
    /// see `tui_direct_pending_start::pin_abort_foreign_identity`). ALWAYS
    /// uncovered: bare row-absence is NOT commit evidence (force-clears also
    /// delete rows; r1's "pre-covered" promotion false-`✅`'d those) — coverage
    /// needs the drain or a commit-tombstone 대조. The pair cannot alias
    /// another turn: one row per `(provider, channel)` and a successor starts
    /// ≥ the 32s backstop after `started_at` — equality identifies the turn.
    pub(super) fn for_abort(
        provider: String,
        channel_id: u64,
        anchor_message_id: u64,
        tmux_session_name: String,
        aborted_at_ms: u64,
        foreign: Option<(u64, String)>,
    ) -> Self {
        let (foreign_user_msg_id, foreign_started_at) = match foreign {
            Some((user_msg_id, started_at)) => (Some(user_msg_id), Some(started_at)),
            None => (None, None),
        };
        Self {
            provider,
            channel_id,
            anchor_message_id,
            tmux_session_name,
            aborted_at_ms,
            covered_at_ms: None,
            foreign_user_msg_id,
            foreign_started_at,
            origin: MarkerOrigin::Abort,
        }
    }

    /// `true` iff this marker carries a recorded foreign identity AND it
    /// equals the given turn identity (codex r1: the positive-correlation
    /// test shared by the drain cover and the sweep's name-less-inflight
    /// hold). Identity-absent markers match nothing.
    pub(super) fn matches_foreign_identity(&self, user_msg_id: u64, started_at: &str) -> bool {
        self.foreign_user_msg_id == Some(user_msg_id)
            && self.foreign_started_at.as_deref() == Some(started_at)
    }
}

// ---------------------------------------------------------------------------
// Durable store + commit tombstones (I/O extracted to `store.rs`, #3303 — the
// contracts and doc rationale live there; re-exported here so every external
// caller path `tui_direct_abort_marker::*` is unchanged)
// ---------------------------------------------------------------------------

mod deferred_claim;
mod store;

pub(in crate::services::discord) use deferred_claim::{
    LiveInflightProbe, ensure_marker_for_own_synthetic_turn, record_for_deferred_claim,
};
use store::reload;
#[cfg(test)]
pub(in crate::services::discord) use store::{
    COMMIT_TOMBSTONE_RETENTION_MS, record_commit_tombstone_at, set_test_root_override,
};
pub(in crate::services::discord) use store::{
    CommitTombstone, delete, gc_expired_commit_tombstones, load_all, load_commit_tombstones,
    load_for_channel, record, record_commit_tombstone, try_claim_marker,
};
#[cfg(test)]
use store::{root, tombstone_root};

/// `true` iff this tombstone is commit evidence for this marker's recorded
/// foreign turn: same tmux session AND the committed identity IS the pinned
/// foreign identity (the codex-r1 positive correlation — identity-absent
/// markers match nothing, and an unrelated turn's tombstone never covers).
fn commit_tombstone_matches_marker(marker: &AbortedAnchorMarker, t: &CommitTombstone) -> bool {
    t.tmux_session_name == marker.tmux_session_name
        && marker.matches_foreign_identity(t.committed_user_msg_id, &t.committed_started_at)
}

/// Record-instant 대조 (codex r2 — replaces the unfounded "pre-covered"
/// promotion): a tombstone matching the marker's foreign identity ALREADY
/// durable at record time means that turn terminal-committed before this
/// marker existed (its drain pass could never see it) → stamp covered with
/// the commit instant. No wall-clock condition — the structural argument that
/// makes the sweep 대조's `>=` safe ([`post_abort_commit_tombstone`]) holds a
/// fortiori for evidence predating the marker.
pub(super) fn cover_from_commit_tombstone(marker: &mut AbortedAnchorMarker) -> bool {
    if marker.covered_at_ms.is_some() {
        return false;
    }
    let Some(t) = load_commit_tombstones(&marker.provider, marker.channel_id)
        .into_iter()
        .find(|t| commit_tombstone_matches_marker(marker, t))
    else {
        return false;
    };
    marker.covered_at_ms = Some(t.committed_at_ms);
    true
}

/// Sweep-side 대조: a tombstone covers an UNCOVERED marker when it matches the
/// foreign identity AND its commit is not EARLIER than the abort. `>=`, not
/// `>` (codex r3): identity is the PRIMARY evidence — the ABORT fired because
/// the recorded foreign turn stayed live through the whole 32s backstop, so a
/// commit of THAT turn cannot predate the input submission (itself before the
/// marker existed); an identity-matched same-ms commit is therefore
/// necessarily post-submission, and strict `>` only ever rejected ANSWERED
/// anchors at the ms boundary (false `⚠`). The record-instant 대조 above owns
/// evidence predating the marker; this subsidiary guard mirrors the drain's.
pub(super) fn post_abort_commit_tombstone(marker: &AbortedAnchorMarker) -> Option<u64> {
    load_commit_tombstones(&marker.provider, marker.channel_id)
        .into_iter()
        .find(|t| {
            commit_tombstone_matches_marker(marker, t) && t.committed_at_ms >= marker.aborted_at_ms
        })
        .map(|t| t.committed_at_ms)
}

/// The ABORT path's one-call record: build the marker (always uncovered —
/// codex r2), run the record-instant tombstone 대조, persist. Returns the
/// persisted marker for the caller's structured log fields.
pub(super) fn record_for_abort(
    provider: String,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: String,
    foreign: Option<(u64, String)>,
) -> Result<AbortedAnchorMarker, String> {
    let mut marker = AbortedAnchorMarker::for_abort(
        provider,
        channel_id,
        anchor_message_id,
        tmux_session_name,
        now_ms(),
        foreign,
    );
    cover_from_commit_tombstone(&mut marker);
    record(&marker)?;
    Ok(marker)
}

// ---------------------------------------------------------------------------
// Pure decision functions (truth-table tested — no I/O, no clock)
// ---------------------------------------------------------------------------

/// What the sweep should do with a marker this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MarkerDisposition {
    /// TTL not elapsed, or a live inflight for the session still holds the
    /// verdict (a long prior turn may yet cover the anchor) — re-evaluate next
    /// pass.
    KeepWaiting,
    /// A covering commit was already seen (`covered_at_ms`) — (re)deliver the
    /// `⏳ → ✅` completion.
    DeliverCompletion,
    /// TTL elapsed with no live inflight and no covering commit — deliver the
    /// `⏳ → ⚠` failure fallback (I10: the only path that may `⚠`).
    DeliverFailureWarn,
    /// Http unavailable — leave every marker intact (I6 fail-open).
    LeftIntactHttpUnavailable,
}

/// The sweep's per-marker verdict. Conservative by design (I10): `⚠` requires
/// BOTH an elapsed TTL AND no live inflight for the session (a long prior turn
/// is never falsely branded); `✅` retry requires a previously-seen covering
/// commit. The live-inflight hold is bounded by the hard cap (r1 finding 2) so
/// no inflight churn can orphan the `⏳` — the cap is also the sole terminator
/// for identity-ABSENT markers, which the drain never covers.
pub(super) fn decide_marker_disposition(
    now_ms: u64,
    marker: &AbortedAnchorMarker,
    live_inflight_for_session: bool,
    ttl: std::time::Duration,
    http_available: bool,
) -> MarkerDisposition {
    if !http_available {
        return MarkerDisposition::LeftIntactHttpUnavailable;
    }
    if marker.covered_at_ms.is_some() {
        return MarkerDisposition::DeliverCompletion;
    }
    let elapsed_ms = now_ms.saturating_sub(marker.aborted_at_ms);
    let ttl_ms = ttl.as_millis() as u64;
    if elapsed_ms >= ttl_ms.saturating_mul(ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER) {
        return MarkerDisposition::DeliverFailureWarn;
    }
    if elapsed_ms < ttl_ms || live_inflight_for_session {
        return MarkerDisposition::KeepWaiting;
    }
    MarkerDisposition::DeliverFailureWarn
}

/// Does a terminal commit by `(committed_user_msg_id, committed_started_at)`
/// cover this marker? Codex r1: POSITIVE correlation required — the committed
/// turn must BE the foreign prior inflight recorded at ABORT time (the old
/// wall-clock-only condition let a racing prior-owner commit, a re-created
/// same-name tmux session, and a dropped-input prior commit each false-`✅` a
/// possibly-unanswered anchor); not-earlier-than-abort stays as a SUBSIDIARY
/// guard only (`>=` since codex r3 — a commit in the abort's OWN millisecond
/// covers; safety argument at [`post_abort_commit_tombstone`]). Identity-
/// absent (legacy) markers never cover here — the sweep bound is their sole
/// terminator. Deliberately NO TTL upper bound (verify r1): the sweep defers
/// to a live same-session inflight, so a foreign turn streaming past the TTL
/// must still have its eventual commit accepted.
pub(super) fn terminal_commit_covers_marker(
    now_ms: u64,
    marker: &AbortedAnchorMarker,
    committed_user_msg_id: u64,
    committed_started_at: &str,
) -> bool {
    marker.anchor_message_id != 0
        && now_ms >= marker.aborted_at_ms
        && marker.matches_foreign_identity(committed_user_msg_id, committed_started_at)
}

/// Does a live same-channel inflight defer the sweep's `⚠` fallback for this
/// marker? A NAME-BEARING row defers on a tmux-session-name match (the
/// long-prior-turn hold). A NAME-LESS row defers ONLY when it IS the recorded
/// foreign prior turn (codex r1 finding 2: the old `is_none_or(..)` treated
/// EVERY name-less same-channel row as "could be the prior owner", letting an
/// unrelated/stale inflight hold the marker; the hard cap bounds even a match).
pub(super) fn inflight_defers_sweep(
    marker: &AbortedAnchorMarker,
    inflight_tmux_session_name: Option<&str>,
    inflight_user_msg_id: u64,
    inflight_started_at: &str,
) -> bool {
    match inflight_tmux_session_name {
        Some(name) => name == marker.tmux_session_name,
        None => marker.matches_foreign_identity(inflight_user_msg_id, inflight_started_at),
    }
}

// ---------------------------------------------------------------------------
// Reaction applier (boxed-fn injection, `ClaimFn`/`AbortCleanupFn` convention)
// ---------------------------------------------------------------------------

/// The reaction correction to apply to the marker's pinned anchor message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReactionOp {
    /// `⏳` remove + `✅` add (anchor covered by the prior owner).
    Complete,
    /// `⏳` remove + `⚠` add (TTL'd genuine failure).
    FailureWarn,
}

/// Outcome of one applier invocation, driving keep/delete of the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReactionDelivery {
    Delivered,
    /// Transient failure (5xx, rate limit, transport) — keep the marker; a
    /// later pass retries.
    Failed,
    /// Permanent failure (404/403/410, e.g. Unknown Message 10008): the anchor
    /// can NEVER be reacted on again — terminate the marker instead of
    /// retrying forever (verify r1 fix #3).
    FailedPermanent,
    HttpUnavailable,
}

/// Classify a reaction-create failure status into transient vs permanent.
/// Reuses the sweeper's message-gone allowlist (404/403/410;
/// `placeholder_sweeper::is_permanent_message_gone_status`, the #3293-shared
/// classifier) so every Discord-permanence verdict in this subtree agrees.
fn classify_reaction_failure(status: Option<u16>) -> ReactionDelivery {
    if status.is_some_and(super::placeholder_sweeper::is_permanent_message_gone_status) {
        ReactionDelivery::FailedPermanent
    } else {
        ReactionDelivery::Failed
    }
}

/// Boxed applier so tests record ops instead of calling Discord. The PRODUCTION
/// applier is [`shared_reaction_applier`]; per I1 it does NOT accept an http
/// parameter — it resolves `shared.serenity_http_or_token_fallback()` per call.
pub(super) type ReactionApplierFn = Box<
    dyn Fn(
            &AbortedAnchorMarker,
            ReactionOp,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ReactionDelivery> + Send>>
        + Send
        + Sync,
>;

/// The production applier. Bot identity (#3164 add≡remove, I1): the `⏳` was
/// added via the relay's `shared.serenity_http_or_token_fallback()`, and
/// `remove_reaction_raw` only removes `@me`'s reaction — resolving the SAME
/// source guarantees the removal targets exactly the reaction the add created.
/// Success is keyed on the `✅`/`⚠` create (the remove is best-effort,
/// mirroring `complete_tui_direct_prompt_anchor_lifecycle_if_present`).
pub(super) fn shared_reaction_applier(shared: Arc<SharedData>) -> ReactionApplierFn {
    Box::new(move |marker, op| {
        let shared = shared.clone();
        let provider = marker.provider.clone();
        let channel_id = marker.channel_id;
        let anchor_message_id = marker.anchor_message_id;
        Box::pin(async move {
            if anchor_message_id == 0 {
                return ReactionDelivery::Failed; // I5 (defensive; record() already rejects)
            }
            let Some(http) = shared.serenity_http_or_token_fallback() else {
                return ReactionDelivery::HttpUnavailable;
            };
            let channel = ChannelId::new(channel_id);
            let message = MessageId::new(anchor_message_id);
            super::formatting::remove_reaction_raw(&http, channel, message, '⏳').await;
            let emoji = match op {
                ReactionOp::Complete => '✅',
                ReactionOp::FailureWarn => '⚠',
            };
            let reaction = serenity::ReactionType::Unicode(emoji.to_string());
            match channel.create_reaction(&http, message, reaction).await {
                Ok(_) => ReactionDelivery::Delivered,
                Err(error) => {
                    let status = match &error {
                        serenity::Error::Http(http_err) => {
                            http_err.status_code().map(|status| status.as_u16())
                        }
                        _ => None,
                    };
                    let delivery = classify_reaction_failure(status);
                    // The permanent case logs ONCE at its termination site in
                    // the reconciler (where the marker is deleted) — not here.
                    if delivery == ReactionDelivery::Failed {
                        tracing::warn!(
                            provider = %provider,
                            channel_id,
                            anchor_message_id,
                            op = ?op,
                            error = %error,
                            "tui_direct_abort_marker: reaction correction delivery failed transiently; marker preserved for retry (I6)"
                        );
                    }
                    delivery
                }
            }
        })
    })
}

// ---------------------------------------------------------------------------
// Reconcilers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Watcher terminal-commit chokepoint: a body-visible normal commit covers
/// every marker whose recorded foreign identity matches the COMMITTED turn
/// (codex r1) → `⏳ → ✅`. Returns markers drained.
pub(super) async fn drain_on_terminal_commit(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
) -> usize {
    let applier = shared_reaction_applier(shared.clone());
    drain_on_terminal_commit_with_applier(
        provider,
        tmux_session_name,
        channel_id,
        now_ms(),
        committed_user_msg_id,
        committed_started_at,
        &applier,
    )
    .await
}

pub(super) async fn drain_on_terminal_commit_with_applier(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    now_ms: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    applier: &ReactionApplierFn,
) -> usize {
    let mut drained = 0usize;
    for marker in load_for_channel(provider, channel_id) {
        if marker.tmux_session_name != tmux_session_name {
            continue; // I4: a different session's marker is never this commit's
        }
        // Mutual exclusion vs the sweep (verify r1 fix #2): claim, then decide
        // on a FRESH re-read — the sweep may have resolved it since load.
        let Some(_claim) = try_claim_marker(&marker) else {
            continue; // the sweep owns this marker right now
        };
        let Some(mut marker) = reload(&marker) else {
            continue; // resolved while unclaimed
        };
        if !terminal_commit_covers_marker(
            now_ms,
            &marker,
            committed_user_msg_id,
            committed_started_at,
        ) {
            continue;
        }
        match applier(&marker, ReactionOp::Complete).await {
            ReactionDelivery::Delivered => {
                delete(&marker);
                drained += 1;
                tracing::info!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    tmux_session_name = %marker.tmux_session_name,
                    anchor_message_id = marker.anchor_message_id,
                    origin = ?marker.origin,
                    "tui_direct_abort_marker: anchor covered by the pinned turn's terminal commit; ⏳ → ✅ delivered and marker drained (#3296/#3303)"
                );
            }
            ReactionDelivery::FailedPermanent => {
                // Permanently gone anchor (404/403/410): no reaction can EVER
                // land; terminate, don't retry (verify r1 fix #3). WARN here only.
                delete(&marker);
                tracing::warn!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    anchor_message_id = marker.anchor_message_id,
                    "tui_direct_abort_marker: anchor permanently gone (404/403/410); covered marker terminated without ✅ (#3296)"
                );
            }
            ReactionDelivery::Failed | ReactionDelivery::HttpUnavailable => {
                // I6 fail-open: the anchor IS covered — stamp it so the sweep
                // retries the ✅ (and can never degrade it to ⚠).
                marker.covered_at_ms = Some(now_ms);
                if let Err(error) = record(&marker) {
                    // verify r1 fix #4: surface loudly — a swallowed stamp
                    // failure would let the sweep ⚠ a COVERED anchor after the
                    // TTL; un-stamped, the next covering drain retries the ✅.
                    tracing::error!(
                        provider = %marker.provider,
                        channel_id = marker.channel_id,
                        anchor_message_id = marker.anchor_message_id,
                        error = %error,
                        "tui_direct_abort_marker: failed to persist covered_at stamp after a ✅ delivery failure; next covering commit retries (#3296)"
                    );
                }
            }
        }
    }
    drained
}

/// #3350 issue-1 (lease-gated row-absent commit): a VISIBLE `⏳ → ✅` was just
/// delivered on `anchor_message_id`, but the committed row was already gone,
/// so the watcher chokepoint has no row identity to tombstone. Source it from
/// the marker instead: every DeferredClaim marker pinned to ITS OWN anchor
/// (#3303 SC1) gets its pinned identity recorded as a commit tombstone FIRST
/// (write-before-discard — a sweep claiming the marker mid-pass still 대조s
/// `✅`, never `⚠`), then is discarded under the claim mutex. Reaction-free by
/// design (#3350 I1): the `✅` is already on the message, so the marker's
/// whole job — bounding the `⏳` — is done. Abort-kind markers (foreign pin)
/// are left untouched: this commit proves nothing about the foreign prior
/// turn, so their #3296 convergence (drain / sweep / hard cap) is preserved.
pub(super) fn resolve_own_claim_markers_for_visibly_completed_anchor(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    anchor_message_id: u64,
) -> usize {
    let mut resolved = 0usize;
    for marker in load_for_channel(provider, channel_id) {
        if marker.tmux_session_name != tmux_session_name
            || marker.anchor_message_id != anchor_message_id
            || marker.origin != MarkerOrigin::DeferredClaim
            || marker.foreign_user_msg_id != Some(anchor_message_id)
        {
            continue; // SC1: own-pin DeferredClaim markers of THIS anchor only
        }
        let Some(own_started_at) = marker.foreign_started_at.clone() else {
            continue; // identity-absent marker — the sweep bound alone resolves it
        };
        record_commit_tombstone(
            provider,
            tmux_session_name,
            channel_id,
            anchor_message_id,
            &own_started_at,
        );
        let Some(_claim) = try_claim_marker(&marker) else {
            continue; // a reconciler owns it; the durable tombstone keeps its verdict ✅
        };
        let Some(marker) = reload(&marker) else {
            continue; // resolved while unclaimed
        };
        delete(&marker);
        resolved += 1;
        tracing::info!(
            provider = %marker.provider,
            channel_id = marker.channel_id,
            tmux_session_name = %marker.tmux_session_name,
            anchor_message_id = marker.anchor_message_id,
            "tui_direct_abort_marker: own-pin marker discarded after a visible lease-gated ✅ (row-absent commit); tombstone recorded first so a racing sweep still lands ✅ (#3350)"
        );
    }
    resolved
}

/// Placeholder-sweeper pass: retry `✅` for covered markers; apply the TTL'd
/// `⏳ → ⚠` fallback for anchors no commit ever covered (held while a live
/// inflight may still cover them, up to the hard cap). Returns resolved.
pub(super) async fn sweep_expired(
    shared: &Arc<SharedData>,
    provider: &super::ProviderKind,
) -> usize {
    let http_available = shared.serenity_http_or_token_fallback().is_some();
    let applier = shared_reaction_applier(shared.clone());
    // ONE inflight read feeds both kind predicates (#3303): `defers` is the
    // Abort-kind hold; `is_pinned_turn` is the DeferredClaim-kind hold (the
    // live row IS the marker's pinned own turn — user_msg_id / started_at /
    // tmux session name all match).
    let live_inflight = |marker: &AbortedAnchorMarker| -> LiveInflightProbe {
        match super::inflight::load_inflight_state(provider, marker.channel_id) {
            None => LiveInflightProbe {
                defers: false,
                is_pinned_turn: false,
            },
            Some(state) => LiveInflightProbe {
                defers: inflight_defers_sweep(
                    marker,
                    state.tmux_session_name.as_deref(),
                    state.user_msg_id,
                    &state.started_at,
                ),
                is_pinned_turn: marker
                    .matches_foreign_identity(state.user_msg_id, &state.started_at)
                    && state.tmux_session_name.as_deref()
                        == Some(marker.tmux_session_name.as_str()),
            },
        }
    };
    sweep_expired_with_applier(
        provider.as_str(),
        now_ms(),
        http_available,
        &live_inflight,
        &applier,
    )
    .await
}

/// Generic over the probe return (`Into<LiveInflightProbe>`) so the existing
/// Abort-kind test closures keep their bare-bool form (`From<bool>` maps to a
/// never-pinned probe) while the production sweep supplies the full #3303
/// probe.
pub(super) async fn sweep_expired_with_applier<P: Into<LiveInflightProbe>>(
    provider: &str,
    now_ms: u64,
    http_available: bool,
    live_inflight_for_session: &(dyn Fn(&AbortedAnchorMarker) -> P + Send + Sync),
    applier: &ReactionApplierFn,
) -> usize {
    let mut resolved = 0usize;
    for marker in load_all() {
        if !marker.provider.eq_ignore_ascii_case(provider) {
            continue;
        }
        if marker.anchor_message_id == 0 {
            delete(&marker); // I5: corrupt record — nothing could ever target it
            continue;
        }
        // Mutual exclusion vs the watcher drain (verify r1 fix #2): claim,
        // then decide on a FRESH re-read — a terminal commit racing this pass
        // may have covered or drained the marker since load.
        let Some(_claim) = try_claim_marker(&marker) else {
            continue; // the drain owns this marker right now
        };
        let Some(mut marker) = reload(&marker) else {
            continue; // drained while unclaimed
        };
        // codex r2 ordering: live-inflight read FIRST, tombstone 대조 SECOND.
        // The chokepoint writes the tombstone BEFORE clearing the row, so "no
        // live row" here guarantees a commit-caused clear already made its
        // tombstone visible to the 대조 — a sweep pass claiming the marker
        // mid-commit can no longer beat the correct ✅ with a ⚠ (finding 1;
        // the flock only serializes, it does not order the verdicts).
        let probe: LiveInflightProbe = live_inflight_for_session(&marker).into();
        if marker.covered_at_ms.is_none()
            && let Some(committed_at_ms) = post_abort_commit_tombstone(&marker)
        {
            marker.covered_at_ms = Some(committed_at_ms);
            if let Err(error) = record(&marker) {
                // Loud (verify-r1 fix #4): un-persisted, the stamp re-derives
                // from the tombstone next pass — but a Discord outage outliving
                // the tombstone retention would then ⚠ a covered anchor.
                tracing::error!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    anchor_message_id = marker.anchor_message_id,
                    error = %error,
                    "tui_direct_abort_marker: failed to persist tombstone-covered stamp; next sweep pass re-derives (#3296 r2)"
                );
            }
        }
        // #3303: ONLY the disposition branches on the marker kind. The Abort
        // disposition (incl. its 6×TTL hard cap) is byte-for-byte untouched;
        // the DeferredClaim disposition swaps the live-inflight hold for the
        // pinned-own-turn hold (uncapped) and drops the name-only hold.
        let disposition = match marker.origin {
            MarkerOrigin::Abort => decide_marker_disposition(
                now_ms,
                &marker,
                probe.defers,
                ABORT_MARKER_TTL,
                http_available,
            ),
            MarkerOrigin::DeferredClaim => {
                deferred_claim::decide_deferred_claim_marker_disposition(
                    now_ms,
                    &marker,
                    probe,
                    ABORT_MARKER_TTL,
                    http_available,
                )
            }
        };
        let op = match disposition {
            MarkerDisposition::KeepWaiting | MarkerDisposition::LeftIntactHttpUnavailable => {
                continue;
            }
            MarkerDisposition::DeliverCompletion => ReactionOp::Complete,
            MarkerDisposition::DeliverFailureWarn => ReactionOp::FailureWarn,
        };
        match applier(&marker, op).await {
            ReactionDelivery::Delivered => {
                delete(&marker);
                resolved += 1;
                tracing::info!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    tmux_session_name = %marker.tmux_session_name,
                    anchor_message_id = marker.anchor_message_id,
                    op = ?op,
                    origin = ?marker.origin,
                    "tui_direct_abort_marker: sweep resolved marked anchor (#3296/#3303)"
                );
            }
            ReactionDelivery::FailedPermanent => {
                // Permanently gone anchor (404/403/410): terminate the marker
                // instead of retrying every pass forever (verify r1 fix #3).
                delete(&marker);
                tracing::warn!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    anchor_message_id = marker.anchor_message_id,
                    op = ?op,
                    "tui_direct_abort_marker: anchor permanently gone (404/403/410); marker terminated by sweep (#3296)"
                );
            }
            // I6: keep the marker for the next pass (delivery failed late).
            ReactionDelivery::Failed | ReactionDelivery::HttpUnavailable => {}
        }
    }
    // GC AFTER the marker loop: the first pass after long downtime must 대조
    // against evidence that aged past the retention cap before deleting it.
    gc_expired_commit_tombstones(now_ms);
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Injects a per-test tempdir as the durable BASE root (marker store +
    /// commit-tombstone store are siblings under it — codex r2) via the
    /// THREAD-LOCAL override (never the process-global `AGENTDESK_ROOT_DIR`
    /// env — mutating that races every test that reads the root without the
    /// crate env lock, e.g. the `tui_direct_pending_start` worker tests'
    /// `persist()`). No lock is needed: each test thread sees only its own
    /// override.
    struct TestRoot {
        _temp: tempfile::TempDir,
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            set_test_root_override(None);
        }
    }

    fn test_root() -> TestRoot {
        let temp = tempfile::tempdir().unwrap();
        set_test_root_override(Some(temp.path().to_path_buf()));
        std::fs::create_dir_all(root().expect("durable root configured under temp")).unwrap();
        std::fs::create_dir_all(tombstone_root().expect("tombstone root configured under temp"))
            .unwrap();
        TestRoot { _temp: temp }
    }

    /// A current-thread runtime keeps the async drains on THIS thread so the
    /// thread-local root override resolves inside them (and no
    /// `await_holding_lock` allow sites are needed — the repo ratchet is
    /// frozen at its baseline).
    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// `started_at` of every test marker's recorded foreign prior turn
    /// (`now_string` localtime form, 1-second resolution like production).
    const FOREIGN_STARTED_AT: &str = "2026-06-10 12:00:00";

    fn marker(
        provider: &str,
        channel: u64,
        anchor: u64,
        aborted_at_ms: u64,
    ) -> AbortedAnchorMarker {
        AbortedAnchorMarker {
            provider: provider.to_string(),
            channel_id: channel,
            anchor_message_id: anchor,
            tmux_session_name: format!("tmux-{channel}"),
            aborted_at_ms,
            covered_at_ms: None,
            // codex r1: test markers carry a recorded foreign identity by
            // default (the ABORT path pins one whenever the prior row is live).
            foreign_user_msg_id: Some(anchor + 500_000),
            foreign_started_at: Some(FOREIGN_STARTED_AT.to_string()),
            origin: MarkerOrigin::Abort,
        }
    }

    /// The committed-turn identity that MATCHES `marker()`'s recorded foreign
    /// prior turn — what the watcher passes when that turn terminal-commits.
    fn committed(m: &AbortedAnchorMarker) -> (u64, String) {
        (
            m.foreign_user_msg_id.expect("test markers carry identity"),
            m.foreign_started_at
                .clone()
                .expect("test markers carry identity"),
        )
    }

    type RecordedOps = Arc<Mutex<Vec<(u64, ReactionOp)>>>;

    /// Recording applier (the `recording_abort_cleanup` convention): captures
    /// `(anchor_message_id, op)` so tests pin the identity-pinned target (I4)
    /// and returns a fixed delivery outcome.
    fn recording_applier(outcome: ReactionDelivery) -> (ReactionApplierFn, RecordedOps) {
        let calls: RecordedOps = Arc::new(Mutex::new(Vec::new()));
        let calls_for_fn = calls.clone();
        let applier: ReactionApplierFn = Box::new(move |marker, op| {
            let calls = calls_for_fn.clone();
            let anchor = marker.anchor_message_id;
            Box::pin(async move {
                calls.lock().unwrap().push((anchor, op));
                outcome
            })
        });
        (applier, calls)
    }

    const TTL_MS: u64 = ABORT_MARKER_TTL.as_millis() as u64;

    /// RED-4: the full {ttl}×{live inflight}×{covered}×{http} truth table.
    #[test]
    fn decide_marker_disposition_truth_table() {
        let base = marker("claude", 1, 10, 1_000);
        let covered = AbortedAnchorMarker {
            covered_at_ms: Some(2_000),
            ..base.clone()
        };
        let pre_ttl = 1_000 + TTL_MS - 1;
        let post_ttl = 1_000 + TTL_MS;
        for (now, m, live, http, want) in [
            // http unavailable → ALWAYS left intact (I6), regardless of the rest.
            (
                pre_ttl,
                &base,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                pre_ttl,
                &base,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &base,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &base,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                pre_ttl,
                &covered,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                pre_ttl,
                &covered,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &covered,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &covered,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            // covered → completion retry, before AND after the TTL, inflight or not.
            (
                pre_ttl,
                &covered,
                false,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            (
                pre_ttl,
                &covered,
                true,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            (
                post_ttl,
                &covered,
                false,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            (
                post_ttl,
                &covered,
                true,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            // uncovered, TTL not elapsed → wait (no premature ⚠, RED if ⚠ here).
            (pre_ttl, &base, false, true, MarkerDisposition::KeepWaiting),
            (pre_ttl, &base, true, true, MarkerDisposition::KeepWaiting),
            // uncovered, TTL elapsed, live inflight → HOLD (long-turn ⚠ guard, I10).
            (post_ttl, &base, true, true, MarkerDisposition::KeepWaiting),
            // uncovered, TTL elapsed, no inflight → the ONLY ⚠ path (I10).
            (
                post_ttl,
                &base,
                false,
                true,
                MarkerDisposition::DeliverFailureWarn,
            ),
        ] {
            assert_eq!(
                decide_marker_disposition(now, m, live, ABORT_MARKER_TTL, http),
                want,
                "now={now} covered={:?} live={live} http={http}",
                m.covered_at_ms
            );
        }
    }

    #[test]
    fn terminal_commit_cover_accepts_same_ms_and_rejects_earlier() {
        let m = marker("claude", 1, 10, 5_000);
        let (cid, cstart) = committed(&m);
        // codex r3 (RED ① — pure): a commit in the abort's OWN millisecond is
        // a real cover — identity is the primary evidence (the foreign turn
        // was live through the backstop, so its commit cannot predate the
        // input submission); strict `>` falsely `⚠`'d these ANSWERED anchors.
        // Strictly-earlier wall-clocks still never cover (subsidiary guard).
        assert!(terminal_commit_covers_marker(5_000, &m, cid, &cstart));
        assert!(!terminal_commit_covers_marker(4_999, &m, cid, &cstart));
        assert!(!terminal_commit_covers_marker(4_000, &m, cid, &cstart));
        assert!(terminal_commit_covers_marker(5_001, &m, cid, &cstart));
        assert!(terminal_commit_covers_marker(
            5_000 + TTL_MS,
            &m,
            cid,
            &cstart
        ));
        // verify r1 fix #1: a commit PAST the TTL still covers — the sweep
        // defers to a live same-session inflight indefinitely, so a foreign
        // turn streaming longer than the TTL must not lose its cover (RED on
        // the old `<= ttl` bound: false ⚠ on an answered anchor).
        assert!(terminal_commit_covers_marker(
            5_000 + TTL_MS + 1,
            &m,
            cid,
            &cstart
        ));
        // Zero anchor id never covers (I5).
        let zero = AbortedAnchorMarker {
            anchor_message_id: 0,
            ..m
        };
        assert!(!terminal_commit_covers_marker(5_001, &zero, cid, &cstart));
    }

    /// codex r1 (RED ① — pure): a commit by a DIFFERENT turn must NOT cover,
    /// however plausible its wall-clock. RED pre-fix: the cover test was
    /// `now_ms > aborted_at_ms` alone, so a prior-owner commit racing the
    /// marker write, a recreated same-name tmux session, and a prior turn
    /// that DROPPED the queued input all false-`✅`'d the anchor.
    #[test]
    fn terminal_commit_cover_requires_matching_foreign_identity() {
        let m = marker("claude", 1, 11, 5_000);
        let (cid, cstart) = committed(&m);
        assert!(
            terminal_commit_covers_marker(6_000, &m, cid, &cstart),
            "sanity: the recorded foreign turn's own commit covers"
        );
        assert!(
            !terminal_commit_covers_marker(6_000, &m, cid + 1, &cstart),
            "a different user_msg_id is a different turn — no cover (RED ①)"
        );
        assert!(
            !terminal_commit_covers_marker(6_000, &m, cid, "2026-06-10 12:00:01"),
            "a different started_at is a different turn — no cover"
        );
        let legacy = AbortedAnchorMarker {
            foreign_user_msg_id: None,
            foreign_started_at: None,
            ..m
        };
        assert!(
            !terminal_commit_covers_marker(6_000, &legacy, cid, &cstart),
            "an identity-absent (legacy) marker is sweep-only — never drain-covered"
        );
    }

    /// codex r2 (reverses the r1 pre-covered semantics): the ABORT constructor
    /// NEVER records a covered marker. Bare row-absence is not commit evidence
    /// — a placeholder-sweeper/stop/recovery force-clear also deletes rows, so
    /// the r1 `foreign: None ⇒ covered_at = aborted_at` promotion false-`✅`'d
    /// unanswered anchors (RED on the r1 code: `raced.covered_at_ms` was
    /// `Some(aborted_at)`). Coverage now requires positive evidence (the drain
    /// or a commit-tombstone 대조 — see
    /// `record_instant_tombstone_covers_when_row_vanished_for_commit`).
    #[test]
    fn for_abort_pins_identity_and_never_pre_covers() {
        let live = AbortedAnchorMarker::for_abort(
            "claude".into(),
            9,
            901,
            "tmux-9".into(),
            42_000,
            Some((777, "2026-06-10 09:00:00".into())),
        );
        assert_eq!(live.foreign_user_msg_id, Some(777));
        assert_eq!(
            live.foreign_started_at.as_deref(),
            Some("2026-06-10 09:00:00")
        );
        assert_eq!(
            live.covered_at_ms, None,
            "a still-live foreign turn has covered nothing yet"
        );
        let raced =
            AbortedAnchorMarker::for_abort("claude".into(), 9, 902, "tmux-9".into(), 42_000, None);
        assert_eq!(
            raced.covered_at_ms, None,
            "row-absence alone must NOT cover (r2: the deletion may be a \
             non-commit force-clear — only tombstone evidence may cover)"
        );
        assert_eq!(raced.foreign_user_msg_id, None);
        assert_eq!(raced.foreign_started_at, None);
    }

    /// codex r1 finding 2 (RED ③ — pure): a NAME-LESS same-channel inflight
    /// defers the sweep ONLY when it IS the recorded foreign turn. RED
    /// pre-fix: `is_none_or(..)` held for EVERY name-less row, so an
    /// unrelated/stale inflight kept the marker `KeepWaiting` indefinitely
    /// (orphaned ⏳). Name-bearing rows keep the session-name hold.
    #[test]
    fn nameless_inflight_defers_sweep_only_on_foreign_identity_match() {
        let m = marker("claude", 1, 12, 5_000);
        let (cid, cstart) = committed(&m);
        // Name-bearing rows: session-name match decides (unchanged).
        assert!(inflight_defers_sweep(&m, Some("tmux-1"), 0, ""));
        assert!(!inflight_defers_sweep(&m, Some("tmux-other"), cid, &cstart));
        // Name-less rows: positive identity match only.
        assert!(inflight_defers_sweep(&m, None, cid, &cstart));
        assert!(
            !inflight_defers_sweep(&m, None, cid + 1, &cstart),
            "an unrelated name-less inflight must not hold the marker (RED ③)"
        );
        assert!(!inflight_defers_sweep(&m, None, cid, "1999-01-01 00:00:00"));
        let legacy = AbortedAnchorMarker {
            foreign_user_msg_id: None,
            foreign_started_at: None,
            ..m
        };
        assert!(
            !inflight_defers_sweep(&legacy, None, cid, &cstart),
            "an identity-absent marker gains no name-less hold"
        );
    }

    /// codex r1 finding 2 (hard cap): past `TTL × ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER`
    /// an UNCOVERED marker takes the `⚠` fallback even with a live holding
    /// inflight — the hold can no longer be renewed forever (RED pre-fix:
    /// unconditional `KeepWaiting` while any holding inflight existed).
    /// A COVERED marker stays immune: completion always wins.
    #[test]
    fn sweep_hard_cap_overrides_live_inflight_hold() {
        let base = marker("claude", 1, 13, 1_000);
        let cap = 1_000 + TTL_MS * ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER;
        assert_eq!(
            decide_marker_disposition(cap - 1, &base, true, ABORT_MARKER_TTL, true),
            MarkerDisposition::KeepWaiting,
            "inside the cap a live inflight still holds the verdict"
        );
        assert_eq!(
            decide_marker_disposition(cap, &base, true, ABORT_MARKER_TTL, true),
            MarkerDisposition::DeliverFailureWarn,
            "at the cap the ⚠ fallback fires regardless of the inflight (RED: eternal hold)"
        );
        let covered = AbortedAnchorMarker {
            covered_at_ms: Some(2_000),
            ..base
        };
        assert_eq!(
            decide_marker_disposition(cap + 1, &covered, true, ABORT_MARKER_TTL, true),
            MarkerDisposition::DeliverCompletion,
            "a covered anchor is never degraded to ⚠, cap or not"
        );
    }

    /// I5: the recorder refuses zero anchor ids outright.
    #[test]
    fn record_rejects_zero_anchor_id() {
        let _root = test_root();
        let zero = AbortedAnchorMarker {
            anchor_message_id: 0,
            ..marker("claude", 7, 1, 100)
        };
        assert!(record(&zero).is_err());
        assert!(load_all().is_empty());
    }

    /// Restart survival: a persisted marker reloads with full field fidelity
    /// so the post-restart sweep handles it identically.
    #[test]
    fn durable_roundtrip_survives_reload() {
        let _root = test_root();
        let mut m = marker("codex", 42, 9001, 123_456);
        m.covered_at_ms = Some(123_999);
        record(&m).unwrap();
        let loaded = load_for_channel("codex", 42);
        assert_eq!(loaded, vec![m.clone()]);
        delete(&m);
        assert!(load_for_channel("codex", 42).is_empty());
    }

    /// RED-1 (covered direction): a same-(provider,tmux,channel) terminal
    /// commit after the abort drains the marker with EXACTLY ONE `Complete`
    /// op on the pinned anchor id — and never a `⚠`.
    #[test]
    fn drain_on_terminal_commit_completes_covered_marker() {
        let _root = test_root();
        let m = marker("claude", 100, 555, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let drained = test_rt().block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, // commit strictly after the abort, within TTL
            cid, &cstart, &applier,
        ));
        assert_eq!(drained, 1);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(555, ReactionOp::Complete)],
            "exactly one ⏳→✅ on the marker's own anchor id (I4), ⚠ never — \
             RED if the drain skips the marker (the 10:52 ⚠-on-answered case) \
             or targets a shared slot"
        );
        assert!(
            load_for_channel("claude", 100).is_empty(),
            "delivered completion must drain the durable marker"
        );
    }

    /// I4/R3 + identity scoping: a commit for a DIFFERENT tmux session or a
    /// commit strictly BEFORE the abort ms must not touch the marker (a
    /// same-ms commit covers since codex r3 — see the same-ms test below).
    #[test]
    fn drain_skips_foreign_session_and_pre_abort_commit() {
        let _root = test_root();
        let m = marker("claude", 100, 556, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let rt = test_rt();
        // Foreign tmux session on the same channel → no-op.
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude",
            "tmux-other",
            100,
            10_500,
            cid,
            &cstart,
            &applier,
        ));
        assert_eq!(drained, 0);
        // Commit strictly earlier than the abort → no-op (clock anomaly).
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 9_999, cid, &cstart, &applier,
        ));
        assert_eq!(drained, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(load_for_channel("claude", 100).len(), 1, "marker retained");
    }

    /// codex r1 (RED ① — drain level): a body-visible commit by an UNRELATED
    /// turn on the SAME `(provider, tmux, channel)` leaves the marker
    /// untouched; the recorded foreign turn's own commit then drains it.
    /// RED pre-fix: the unrelated commit `✅`'d the possibly-unanswered anchor
    /// (wall-clock was the only test).
    #[test]
    fn drain_refuses_unrelated_commit_and_accepts_foreign_identity_commit() {
        let _root = test_root();
        let m = marker("claude", 100, 580, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let rt = test_rt();
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude",
            "tmux-100",
            100,
            10_500,
            cid + 7, // an unrelated turn (recreated session / dropped-input prior)
            &cstart,
            &applier,
        ));
        assert_eq!(drained, 0, "no positive identity match → no cover (RED ①)");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(load_for_channel("claude", 100).len(), 1, "marker retained");
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 11_000, cid, &cstart, &applier,
        ));
        assert_eq!(drained, 1, "the foreign turn's own commit covers (②)");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(580, ReactionOp::Complete)]
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// codex r1 schema compat: a legacy (pre-r1) marker JSON without the
    /// foreign-identity fields loads with `None` identity — NEVER
    /// drain-covered (no wall-clock fallback), terminated by the sweep's
    /// TTL/hard-cap bound alone.
    #[test]
    fn legacy_marker_without_identity_fields_is_sweep_only() {
        let _root = test_root();
        let store = root().unwrap();
        std::fs::write(
            store.join("claude_100_590.json"),
            r#"{"provider":"claude","channel_id":100,"anchor_message_id":590,"tmux_session_name":"tmux-100","aborted_at_ms":10000}"#,
        )
        .unwrap();
        let loaded = load_for_channel("claude", 100);
        assert_eq!(
            loaded.len(),
            1,
            "legacy schema must keep parsing (#[serde(default)])"
        );
        assert_eq!(loaded[0].foreign_user_msg_id, None);
        assert_eq!(loaded[0].foreign_started_at, None);
        assert_eq!(loaded[0].covered_at_ms, None);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let rt = test_rt();
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude",
            "tmux-100",
            100,
            10_500,
            12_345,
            FOREIGN_STARTED_AT,
            &applier,
        ));
        assert_eq!(drained, 0, "identity-absent marker is never drain-covered");
        assert!(calls.lock().unwrap().is_empty());
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1, "the sweep bound terminates it");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(590, ReactionOp::FailureWarn)]
        );
    }

    /// I6: a covering commit whose ✅ delivery FAILS preserves the marker with
    /// `covered_at_ms` stamped, and the next sweep retries the COMPLETION
    /// (never degrades the covered anchor to ⚠ even past the TTL).
    #[test]
    fn failed_delivery_stamps_covered_and_sweep_retries_completion() {
        let _root = test_root();
        let m = marker("claude", 100, 557, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let rt = test_rt();
        let (failing, _calls) = recording_applier(ReactionDelivery::Failed);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, cid, &cstart, &failing,
        ));
        assert_eq!(drained, 0);
        let kept = load_for_channel("claude", 100);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].covered_at_ms,
            Some(10_500),
            "failed ✅ delivery must stamp covered_at and keep the marker (I6) — \
             RED if the marker is dropped (silent loss) or left unstamped (would ⚠ a covered anchor)"
        );
        // Sweep far past the TTL with no inflight: still retries ✅, never ⚠.
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS * 2,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(557, ReactionOp::Complete)]
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// RED-2 (a): TTL elapsed but a live inflight for the session exists —
    /// the sweep HOLDS (no reaction op, marker preserved).
    #[test]
    fn sweep_holds_while_live_inflight_present() {
        let _root = test_root();
        let m = marker("claude", 100, 558, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| true, // live inflight for the session
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(
            calls.lock().unwrap().is_empty(),
            "a long-running prior turn must hold the ⚠ verdict (I10) — \
             RED if the sweep warns while an inflight is live (false ⚠ on a long turn)"
        );
        assert_eq!(load_for_channel("claude", 100).len(), 1);
    }

    /// RED-2 (b): TTL elapsed and NO live inflight — the sweep delivers the
    /// `⏳ → ⚠` fallback exactly once on the pinned anchor and drains the
    /// marker (bounded convergence: no #3282 eternal hourglass).
    #[test]
    fn sweep_warns_after_ttl_without_inflight() {
        let _root = test_root();
        let m = marker("claude", 100, 559, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(559, ReactionOp::FailureWarn)],
            "a genuinely-uncovered anchor must reach ⚠ in bounded time — \
             RED if the sweep never warns (the ⏳ would linger forever, #3282)"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// RED-2 (c) / I6: http unavailable — EVERY marker is preserved untouched.
    #[test]
    fn sweep_preserves_all_when_http_unavailable() {
        let _root = test_root();
        let m = marker("claude", 100, 560, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            false, // http unavailable
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            load_for_channel("claude", 100).len(),
            1,
            "http-unavailable must fail open (marker preserved for the next pass, I6)"
        );
    }

    /// #3296 verify r1 (major): an ABORT followed by a foreign turn that
    /// streams LONGER than the TTL. The sweep correctly holds while the
    /// inflight is live, so the marker outlives the TTL — the eventual
    /// covering commit must STILL drain `⏳ → ✅`, and the next sweep (inflight
    /// gone) must find nothing to `⚠`. RED before verify-r1: the drain's TTL
    /// bound refused the late cover (no `covered_at` stamp either), so the
    /// very next sweep pass attached a false `⚠` to an ANSWERED anchor — the
    /// exact symptom this PR exists to fix.
    #[test]
    fn late_covering_commit_after_long_turn_still_completes() {
        let _root = test_root();
        let m = marker("claude", 100, 563, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let rt = test_rt();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        // Sweep passes during the long foreign turn: live inflight → hold.
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| true,
            &applier,
        ));
        assert_eq!(resolved, 0);
        // The foreign turn finally commits ~100s past the TTL.
        let commit_at = 10_000 + TTL_MS + 100_000;
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, commit_at, cid, &cstart, &applier,
        ));
        assert_eq!(
            drained, 1,
            "a covering commit after a >TTL foreign turn must still complete \
             the anchor — RED pre-verify-r1 (drain refused TTL-expired covers \
             while the sweep deferred to the same live inflight)"
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(563, ReactionOp::Complete)]
        );
        // Inflight cleared after the commit: the next sweep must have nothing
        // left to ⚠ (the marker was drained above).
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            commit_at + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "no ⚠ may ever land on a covered anchor"
        );
    }

    /// codex r2 finding 1 (RED ① — REVERSES the r1
    /// `sweep_and_drain_cannot_both_react_to_one_marker` pin, which froze this
    /// exact race as `FailureWarn`): a terminal commit by the recorded foreign
    /// turn landing BETWEEN the sweep's claim and its verdict must end `✅`,
    /// never `⚠` — the flock only serializes the reconcilers, it does not make
    /// the RIGHT verdict win. The chokepoint's tombstone-before-clear write
    /// plus the sweep's live-read-then-대조 ordering closes it: by the time the
    /// sweep can observe "no live row", the commit's tombstone is durable.
    /// Mutual exclusion stays intact (the racing drain still skips — exactly
    /// ONE reaction lands, and it is the completion).
    #[test]
    fn sweep_claim_racing_terminal_commit_resolves_completion_not_warn() {
        let _root = test_root();
        let m = marker("claude", 100, 562, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let commit_at = 10_000 + TTL_MS + 5_000;
        // The watcher chokepoint interleaves inside the sweep's live-inflight
        // read (post-claim, pre-verdict): tombstone FIRST (durable before the
        // row clear), then its drain pass — which must SKIP the sweep-claimed
        // marker (try_claim is the drain's first step) — then the row clear
        // (this closure returns false = no live row).
        let live_inflight = move |marker: &AbortedAnchorMarker| -> bool {
            record_commit_tombstone_at(commit_at, "claude", "tmux-100", 100, cid, &cstart);
            assert!(
                try_claim_marker(marker).is_none(),
                "sweep holds the claim; the racing drain must skip (exclusion)"
            );
            false
        };
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            commit_at + 1_000,
            true,
            &live_inflight,
            &applier,
        ));
        assert_eq!(resolved, 1);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(562, ReactionOp::Complete)],
            "the committed turn's tombstone must decide the verdict: exactly \
             ONE reaction and it is ✅ — RED pre-r2 (without the 대조 the sweep \
             ⚠'d this ANSWERED anchor; the old test pinned that ⚠ as expected)"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// #3296 verify r1 fix #2 (claim semantics): a marker claimed by one
    /// reconciler is SKIPPED by the other — and processed normally once the
    /// claim is released.
    #[test]
    fn claimed_marker_is_skipped_until_released() {
        let _root = test_root();
        let m = marker("claude", 100, 570, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let claim = try_claim_marker(&m).expect("first claim succeeds");
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let rt = test_rt();
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, cid, &cstart, &applier,
        ));
        assert_eq!(drained, 0, "drain must skip a claimed marker");
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0, "sweep must skip a claimed marker");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(load_for_channel("claude", 100).len(), 1);
        drop(claim);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, cid, &cstart, &applier,
        ));
        assert_eq!(drained, 1, "released claim → next pass processes normally");
    }

    /// #3296 verify r1 fix #3: a PERMANENT delivery failure (Unknown Message
    /// 10008 → 404, etc.) terminates the marker on both reconcilers instead of
    /// retrying every pass forever. RED pre-fix: the marker was preserved
    /// indefinitely (covered-stamped by the drain, retried by every sweep).
    #[test]
    fn permanent_delivery_failure_terminates_marker() {
        let _root = test_root();
        let rt = test_rt();
        let (applier, calls) = recording_applier(ReactionDelivery::FailedPermanent);
        // Drain path: covering commit, but the anchor message is gone.
        let m = marker("claude", 100, 571, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, cid, &cstart, &applier,
        ));
        assert_eq!(drained, 0, "a terminated marker is not a delivered ✅");
        assert!(
            load_for_channel("claude", 100).is_empty(),
            "permanently-failed marker must be terminated by the drain, \
             not stamped covered and retried forever"
        );
        // Sweep path: same termination on the ⚠ fallback.
        let m2 = marker("claude", 100, 572, 10_000);
        record(&m2).unwrap();
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(
            load_all().is_empty(),
            "permanently-failed marker must be terminated by the sweep"
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    /// #3296 verify r1 fix #3: permanent-vs-transient classification reuses
    /// the sweeper's message-gone allowlist (404/403/410 permanent; 5xx, 429,
    /// no-status transient).
    #[test]
    fn reaction_failure_classification_matches_sweeper_allowlist() {
        for status in [404, 403, 410] {
            assert_eq!(
                classify_reaction_failure(Some(status)),
                ReactionDelivery::FailedPermanent,
                "{status} must classify permanent"
            );
        }
        for status in [500, 502, 503, 504, 429, 408, 401] {
            assert_eq!(
                classify_reaction_failure(Some(status)),
                ReactionDelivery::Failed,
                "{status} must classify transient"
            );
        }
        assert_eq!(classify_reaction_failure(None), ReactionDelivery::Failed);
    }

    /// #3296 verify r1 fix #3: an unparseable marker is quarantined via a
    /// `.bad` rename — never silently re-parsed (and re-skipped) every pass.
    #[test]
    fn unparseable_marker_is_quarantined_not_relooped() {
        let _root = test_root();
        let store = root().unwrap();
        std::fs::write(store.join("claude_1_2.json"), "{not json").unwrap();
        assert!(load_all().is_empty());
        assert!(
            !store.join("claude_1_2.json").exists(),
            "corrupt marker must be renamed away"
        );
        assert!(
            store.join("claude_1_2.json.bad").exists(),
            "quarantined as .bad for post-mortem"
        );
    }

    /// #3296 verify r1 fix #4: when BOTH the ✅ delivery and the covered_at
    /// stamp rewrite fail, the marker survives un-stamped and the next
    /// covering drain pass still completes it (covers are not TTL-bounded).
    #[cfg(unix)]
    #[test]
    fn covered_stamp_persist_failure_is_retried_by_next_drain() {
        use std::os::unix::fs::PermissionsExt;
        let _root = test_root();
        let store = root().unwrap();
        let m = marker("claude", 100, 573, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        // Pre-create the claim sidecar so the read-only store below blocks
        // ONLY the stamp rewrite, not the claim acquisition.
        drop(try_claim_marker(&m).expect("pre-create claim sidecar"));
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o555)).unwrap();
        let rt = test_rt();
        let (failing, _ops) = recording_applier(ReactionDelivery::Failed);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, cid, &cstart, &failing,
        ));
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(drained, 0);
        let kept = load_for_channel("claude", 100);
        assert_eq!(kept.len(), 1, "marker must survive the failed stamp");
        assert_eq!(
            kept[0].covered_at_ms, None,
            "stamp write failed (read-only store) — marker stays un-stamped"
        );
        // The next covering commit retries and completes (✅ idempotent).
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 11_000, cid, &cstart, &applier,
        ));
        assert_eq!(drained, 1, "next drain pass must retry the cover");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(573, ReactionOp::Complete)]
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// Sweep scoping: another provider's markers are never touched.
    #[test]
    fn sweep_is_provider_scoped() {
        let _root = test_root();
        let m = marker("codex", 100, 561, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(load_for_channel("codex", 100).len(), 1);
    }

    /// codex r2: tombstone store roundtrip — durable, provider/channel scoped.
    #[test]
    fn commit_tombstone_roundtrip_is_provider_and_channel_scoped() {
        let _root = test_root();
        record_commit_tombstone_at(50_000, "claude", "tmux-100", 100, 777, FOREIGN_STARTED_AT);
        record_commit_tombstone_at(50_001, "codex", "tmux-100", 100, 778, FOREIGN_STARTED_AT);
        record_commit_tombstone_at(50_002, "claude", "tmux-200", 200, 779, FOREIGN_STARTED_AT);
        let loaded = load_commit_tombstones("claude", 100);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].committed_user_msg_id, 777);
        assert_eq!(loaded[0].committed_at_ms, 50_000);
        assert_eq!(loaded[0].tmux_session_name, "tmux-100");
        assert_eq!(load_commit_tombstones("codex", 100).len(), 1);
        assert_eq!(load_commit_tombstones("claude", 200).len(), 1);
    }

    /// codex r2 (RED ② — replaces the pre-covered promotion): the ABORT record
    /// path with the foreign row already GONE. With a matching tombstone the
    /// deletion WAS the prior owner's terminal commit → the marker records
    /// covered at the tombstone's commit instant (evidence-backed — note the
    /// commit PRECEDES `aborted_at`, the case the sweep's not-earlier-than-
    /// abort 대조 deliberately excludes). WITHOUT one (a placeholder-sweeper/
    /// recovery force-clear) it records UNCOVERED and the sweep bound delivers
    /// the conservative `⚠` — RED pre-r2: bare row-absence pre-covered the
    /// marker and a force-cleared, genuinely-unanswered anchor got a false ✅.
    #[test]
    fn record_instant_tombstone_evidence_decides_cover_for_vanished_row() {
        let _root = test_root();
        // (a) commit-caused deletion: tombstone for the foreign turn exists.
        record_commit_tombstone_at(
            60_000,
            "claude",
            "tmux-100",
            100,
            600_777,
            FOREIGN_STARTED_AT,
        );
        let covered = record_for_abort(
            "claude".into(),
            100,
            600,
            "tmux-100".into(),
            Some((600_777, FOREIGN_STARTED_AT.into())),
        )
        .unwrap();
        assert_eq!(
            covered.covered_at_ms,
            Some(60_000),
            "matching tombstone at record time ⇒ evidence-backed cover"
        );
        // (b) non-commit deletion (force-clear simulation): no tombstone for
        // THIS identity ⇒ uncovered; the sweep bound then delivers ⚠.
        let uncovered = record_for_abort(
            "claude".into(),
            100,
            601,
            "tmux-100".into(),
            Some((601_888, FOREIGN_STARTED_AT.into())),
        )
        .unwrap();
        assert_eq!(
            uncovered.covered_at_ms, None,
            "row-absence without commit evidence must stay uncovered (RED ②: \
             the r1 pre-covered promotion ✅'d this force-clear case)"
        );
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            uncovered.aborted_at_ms + TTL_MS * ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 2);
        let calls = calls.lock().unwrap();
        assert!(
            calls.contains(&(600, ReactionOp::Complete)),
            "evidence-covered marker completes"
        );
        assert!(
            calls.contains(&(601, ReactionOp::FailureWarn)),
            "force-clear marker takes the conservative ⚠ in bounded time"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// codex r2 (RED ③): GC removes only RETAINED-out tombstones, and once the
    /// matching evidence is gone an UNRELATED turn's fresh tombstone must never
    /// cover the marker (positive identity correlation, exactly like the
    /// drain) — the hard-capped `⚠` stays the terminator.
    #[test]
    fn expired_tombstone_gc_and_unrelated_commit_never_cover() {
        let _root = test_root();
        let m = marker("claude", 100, 564, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        // The foreign turn's own tombstone, but already past retention at the
        // sweep instant below; plus a FRESH unrelated turn's tombstone.
        record_commit_tombstone_at(11_000, "claude", "tmux-100", 100, cid, &cstart);
        let now = 11_000 + COMMIT_TOMBSTONE_RETENTION_MS;
        record_commit_tombstone_at(now - 1_000, "claude", "tmux-100", 100, cid + 7, &cstart);
        // Pass 1: a sweep for a provider with NO markers — it runs only the
        // end-of-pass GC. (A claude pass would 대조 BEFORE the GC and the
        // still-present expired evidence would cover — deliberate
        // post-restart semantics; this test targets the post-GC world.)
        let (applier, _calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "gemini", // no gemini markers: this pass only runs the GC
            now,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        let remaining = load_commit_tombstones("claude", 100);
        assert_eq!(
            remaining.len(),
            1,
            "GC must drop the retention-expired tombstone and keep the fresh one"
        );
        assert_eq!(remaining[0].committed_user_msg_id, cid + 7);
        // Pass 2: only the unrelated tombstone remains — it must NOT cover;
        // the hard cap delivers the conservative ⚠ (RED ③ if an unrelated
        // commit covers or the expired evidence resurrects).
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS * ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(564, ReactionOp::FailureWarn)],
            "an unrelated commit's tombstone must never ✅ a foreign marker"
        );
    }

    /// codex r2: the sweep's tombstone-covered stamp is PERSISTED, so a ✅
    /// delivery failure followed by the tombstone aging out of retention can
    /// never degrade the covered anchor to `⚠` (the persisted `covered_at_ms`
    /// outlives the evidence that produced it).
    #[test]
    fn tombstone_cover_stamp_survives_delivery_failure_and_evidence_gc() {
        let _root = test_root();
        let m = marker("claude", 100, 565, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let commit_at = 20_000;
        record_commit_tombstone_at(commit_at, "claude", "tmux-100", 100, cid, &cstart);
        let rt = test_rt();
        // Pass 1: 대조 covers, but the ✅ delivery fails transiently.
        let (failing, _ops) = recording_applier(ReactionDelivery::Failed);
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            commit_at + 1_000,
            true,
            &|_| false,
            &failing,
        ));
        assert_eq!(resolved, 0);
        let kept = load_for_channel("claude", 100);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].covered_at_ms,
            Some(commit_at),
            "the 대조 stamp must be durable, not in-memory only"
        );
        // Pass 2: far past tombstone retention (evidence GC'd) — the persisted
        // stamp still drives the ✅ retry, never a ⚠.
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            commit_at + COMMIT_TOMBSTONE_RETENTION_MS * 2,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(565, ReactionOp::Complete)]
        );
        assert!(load_commit_tombstones("claude", 100).is_empty());
    }

    /// codex r3 (RED ① — behavior): a terminal commit landing in the SAME
    /// millisecond as the abort covers the marker on BOTH reconcile paths.
    /// RED pre-r3: the strict `commit_at > aborted_at` 대조 refused the cover
    /// — the drain skipped the marker and the sweep's TTL bound then `⚠`'d an
    /// ANSWERED anchor. Safe because identity, not wall-clock, is the primary
    /// evidence: the recorded foreign turn was live through the 32s backstop,
    /// so its commit cannot predate the input submission.
    #[test]
    fn same_ms_commit_covers_via_drain_and_sweep_tombstone() {
        let _root = test_root();
        let rt = test_rt();
        // Drain path: the chokepoint clock equals the abort ms.
        let m = marker("claude", 100, 566, 10_000);
        record(&m).unwrap();
        let (cid, cstart) = committed(&m);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_000, cid, &cstart, &applier,
        ));
        assert_eq!(
            drained, 1,
            "a same-ms commit must cover (RED pre-r3: strict `>` refused it)"
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(566, ReactionOp::Complete)]
        );
        // Sweep 대조 path: tombstone `committed_at_ms == aborted_at_ms`.
        let m2 = marker("claude", 100, 567, 10_000);
        record(&m2).unwrap();
        let (cid2, cstart2) = committed(&m2);
        record_commit_tombstone_at(10_000, "claude", "tmux-100", 100, cid2, &cstart2);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(567, ReactionOp::Complete)],
            "the same-ms tombstone must land ✅, never the TTL ⚠ (RED pre-r3)"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// codex r3 (RED ② — durable evidence): two commits on one
    /// `(provider, channel)` inside the SAME millisecond must BOTH retain
    /// their tombstones. RED pre-r3: the ms-only file stem made the second
    /// write OVERWRITE the first — a marker pinned to the FIRST turn lost its
    /// only commit evidence and degraded to the bounded `⚠` on an answered
    /// anchor.
    #[test]
    fn same_ms_commit_tombstones_preserve_both_evidences() {
        let _root = test_root();
        record_commit_tombstone_at(50_000, "claude", "tmux-100", 100, 777, FOREIGN_STARTED_AT);
        record_commit_tombstone_at(50_000, "claude", "tmux-100", 100, 778, FOREIGN_STARTED_AT);
        let mut ids: Vec<u64> = load_commit_tombstones("claude", 100)
            .into_iter()
            .map(|t| t.committed_user_msg_id)
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![777, 778],
            "both same-ms tombstones must survive (RED pre-r3: the second \
             stem overwrote the first — evidence erased)"
        );
        // The FIRST turn's surviving evidence still 대조-covers its marker
        // (also exercises the r3 `>=`: commit ms == abort ms).
        let mut m = marker("claude", 100, 568, 50_000);
        m.foreign_user_msg_id = Some(777);
        assert_eq!(
            post_abort_commit_tombstone(&m),
            Some(50_000),
            "the first commit's retained tombstone must still cover its marker"
        );
    }

    // ====================================================================
    // #3303 — DeferredClaim-kind markers (own-identity pin for SUCCESSFUL
    // deferred claims). The pure disposition truth table lives in
    // `deferred_claim.rs`; these drive the SHARED reconcilers (drain / sweep
    // / record-instant 대조) against the new kind.
    // ====================================================================

    /// `started_at` of every test marker's OWN synthetic turn (#3303).
    const OWN_STARTED_AT: &str = "2026-06-10 13:00:00";

    fn deferred_marker(
        provider: &str,
        channel: u64,
        anchor: u64,
        claimed_at_ms: u64,
    ) -> AbortedAnchorMarker {
        AbortedAnchorMarker::for_deferred_claim(
            provider.to_string(),
            channel,
            anchor,
            format!("tmux-{channel}"),
            claimed_at_ms,
            (anchor, OWN_STARTED_AT.to_string()),
        )
    }

    /// R2 (#3303 happy path): the watcher chokepoint's existing drain covers a
    /// DeferredClaim marker when the OWN synthetic turn terminal-commits —
    /// exactly one `⏳ → ✅` on the pinned anchor and the marker drains. The
    /// drain is deliberately kind-agnostic (identity is the shared cover
    /// test), so this needs ZERO watcher changes.
    #[test]
    fn deferred_claim_marker_drains_completion_on_own_commit() {
        let _root = test_root();
        let m = deferred_marker("claude", 100, 700, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let drained = test_rt().block_on(drain_on_terminal_commit_with_applier(
            "claude",
            "tmux-100",
            100,
            10_500,
            700, // the OWN synthetic turn: user_msg_id == anchor
            OWN_STARTED_AT,
            &applier,
        ));
        assert_eq!(
            drained, 1,
            "the own turn's commit must cover the deferred-claim marker"
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(700, ReactionOp::Complete)],
            "exactly one ⏳ → ✅ on the pinned anchor (I4)"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// R3 (#3303 SC1 regression guard, lens2-①): the FOREIGN prior turn's
    /// commit tombstone is definitionally already durable at the claim
    /// instant (the claim runs right after the prior finalize) — it must
    /// cover NOTHING: not the record-instant 대조, not the sweep 대조, and the
    /// sweep must converge the marker to the bounded `⚠`, never a `✅`. The
    /// ONLY way to neutralize this test is to pin the foreign identity on the
    /// marker — i.e. the exact false-✅-on-a-streaming-unanswered-turn
    /// regression this guard exists to block.
    #[test]
    fn foreign_prior_tombstone_never_covers_deferred_claim_marker() {
        let _root = test_root();
        // The prior turn (999) committed just before the claim.
        record_commit_tombstone_at(50_000, "claude", "tmux-100", 100, 999, FOREIGN_STARTED_AT);
        let marker = record_for_deferred_claim(
            "claude".into(),
            100,
            701,
            "tmux-100".into(),
            (701, OWN_STARTED_AT.into()),
        )
        .unwrap();
        assert_eq!(
            marker.covered_at_ms, None,
            "the prior turn's tombstone must NOT cover at record time (SC1)"
        );
        assert_eq!(
            post_abort_commit_tombstone(&marker),
            None,
            "the sweep 대조 must not match the prior turn either"
        );
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            marker.aborted_at_ms + TTL_MS,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(701, ReactionOp::FailureWarn)],
            "Mode-1 (response delivered under the PRIOR turn's relay) converges \
             to the bounded ⚠ by design — never a foreign-evidence ✅"
        );
    }

    /// R4 (#3303 bounded convergence) + R9 (no verdict without evidence or
    /// TTL): a DeferredClaim marker whose own turn never commits (relay
    /// failure → watchdog row clear, or the EOF-seeded commit pass never ran)
    /// holds before the TTL even with the row gone, then takes EXACTLY ONE
    /// bounded `⚠`. RED pre-#3303 at the system level: no marker existed at
    /// all, so the anchor's ⏳ was eternal.
    #[test]
    fn deferred_claim_marker_converges_to_bounded_warn_when_never_committed() {
        let _root = test_root();
        let m = deferred_marker("claude", 100, 702, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let rt = test_rt();
        // R9: before the TTL, row absent → no ✅, no ⚠ (row-absence is never
        // commit evidence; the ⚠ must wait for the bound).
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS - 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(
            calls.lock().unwrap().is_empty(),
            "no verdict may land before the TTL without commit evidence (R9)"
        );
        assert_eq!(load_for_channel("claude", 100).len(), 1);
        // R4: at the TTL the bounded ⚠ fires once and the marker drains.
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(702, ReactionOp::FailureWarn)],
            "the eternal-⏳ bug mode must converge to a bounded ⚠ (#3303)"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// #3303: the OWN turn's commit tombstone (chokepoint wrote it, but the
    /// drain raced/failed — e.g. the ✅ delivery failed transiently) covers
    /// the marker via the sweep 대조 → `✅`, never the TTL `⚠`.
    #[test]
    fn deferred_claim_marker_sweep_covers_from_own_commit_tombstone() {
        let _root = test_root();
        let m = deferred_marker("claude", 100, 703, 10_000);
        record(&m).unwrap();
        record_commit_tombstone_at(20_000, "claude", "tmux-100", 100, 703, OWN_STARTED_AT);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(703, ReactionOp::Complete)],
            "own-commit evidence must win over the TTL ⚠"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// R5 at the sweep level (#3303): while the live row IS the pinned own
    /// turn the sweep holds UNCAPPED — even past the Abort kind's 6×TTL hard
    /// cap (a 1h+ streaming own turn must never be false-`⚠`'d; the hold ends
    /// naturally with the row's commit/clear/watchdog). RED if the sweep
    /// routes DeferredClaim markers through the Abort disposition.
    #[test]
    fn sweep_holds_deferred_claim_marker_while_pinned_own_row_lives() {
        let _root = test_root();
        let m = deferred_marker("claude", 100, 704, 10_000);
        record(&m).unwrap();
        let pinned = |_: &AbortedAnchorMarker| LiveInflightProbe {
            defers: true,
            is_pinned_turn: true,
        };
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS * ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER + 1,
            true,
            &pinned,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(
            calls.lock().unwrap().is_empty(),
            "the pinned own row holds the verdict without a cap (RED: Abort \
             disposition would ⚠ here)"
        );
        assert_eq!(load_for_channel("claude", 100).len(), 1);
    }

    /// R8 store-level (#3303 SC2): a stale Abort marker left by an abnormal
    /// restart shares the `(provider, channel, anchor)` stem — a successful
    /// re-claim OVERWRITES it with the refreshed own-identity DeferredClaim
    /// marker (the turn is adopted and live, so the own pin is the truth; no
    /// double-marker is possible on one stem).
    #[test]
    fn record_for_deferred_claim_overwrites_stale_abort_marker() {
        let _root = test_root();
        let stale = record_for_abort(
            "claude".into(),
            100,
            705,
            "tmux-100".into(),
            Some((999, FOREIGN_STARTED_AT.into())),
        )
        .unwrap();
        assert_eq!(stale.origin, MarkerOrigin::Abort);
        let refreshed = record_for_deferred_claim(
            "claude".into(),
            100,
            705,
            "tmux-100".into(),
            (705, OWN_STARTED_AT.into()),
        )
        .unwrap();
        let loaded = load_for_channel("claude", 100);
        assert_eq!(
            loaded,
            vec![refreshed.clone()],
            "one stem, one marker: the re-claim replaces the stale abort marker"
        );
        assert_eq!(refreshed.origin, MarkerOrigin::DeferredClaim);
        assert_eq!(refreshed.foreign_user_msg_id, Some(705));
        assert_eq!(
            refreshed.foreign_started_at.as_deref(),
            Some(OWN_STARTED_AT)
        );
    }

    /// #3303 schema compat: legacy on-disk JSON (no `origin`) deserializes as
    /// `Abort` (the conservative pre-#3303 semantics, also what a version
    /// DOWNGRADE falls back to), and a DeferredClaim marker round-trips.
    #[test]
    fn marker_origin_serde_legacy_default_and_roundtrip() {
        let legacy: AbortedAnchorMarker = serde_json::from_str(
            r#"{"provider":"claude","channel_id":1,"anchor_message_id":2,"tmux_session_name":"tmux-1","aborted_at_ms":3}"#,
        )
        .unwrap();
        assert_eq!(legacy.origin, MarkerOrigin::Abort);
        let m = deferred_marker("claude", 1, 2, 3);
        let back: AbortedAnchorMarker =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m, "DeferredClaim markers must round-trip losslessly");
    }

    // ====================================================================
    // #3350 issue-1 — lease-gated row-absent commit resolution: the visible
    // `⏳ → ✅` path with NO committed row must still resolve the own-pin
    // DeferredClaim marker (tombstone-first + claimed discard, reaction-free).
    // ====================================================================

    /// RED pre-#3350-issue-1: a lease-gated visible ✅ left the own-pin marker
    /// behind, so the TTL sweep stacked a ⚠ next to the delivered ✅. The
    /// resolver must discard the marker AND leave a durable own-identity
    /// tombstone — with ZERO reaction traffic of its own.
    #[test]
    fn lease_gated_completion_discards_own_pin_marker_and_records_tombstone() {
        let _root = test_root();
        let m = deferred_marker("claude", 100, 910, 10_000);
        record(&m).unwrap();

        let resolved =
            resolve_own_claim_markers_for_visibly_completed_anchor("claude", "tmux-100", 100, 910);

        assert_eq!(resolved, 1);
        assert!(
            load_for_channel("claude", 100).is_empty(),
            "RED pre-#3350: the marker survived the visible ✅ and TTL-⚠'d it"
        );
        let tombstones = load_commit_tombstones("claude", 100);
        assert_eq!(
            tombstones.len(),
            1,
            "own-identity tombstone must be durable"
        );
        assert_eq!(tombstones[0].committed_user_msg_id, 910);
        assert_eq!(tombstones[0].committed_started_at, OWN_STARTED_AT);
        assert_eq!(tombstones[0].tmux_session_name, "tmux-100");
    }

    /// Scope pins: an Abort-kind marker on the SAME anchor (foreign pin — this
    /// commit proves nothing about the foreign turn, #3296) and an own-pin
    /// marker for a DIFFERENT anchor must both survive untouched, with no
    /// tombstone fabricated for either.
    #[test]
    fn lease_gated_completion_leaves_abort_and_other_anchor_markers() {
        let _root = test_root();
        record(&marker("claude", 100, 911, 10_000)).unwrap(); // Abort, same anchor
        record(&deferred_marker("claude", 100, 912, 10_000)).unwrap(); // other anchor

        let resolved =
            resolve_own_claim_markers_for_visibly_completed_anchor("claude", "tmux-100", 100, 911);

        assert_eq!(resolved, 0);
        assert_eq!(
            load_for_channel("claude", 100).len(),
            2,
            "foreign-pin and other-anchor markers are out of the resolver's scope"
        );
        assert!(
            load_commit_tombstones("claude", 100).is_empty(),
            "no tombstone may be fabricated for a turn this ✅ does not prove"
        );
    }

    /// Claim contention (write-before-discard): with the marker claimed by a
    /// concurrent reconciler the resolver must SKIP the discard but still
    /// persist the tombstone — the later sweep then 대조s ✅ (Complete), never
    /// the hard-cap ⚠ it would otherwise deliver.
    #[test]
    fn claim_contended_lease_gated_resolution_converges_to_sweep_completion() {
        let _root = test_root();
        let m = deferred_marker("claude", 100, 913, 10_000);
        record(&m).unwrap();
        let held_claim = try_claim_marker(&m).expect("test holds the claim");

        let resolved =
            resolve_own_claim_markers_for_visibly_completed_anchor("claude", "tmux-100", 100, 913);
        assert_eq!(resolved, 0, "claimed marker must not be discarded");
        assert_eq!(load_for_channel("claude", 100).len(), 1);
        assert_eq!(
            load_commit_tombstones("claude", 100).len(),
            1,
            "tombstone must land even when the discard loses the claim race"
        );

        drop(held_claim);
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let swept = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS * ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER + 1,
            true,
            &|_marker: &AbortedAnchorMarker| false,
            &applier,
        ));
        assert_eq!(swept, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(913, ReactionOp::Complete)],
            "the durable tombstone keeps the contended marker's verdict ✅ — \
             without it this past-hard-cap sweep delivers the false ⚠"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// #3350 codex r1-2 (tombstone-BEFORE-deliver): the watcher now runs the
    /// resolver before awaiting the visible `⏳ → ✅` delivery. Race legs:
    ///
    /// * OLD order (RED contrast) — the sweep preempts while the watcher is
    ///   still inside the delivery await, i.e. BEFORE any tombstone landed:
    ///   the row-absent, TTL-expired, uncovered marker degrades to the `⚠`
    ///   that then stacks next to the just-delivered `✅`.
    /// * NEW order — the resolver (tombstone + claimed discard) runs first;
    ///   the same sweep finds nothing to claim and delivers NOTHING.
    #[test]
    fn sweep_preempting_delivery_await_warns_only_under_the_old_order() {
        let _root = test_root();
        // OLD order: no tombstone yet when the sweep fires mid-await.
        record(&deferred_marker("claude", 100, 914, 10_000)).unwrap();
        let (warn_applier, warn_calls) = recording_applier(ReactionDelivery::Delivered);
        let swept = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS,
            true,
            &|_marker: &AbortedAnchorMarker| false,
            &warn_applier,
        ));
        assert_eq!(swept, 1);
        assert_eq!(
            warn_calls.lock().unwrap().as_slice(),
            &[(914, ReactionOp::FailureWarn)],
            "deliver-then-resolve let the mid-await sweep stack ⚠ next to the ✅"
        );

        // NEW order: resolver first (as the watcher call site now does), then
        // the identical sweep — no ⚠, no duplicate ✅, store empty.
        record(&deferred_marker("claude", 100, 915, 10_000)).unwrap();
        assert_eq!(
            resolve_own_claim_markers_for_visibly_completed_anchor("claude", "tmux-100", 100, 915),
            1
        );
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let swept = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS,
            true,
            &|_marker: &AbortedAnchorMarker| false,
            &applier,
        ));
        assert_eq!(swept, 0, "nothing left for the preempting sweep to claim");
        assert!(
            calls.lock().unwrap().is_empty(),
            "tombstone-first: the about-to-be-✅'d anchor sees no sweep reaction at all"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }
}
