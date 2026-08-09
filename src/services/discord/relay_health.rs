//! Derived relay-health model and side-effect-free stall classification.
//!
//! The runtime remains the source of truth. This module only describes a
//! point-in-time, read-only view that health endpoints and future recovery
//! paths can share.

use serde::Serialize;

mod frontier;
pub(in crate::services::discord) use frontier::{
    FrontierResetState, RelayFrontierMutationGuard, RelayFrontierToken,
};

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
    UnpairedActiveToken,
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
            Self::UnpairedActiveToken => "unpaired_active_token",
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
    /// #3277 (Defect D): the attached watcher handle's heartbeat is stale.
    /// Cancel flags are handled by watcher replacement paths and are not folded
    /// into this heartbeat label. `false` whenever `watcher_attached` is false.
    pub watcher_attached_stale: bool,
    pub watcher_owner_channel_id: Option<u64>,
    pub watcher_owns_live_relay: bool,
    pub bridge_inflight_present: bool,
    pub bridge_current_msg_id: Option<u64>,
    pub mailbox_has_cancel_token: bool,
    pub mailbox_active_user_msg_id: Option<u64>,
    pub mailbox_turn_started_at_ms: Option<i64>,
    pub mailbox_turn_age_secs: Option<u64>,
    pub queue_depth: usize,
    pub pending_discord_callback_msg_id: Option<u64>,
    pub pending_thread_proof: bool,
    pub parent_channel_id: Option<u64>,
    pub thread_channel_id: Option<u64>,
    pub last_relay_ts_ms: Option<i64>,
    pub last_relay_age_secs: Option<u64>,
    pub last_outbound_activity_ms: Option<i64>,
    pub last_capture_offset: Option<u64>,
    pub last_relay_offset: u64,
    pub unread_bytes: Option<u64>,
    pub desynced: bool,
    pub stale_thread_proof: bool,
    /// Internal proof that a second mailbox snapshot and inflight read still
    /// saw the same active episode without a durable row.
    #[serde(skip)]
    pub unpaired_active_token_reconfirmed: bool,
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
            watcher_attached_stale: false,
            watcher_owner_channel_id: None,
            watcher_owns_live_relay: false,
            bridge_inflight_present: false,
            bridge_current_msg_id: None,
            mailbox_has_cancel_token: false,
            mailbox_active_user_msg_id: None,
            mailbox_turn_started_at_ms: None,
            mailbox_turn_age_secs: None,
            queue_depth: 0,
            pending_discord_callback_msg_id: None,
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_relay_age_secs: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced: false,
            stale_thread_proof: false,
            unpaired_active_token_reconfirmed: false,
        }
    }

    fn has_live_relay_evidence(&self) -> bool {
        self.active_turn.is_active()
            || self.tmux_alive == Some(true)
            || self.watcher_attached
            || self.bridge_inflight_present
    }

    /// True for the restart/desync signature where a watcher handle still looks
    /// live and may even own the tmux session, but the relay frontier never
    /// advanced while the transcript/capture accumulated bytes.
    pub(in crate::services::discord) fn relay_frontier_never_advanced_with_unread_tail(
        &self,
    ) -> bool {
        self.desynced
            && self.tmux_alive == Some(true)
            && self.last_relay_ts_ms.is_none()
            && self.last_relay_offset == 0
            && self
                .last_capture_offset
                .is_some_and(|capture| capture > self.last_relay_offset)
            && self.unread_bytes.is_some_and(|bytes| bytes > 0)
    }
}

/// Time allowed for a newly minted mailbox token to acquire its durable
/// inflight row before an absent pairing becomes observable as a stall.
/// Its initial value happens to equal the stall-watchdog threshold, but the
/// two policies have different meanings and no reason to move together.
pub(in crate::services::discord) const UNPAIRED_ACTIVE_TOKEN_GRACE_SECS: u64 = 600;

pub(in crate::services::discord) fn observation_age_secs(
    observed_at_ms: i64,
    event_at_ms: Option<i64>,
) -> Option<u64> {
    let elapsed_ms = observed_at_ms.checked_sub(event_at_ms?)?;
    (elapsed_ms >= 0).then_some(elapsed_ms as u64 / 1_000)
}

pub(in crate::services::discord) struct RelayStallClassifier;

impl RelayStallClassifier {
    pub(in crate::services::discord) fn classify(
        snapshot: &RelayHealthSnapshot,
    ) -> RelayStallState {
        let live_watcher_owns_relay = snapshot.watcher_attached
            && !snapshot.watcher_attached_stale
            && snapshot.watcher_owns_live_relay;
        if snapshot.tmux_alive == Some(true)
            && snapshot.desynced
            && (!live_watcher_owns_relay
                || snapshot.relay_frontier_never_advanced_with_unread_tail())
        {
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

        if snapshot.mailbox_has_cancel_token
            && !snapshot.bridge_inflight_present
            && snapshot.unpaired_active_token_reconfirmed
            && snapshot
                .mailbox_turn_age_secs
                .is_some_and(|age| age >= UNPAIRED_ACTIVE_TOKEN_GRACE_SECS)
        {
            return RelayStallState::UnpairedActiveToken;
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
                "live owned watcher with a dead relay frontier is classified relay-dead",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    bridge_inflight_present: true,
                    mailbox_has_cancel_token: true,
                    tmux_alive: Some(true),
                    watcher_attached: true,
                    watcher_owns_live_relay: true,
                    last_capture_offset: Some(128),
                    last_relay_offset: 0,
                    unread_bytes: Some(128),
                    desynced: true,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::TmuxAliveRelayDead,
            ),
            (
                "live owned watcher with relay progress remains an active stream",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    bridge_inflight_present: true,
                    mailbox_has_cancel_token: true,
                    tmux_alive: Some(true),
                    watcher_attached: true,
                    watcher_owns_live_relay: true,
                    last_relay_ts_ms: Some(1_777_001_234_000),
                    last_capture_offset: Some(256),
                    last_relay_offset: 128,
                    unread_bytes: Some(128),
                    desynced: true,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::ActiveForegroundStream,
            ),
            (
                "live tmux plus ownerless desync is relay-dead even during a foreground turn",
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
                    mailbox_turn_started_at_ms: None,
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
            (
                "young rowless active token remains foreground before grace",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    tmux_alive: Some(true),
                    watcher_attached: true,
                    mailbox_has_cancel_token: true,
                    mailbox_active_user_msg_id: Some(9001),
                    mailbox_turn_started_at_ms: Some(1_000_000),
                    mailbox_turn_age_secs: Some(599),
                    unpaired_active_token_reconfirmed: true,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::ActiveForegroundStream,
            ),
            (
                "old rowless active token with null relay coordinates is unpaired",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    tmux_alive: Some(true),
                    watcher_attached: true,
                    mailbox_has_cancel_token: true,
                    mailbox_active_user_msg_id: Some(9001),
                    mailbox_turn_started_at_ms: Some(1_000_000),
                    mailbox_turn_age_secs: Some(601),
                    unpaired_active_token_reconfirmed: true,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::UnpairedActiveToken,
            ),
            (
                "channel relay telemetry does not exempt an old rowless active token",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    tmux_alive: Some(true),
                    watcher_attached: true,
                    mailbox_has_cancel_token: true,
                    mailbox_turn_started_at_ms: Some(1_000_000),
                    mailbox_turn_age_secs: Some(1_200),
                    last_relay_ts_ms: Some(1_600_000),
                    last_relay_age_secs: Some(1),
                    last_relay_offset: 0,
                    unpaired_active_token_reconfirmed: true,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::UnpairedActiveToken,
            ),
            (
                "unreconfirmed mixed-epoch candidate stays active",
                RelayHealthSnapshot {
                    active_turn: RelayActiveTurn::Foreground,
                    tmux_alive: Some(true),
                    watcher_attached: true,
                    mailbox_has_cancel_token: true,
                    mailbox_turn_started_at_ms: Some(1_000_000),
                    mailbox_turn_age_secs: Some(1_200),
                    unpaired_active_token_reconfirmed: false,
                    ..RelayHealthSnapshot::test_snapshot()
                },
                RelayStallState::ActiveForegroundStream,
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

    #[test]
    fn observation_age_rejects_future_and_overflowing_timestamps() {
        assert_eq!(observation_age_secs(10_000, Some(9_001)), Some(0));
        assert_eq!(observation_age_secs(10_000, Some(11_000)), None);
        assert_eq!(observation_age_secs(i64::MAX, Some(i64::MIN)), None);
        assert_eq!(observation_age_secs(10_000, None), None);
    }

    #[test]
    fn serialized_snapshot_exposes_ages_but_not_internal_recheck_proof() {
        let value = serde_json::to_value(RelayHealthSnapshot::test_snapshot()).unwrap();

        assert!(value.get("mailbox_turn_age_secs").is_some());
        assert!(value.get("last_relay_age_secs").is_some());
        assert!(value.get("unpaired_active_token_reconfirmed").is_none());
    }
}
