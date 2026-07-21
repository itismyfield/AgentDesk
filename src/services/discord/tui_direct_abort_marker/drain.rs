//! Terminal-commit drain and visible-completion marker consumption paths.

use std::sync::Arc;

use super::{
    MarkerOrigin, ReactionApplierFn, ReactionDelivery, ReactionOp, SharedData,
    TerminalEvidenceOffsetProof, delete, load_for_channel, now_ms, record,
    record_commit_tombstone_with_offset, reload, shared_reaction_applier,
    terminal_commit_covers_marker_with_offsets, try_claim_marker,
};
/// Watcher terminal-commit chokepoint: a body-visible normal commit covers
/// every marker whose recorded foreign identity matches the COMMITTED turn
/// (codex r1) → `⏳ → ✅`. Returns markers drained.
pub(in crate::services::discord) async fn drain_on_terminal_commit(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
) -> usize {
    drain_on_terminal_commit_with_offset(
        shared,
        provider,
        tmux_session_name,
        channel_id,
        committed_user_msg_id,
        committed_started_at,
        None,
    )
    .await
}

pub(in crate::services::discord) async fn drain_on_terminal_commit_with_offset(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
) -> usize {
    let applier = shared_reaction_applier(shared.clone());
    drain_on_terminal_commit_with_applier_and_offset(
        provider,
        tmux_session_name,
        channel_id,
        now_ms(),
        committed_user_msg_id,
        committed_started_at,
        committed_turn_start_offset,
        &applier,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn drain_on_terminal_commit_with_offsets(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
    committed_terminal_evidence_offset: Option<u64>,
) -> usize {
    let applier = shared_reaction_applier(shared.clone());
    drain_on_terminal_commit_with_applier_and_offsets(
        provider,
        tmux_session_name,
        channel_id,
        now_ms(),
        committed_user_msg_id,
        committed_started_at,
        committed_turn_start_offset,
        TerminalEvidenceOffsetProof::Recorded(committed_terminal_evidence_offset),
        &applier,
    )
    .await
}

pub(in crate::services::discord) async fn drain_on_terminal_commit_with_applier(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    now_ms: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    applier: &ReactionApplierFn,
) -> usize {
    drain_on_terminal_commit_with_applier_and_offset(
        provider,
        tmux_session_name,
        channel_id,
        now_ms,
        committed_user_msg_id,
        committed_started_at,
        None,
        applier,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn drain_on_terminal_commit_with_applier_and_offset(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    now_ms: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
    applier: &ReactionApplierFn,
) -> usize {
    drain_on_terminal_commit_with_applier_and_offsets(
        provider,
        tmux_session_name,
        channel_id,
        now_ms,
        committed_user_msg_id,
        committed_started_at,
        committed_turn_start_offset,
        TerminalEvidenceOffsetProof::Legacy,
        applier,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drain_on_terminal_commit_with_applier_and_offsets(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    now_ms: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
    terminal_evidence_offset: TerminalEvidenceOffsetProof,
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
        if !terminal_commit_covers_marker_with_offsets(
            now_ms,
            &marker,
            committed_user_msg_id,
            committed_started_at,
            committed_turn_start_offset,
            terminal_evidence_offset,
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
            ReactionDelivery::Failed => {
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
pub(in crate::services::discord) fn resolve_own_claim_markers_for_visibly_completed_anchor(
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
        record_commit_tombstone_with_offset(
            provider,
            tmux_session_name,
            channel_id,
            anchor_message_id,
            &own_started_at,
            marker.foreign_turn_start_offset,
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
