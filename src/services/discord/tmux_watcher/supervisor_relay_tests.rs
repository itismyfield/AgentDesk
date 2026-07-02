//! #3479 Phase-1 rank-1: tests for the supervisor relay-FORWARD half. PURE MOVE
//! from `tmux_watcher.rs`'s `#[cfg(test)] mod tests` (zero logic change). Kept in
//! a sibling `*_tests.rs` so the production module stays within the
//! `src/services/discord/tmux_watcher/**` namespace LoC cap (test files are
//! excluded from the cap by the audit's `production_rust_files()` filter).

use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
use crate::services::cluster::stream_relay::{
    RelayDroppedFrame, RelaySink, RelaySinkError, RelaySinkOutcome, RelayTurnIdentity, StreamFrame,
    spawn_stream_relay_with_buffer,
};
use crate::services::discord::inflight::InflightTurnIdentity;
use crate::services::provider::ProviderKind;
use async_trait::async_trait;

#[test]
fn terminal_event_consumed_offset_excludes_buffered_tail() {
    assert_eq!(terminal_event_consumed_offset(128, "next-turn\n"), 118);
    assert_eq!(terminal_event_consumed_offset(8, "longer-than-offset"), 0);
}

struct BlockingSink {
    first_started: tokio::sync::Notify,
    unblock: tokio::sync::Notify,
    block_first: AtomicBool,
}

#[async_trait]
impl RelaySink for BlockingSink {
    async fn deliver(&self, _frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        if self.block_first.swap(false, Ordering::AcqRel) {
            self.first_started.notify_one();
            self.unblock.notified().await;
        }
        Ok(RelaySinkOutcome::FrameAccepted)
    }
}

fn matched(channel: &str) -> MatchedChannel {
    let session = ProviderKind::Claude.build_tmux_session_name(channel);
    MatchedChannel {
        channel_id: channel.to_string(),
        agent_id: format!("agent-{channel}"),
        provider: ProviderKind::Claude,
        expected_session_name: session.clone(),
        expected_rollout_path: expected_rollout_path_for(&session),
    }
}

fn turn_identity(
    session: &str,
    user_msg_id: u64,
    started_at: &str,
    turn_start_offset: u64,
) -> InflightTurnIdentity {
    InflightTurnIdentity {
        user_msg_id,
        started_at: started_at.to_string(),
        tmux_session_name: Some(session.to_string()),
        turn_start_offset: Some(turn_start_offset),
    }
}

#[tokio::test]
async fn forward_chunk_surfaces_backpressure_evicted_victim_identity() {
    let matched = matched("c-supervisor-evict");
    let session = matched.expected_session_name.clone();
    let registry =
        Arc::new(crate::services::cluster::relay_producer_registry::RelayProducerRegistry::new());
    let sink = Arc::new(BlockingSink {
        first_started: tokio::sync::Notify::new(),
        unblock: tokio::sync::Notify::new(),
        block_first: AtomicBool::new(true),
    });
    let handle = spawn_stream_relay_with_buffer(matched.clone(), sink.clone(), 1);
    registry.register(session.clone(), handle.producer());
    let mut cached = None;
    let victim = turn_identity(&session, 77, "2026-06-04T00:00:00Z", 64);
    let newest = turn_identity(&session, 88, "2026-06-04T00:00:01Z", 128);

    let blocked = forward_chunk_to_supervisor_relay_for_turn(
        &session,
        "blocked-in-sink",
        &registry,
        &mut cached,
        Some(&victim),
    );
    assert!(blocked.mirrored);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sink.first_started.notified(),
    )
    .await
    .expect("first frame reaches blocked sink");

    let queued_victim = forward_chunk_to_supervisor_relay_for_turn(
        &session,
        "queued-victim",
        &registry,
        &mut cached,
        Some(&victim),
    );
    let newest_forward = forward_chunk_to_supervisor_relay_for_turn(
        &session,
        "newest",
        &registry,
        &mut cached,
        Some(&newest),
    );

    assert!(queued_victim.evicted_frames.is_empty());
    assert!(
        newest_forward.mirrored,
        "send remains alive after drop-oldest"
    );
    let dropped = newest_forward
        .evicted_frames
        .first()
        .expect("overflow should surface the evicted victim frame");
    assert_eq!(dropped.sequence, queued_victim.ack_target.unwrap().sequence);
    assert_eq!(dropped.turn_identity.turn_user_msg_id, 77);
    assert_eq!(
        dropped.turn_identity.turn_started_at.as_str(),
        victim.started_at.as_str()
    );
    assert_eq!(dropped.turn_identity.turn_start_offset, Some(64));
    assert!(
        supervisor_relay_forward_fully_mirrors_turn(&newest_forward, Some(&newest)),
        "a known different victim must not degrade the new turn"
    );
    assert!(
        !supervisor_relay_forward_fully_mirrors_turn(&newest_forward, Some(&victim)),
        "the evicted victim turn must no longer count as fully mirrored"
    );

    registry.deregister(&session);
    sink.unblock.notify_waiters();
    handle.shutdown().await;
}

#[test]
fn per_turn_fully_mirrored_degrades_when_evicted_victim_matches() {
    let session = ProviderKind::Claude.build_tmux_session_name("c-supervisor-degrade");
    let current = turn_identity(&session, 77, "2026-06-04T00:00:00Z", 64);
    let forward = SupervisorRelayForward {
        mirrored: true,
        ack_target: None,
        evicted_frames: vec![RelayDroppedFrame {
            sequence: 9,
            turn_identity: RelayTurnIdentity {
                turn_user_msg_id: 77,
                turn_started_at: "2026-06-04T00:00:00Z".to_string(),
                turn_start_offset: Some(64),
            },
        }],
        trailing_turn_follows: false,
    };

    let mut session_bound_relay_turn_fully_mirrored = true;
    session_bound_relay_turn_fully_mirrored &=
        supervisor_relay_forward_fully_mirrors_turn(&forward, Some(&current));
    assert!(
        !session_bound_relay_turn_fully_mirrored,
        "per-turn fully_mirrored must degrade when this turn lost a queued frame"
    );

    let different_turn = turn_identity(&session, 77, "2026-06-04T00:00:00Z", 128);
    assert!(
        supervisor_relay_forward_fully_mirrors_turn(&forward, Some(&different_turn)),
        "strict turn_start_offset keeps a different same-user same-start turn unaffected"
    );
}
