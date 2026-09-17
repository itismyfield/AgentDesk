//! Admission of one producer frame into the relay queue: stamp the caller's
//! request with the relay's monotonic `sequence` plus the routing snapshot,
//! then push it into the bounded queue (dropping the oldest frame when full).
//!
//! Split out of `stream_relay.rs` so the request envelope and the single place
//! that turns it into a [`StreamFrame`] read together.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::super::session_matcher::MatchedChannel;
use super::{
    QueuePushResult, RelayFrameQueue, RelayMetrics, RelaySendOutcome, RelayTurnIdentity,
    SourceStamp, StreamFrame, TerminalCommitFence, encode_sequence_marker,
};

/// Owned input envelope before the relay assigns its queue sequence and routing snapshot.
#[derive(Clone, Debug)]
pub struct RelayFrameRequest {
    pub payload: String,
    pub terminal: Option<TerminalCommitFence>,
    pub identity: Option<RelayTurnIdentity>,
    pub range: Option<(u64, u64)>,
    pub generation: Option<i64>,
    pub stamp: Option<SourceStamp>,
    /// #5948 (I17): see [`StreamFrame::source_span`].
    pub span: Option<(u64, u64)>,
}

pub(super) fn try_send_frame_inner(
    matched: &MatchedChannel,
    queue: &Arc<RelayFrameQueue>,
    shutdown: &Arc<AtomicBool>,
    metrics: &Arc<RelayMetrics>,
    sequence: &Arc<AtomicU64>,
    request: RelayFrameRequest,
) -> RelaySendOutcome {
    let RelayFrameRequest {
        payload,
        terminal,
        identity: frame_identity,
        range: relay_range,
        generation: relay_generation_mtime_ns,
        stamp: relay_source_stamp,
        span: source_span,
    } = request;
    if shutdown.load(Ordering::Acquire) {
        return RelaySendOutcome::closed();
    }
    let seq = sequence.fetch_add(1, Ordering::AcqRel);
    let (terminal_consumed_end, frame_identity) = match terminal {
        Some(fence) => (
            Some(fence.consumed_end),
            RelayTurnIdentity {
                turn_user_msg_id: fence.turn_user_msg_id,
                turn_started_at: fence.turn_started_at,
                turn_start_offset: fence.turn_start_offset,
            },
        ),
        None => (None, frame_identity.unwrap_or_default()),
    };
    let frame = StreamFrame {
        session_name: matched.expected_session_name.clone(),
        binding: matched.clone(),
        payload,
        sequence: seq,
        terminal_consumed_end,
        turn_user_msg_id: frame_identity.turn_user_msg_id,
        turn_started_at: frame_identity.turn_started_at,
        turn_start_offset: frame_identity.turn_start_offset,
        relay_range,
        relay_generation_mtime_ns,
        relay_source_stamp,
        source_span,
    };
    metrics.frames_received.fetch_add(1, Ordering::AcqRel);
    match queue.push_drop_oldest(frame) {
        QueuePushResult::Enqueued => RelaySendOutcome::enqueued(seq, None),
        QueuePushResult::DroppedOldest(dropped) => {
            metrics.dropped_frames.fetch_add(1, Ordering::AcqRel);
            metrics
                .last_dropped_sequence_plus_one
                .fetch_max(encode_sequence_marker(dropped.sequence), Ordering::AcqRel);
            RelaySendOutcome::enqueued(seq, Some(dropped))
        }
        QueuePushResult::Closed => RelaySendOutcome::closed(),
    }
}
