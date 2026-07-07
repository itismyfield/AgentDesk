//! Commit-tombstone recording and marker-cover entry points for TUI-direct abort markers.

use super::store::{load_commit_tombstones, record};
use super::{AbortedAnchorMarker, commit_tombstone_matches_marker, now_ms};
/// Record-instant 대조 (codex r2 — replaces the unfounded "pre-covered"
/// promotion): a tombstone satisfying the marker's commit-evidence gate
/// ALREADY durable at record time means that turn terminal-committed before
/// this marker existed (its drain pass could never see it) → stamp covered
/// with the commit instant. The id-0-for-real-marker carve-out is the only
/// exception: because id-0 is not an identity, that tombstone must be recorded
/// at-or-after this marker was created.
pub(in crate::services::discord) fn cover_from_commit_tombstone(
    marker: &mut AbortedAnchorMarker,
) -> bool {
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

/// Sweep-side 대조: a tombstone covers an UNCOVERED marker when it satisfies
/// the marker's commit-evidence gate AND its commit is not EARLIER than the
/// abort. `>=`, not `>` (codex r3): identity/start evidence is PRIMARY — the
/// ABORT fired because the recorded foreign turn stayed live through the whole
/// 32s backstop, so a commit of THAT turn cannot predate the input submission
/// (itself before the marker existed); an evidence-matched same-ms commit is
/// therefore necessarily post-submission, and strict `>` only ever rejected
/// ANSWERED anchors at the ms boundary (false `⚠`). The record-instant 대조
/// above owns evidence predating the marker; this subsidiary guard mirrors the
/// drain's.
pub(in crate::services::discord) fn post_abort_commit_tombstone(
    marker: &AbortedAnchorMarker,
) -> Option<u64> {
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
pub(in crate::services::discord) fn record_for_abort(
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
