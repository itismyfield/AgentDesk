//! Derived relay-health model and side-effect-free stall classification.
//!
//! The runtime remains the source of truth. This module only describes a
//! point-in-time, read-only view that health endpoints and future recovery
//! paths can share.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayActiveTurn {
    None,
    Foreground,
    ExplicitBackground,
}

impl RelayActiveTurn {
    fn is_active(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayStallState {
    Healthy,
    ActiveForegroundStream,
    ExplicitBackgroundWork,
    TmuxAliveRelayDead,
    StaleThreadProof,
    OrphanPendingToken,
    QueueBlocked,
}

impl RelayStallState {
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::ActiveForegroundStream => "active_foreground_stream",
            Self::ExplicitBackgroundWork => "explicit_background_work",
            Self::TmuxAliveRelayDead => "tmux_alive_relay_dead",
            Self::StaleThreadProof => "stale_thread_proof",
            Self::OrphanPendingToken => "orphan_pending_token",
            Self::QueueBlocked => "queue_blocked",
        }
    }

    pub(in crate::services::discord) fn should_log_at_debug(self) -> bool {
        !matches!(
            self,
            Self::Healthy | Self::ActiveForegroundStream | Self::ExplicitBackgroundWork
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayHealthSnapshot {
    pub provider: String,
    pub channel_id: u64,
    pub active_turn: RelayActiveTurn,
    pub tmux_session: Option<String>,
    pub tmux_alive: Option<bool>,
    pub watcher_attached: bool,
    pub watcher_owner_channel_id: Option<u64>,
    pub watcher_owns_live_relay: bool,
    pub bridge_inflight_present: bool,
    pub bridge_current_msg_id: Option<u64>,
    pub mailbox_has_cancel_token: bool,
    pub mailbox_active_user_msg_id: Option<u64>,
    pub queue_depth: usize,
    pub pending_discord_callback_msg_id: Option<u64>,
    pub pending_thread_proof: bool,
    pub parent_channel_id: Option<u64>,
    pub thread_channel_id: Option<u64>,
    pub last_relay_ts_ms: Option<i64>,
    pub last_outbound_activity_ms: Option<i64>,
    pub last_capture_offset: Option<u64>,
    pub last_relay_offset: u64,
    pub unread_bytes: Option<u64>,
    pub desynced: bool,
    pub stale_thread_proof: bool,
}

impl RelayHealthSnapshot {
    #[cfg(test)]
    fn test_snapshot() -> Self {
        Self {
            provider: "codex".to_string(),
            channel_id: 42,
            active_turn: RelayActiveTurn::None,
            tmux_session: None,
            tmux_alive: None,
            watcher_attached: false,
            watcher_owner_channel_id: None,
            watcher_owns_live_relay: false,
            bridge_inflight_present: false,
            bridge_current_msg_id: None,
            mailbox_has_cancel_token: false,
            mailbox_active_user_msg_id: None,
            queue_depth: 0,
            pending_discord_callback_msg_id: None,
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced: false,
            stale_thread_proof: false,
        }
    }

    fn has_live_relay_evidence(&self) -> bool {
        self.active_turn.is_active()
            || self.tmux_alive == Some(true)
            || self.watcher_attached
            || self.bridge_inflight_present
    }
}

pub(in crate::services::discord) struct RelayStallClassifier;

impl RelayStallClassifier {
    pub(in crate::services::discord) fn classify(
        snapshot: &RelayHealthSnapshot,
    ) -> RelayStallState {
        if snapshot.tmux_alive == Some(true) && snapshot.desynced {
            return RelayStallState::TmuxAliveRelayDead;
        }

        if snapshot.stale_thread_proof {
            return RelayStallState::StaleThreadProof;
        }

        if snapshot.mailbox_has_cancel_token
            && !snapshot.bridge_inflight_present
            && !snapshot.watcher_attached
            && snapshot.tmux_alive != Some(true)
        {
            return RelayStallState::OrphanPendingToken;
        }

        if snapshot.queue_depth > 0 && !snapshot.has_live_relay_evidence() {
            return RelayStallState::QueueBlocked;
        }

        match snapshot.active_turn {
            RelayActiveTurn::ExplicitBackground => RelayStallState::ExplicitBackgroundWork,
            RelayActiveTurn::Foreground => RelayStallState::ActiveForegroundStream,
            RelayActiveTurn::None if snapshot.queue_depth > 0 => RelayStallState::QueueBlocked,
            RelayActiveTurn::None => RelayStallState::Healthy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_stall_classifier_is_table_driven() {
        let cases: Vec<(&str, RelayHealthSnapshot, RelayStallState)> = vec![
            (
                "idle with no relay evidence is healthy",
                RelayHealthSnapshot::test_snapshot(),
                RelayStallState::Healthy,
            ),
            (
                "foreground stream remains distinct from background work",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    bridge_inflight_present: true,
                    mailbox_has_cancel_token: true,
                    pending_discord_callback_msg_id: Some(9002),
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::ActiveForegroundStream,
            ),
            (
                "explicit background work is not folded into foreground",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::ExplicitBackground,
                    bridge_inflight_present: true,
                    mailbox_has_cancel_token: true,
                    pending_discord_callback_msg_id: Some(9002),
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::ExplicitBackgroundWork,
            ),
            (
                "live tmux plus desync is relay-dead even during a foreground turn",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    bridge_inflight_present: true,
                    mailbox_has_cancel_token: true,
                    tmux_alive: Some(true),
                    desynced: true,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::TmuxAliveRelayDead,
            ),
            (
                "stale thread proof takes precedence over a queued backlog",
                RelayHealthSnapshot {
                    queue_depth: 3,
                    pending_thread_proof: true,
                    stale_thread_proof: true,
                    thread_channel_id: Some(1001),
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::StaleThreadProof,
            ),
            (
                "mailbox cancel token without bridge or watcher evidence is orphaned",
                RelayHealthSnapshot {
                    mailbox_has_cancel_token: true,
                    mailbox_active_user_msg_id: Some(9001),
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::OrphanPendingToken,
            ),
            (
                "queued work with no live relay evidence is blocked",
                RelayHealthSnapshot {
                    queue_depth: 2,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::QueueBlocked,
            ),
        ];

        for (name, snapshot, expected) in cases {
            assert_eq!(
                RelayStallClassifier::classify(&snapshot),
                expected,
                "{name}"
            );
        }
    }
}
