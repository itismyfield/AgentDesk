//! TTL sweep reconciler for durable TUI-direct abort markers.

use std::sync::Arc;

use super::super::ProviderKind;
use super::deferred_claim;
use super::deferred_claim::LiveInflightProbe;
use super::{
    ABORT_MARKER_TTL, AbortedAnchorMarker, MarkerDisposition, MarkerOrigin, ReactionApplierFn,
    ReactionDelivery, ReactionOp, SharedData, decide_marker_disposition,
    deferred_claim_live_inflight_is_pinned, delete, gc_expired_commit_tombstones,
    inflight_defers_sweep, load_all, now_ms, post_abort_commit_tombstone, record, reload,
    shared_reaction_applier, try_claim_marker,
};
/// Placeholder-sweeper pass: retry `✅` for covered markers; apply the TTL'd
/// `⏳ → ⚠` fallback for anchors no commit ever covered (held while a live
/// inflight may still cover them, up to the hard cap). Returns resolved.
pub(in crate::services::discord) async fn sweep_expired(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> usize {
    let http_available = shared.serenity_http_or_token_fallback().is_some();
    let applier = shared_reaction_applier(shared.clone());
    // ONE inflight read feeds both kind predicates (#3303): `defers` is the
    // Abort-kind hold; `is_pinned_turn` is the DeferredClaim-kind hold (the
    // live row IS the marker's pinned own turn — user_msg_id / started_at /
    // tmux session name all match).
    let live_inflight = |marker: &AbortedAnchorMarker| -> LiveInflightProbe {
        match super::super::inflight::load_inflight_state(provider, marker.channel_id) {
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
                is_pinned_turn: deferred_claim_live_inflight_is_pinned(
                    marker,
                    state.tmux_session_name.as_deref(),
                    state.user_msg_id,
                    &state.started_at,
                    state.turn_start_offset,
                ),
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
pub(in crate::services::discord) async fn sweep_expired_with_applier<P: Into<LiveInflightProbe>>(
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
            ReactionDelivery::Failed => {}
        }
    }
    // GC AFTER the marker loop: the first pass after long downtime must 대조
    // against evidence that aged past the retention cap before deleting it.
    gc_expired_commit_tombstones(now_ms);
    resolved
}
