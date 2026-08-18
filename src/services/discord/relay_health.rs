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

// ---------------------------------------------------------------------------
// #5071 relay-tail S1 (I-4): frontier provenance
// ---------------------------------------------------------------------------

/// What the in-memory relay-coordinate map said about this channel's frontier.
///
/// `Absent` is NOT `Advanced { offset: 0 }`. The map holding no entry and an
/// entry holding a zero are different observations, and
/// `health::session_enrichment::load` flattens both into the same `0` through
/// its `unwrap_or((0, 0, 0))` — which is why E2's frontier reads cannot be
/// attributed to either. This records which of the two happened.
///
/// Recording only: every value derived from the coordinate keeps its current
/// source and polarity. Making an unsourced frontier *unknown* is S2's change,
/// not this one's.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(in crate::services::discord) enum CoordFrontierObservation {
    /// The map holds an entry whose `confirmed_end_offset` is past zero.
    Advanced { offset: u64 },
    /// The map holds an entry that has never committed a byte.
    PresentZero,
    /// The map holds no entry for this channel at all.
    Absent,
}

impl CoordFrontierObservation {
    /// `None` means the lookup missed; `Some` carries that entry's
    /// `confirmed_end_offset`.
    pub(in crate::services::discord) fn observe(confirmed_end_offset: Option<u64>) -> Self {
        match confirmed_end_offset {
            None => Self::Absent,
            Some(0) => Self::PresentZero,
            Some(offset) => Self::Advanced { offset },
        }
    }

    /// Whether two readings are the same KIND of observation, ignoring how far
    /// each advanced. Two channels legitimately relay different byte counts, so
    /// an offset difference is not an axis split; `Absent` opposite anything
    /// present is.
    fn same_kind(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

/// What the durable in-flight row said about the same frontier.
///
/// Independent of [`CoordFrontierObservation`] on purpose (r1 review P1-4): the
/// two witnesses fail separately, and one label for the pair would have to
/// elect one of them to speak for the other — the exact confusion that made H1
/// and H2 indistinguishable in the E2 traces.
///
/// `relayed_start` is the turn's START offset (what
/// `tmux_watcher::commit_decisions` persists), not a delivered end, and it
/// arrives with the `.generation` mtime it was snapshotted against because an
/// offset is only attributable to the incarnation that produced it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(in crate::services::discord) enum DurableFrontierObservation {
    /// No durable row records a relayed offset for this channel — either there
    /// is no row, or the row carries no `last_watcher_relayed_offset`.
    RowAbsent,
    /// A row records one and nothing on hand contradicts its generation. That
    /// includes "no live generation to compare against": an unwitnessed
    /// generation is not a mismatch.
    RowPresent {
        relayed_start: u64,
        generation_ns: Option<i64>,
    },
    /// A row records one, and the live coordinate's generation says it belongs
    /// to an earlier incarnation.
    GenerationMismatch {
        relayed_start: u64,
        row_generation_ns: i64,
        live_generation_ns: i64,
    },
}

impl DurableFrontierObservation {
    /// `live_generation_ns` is the generation the LIVE coordinate snapshotted
    /// when it last advanced (`TmuxRelayCoord::confirmed_end_generation_mtime_ns`).
    /// Callers pass `None` for its "never observed" zero, so a coordinate that
    /// never advanced cannot manufacture a mismatch against a row that did.
    pub(in crate::services::discord) fn observe(
        relayed_start: Option<u64>,
        row_generation_ns: Option<i64>,
        live_generation_ns: Option<i64>,
    ) -> Self {
        let Some(relayed_start) = relayed_start else {
            return Self::RowAbsent;
        };
        match (row_generation_ns, live_generation_ns) {
            (Some(row), Some(live)) if row != live => Self::GenerationMismatch {
                relayed_start,
                row_generation_ns: row,
                live_generation_ns: live,
            },
            _ => Self::RowPresent {
                relayed_start,
                generation_ns: row_generation_ns,
            },
        }
    }
}

/// The two witnesses, side by side and neither derived from the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct FrontierProvenance {
    pub coord_observation: CoordFrontierObservation,
    pub durable_observation: DurableFrontierObservation,
}

/// Which E2 hypothesis the pair of witnesses is consistent with (design §2.3).
///
/// A derived READ of the two fields, never a replacement for them: the mapping
/// is lossy in one direction only, so the record stays the pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum FrontierHypothesis {
    /// H1 — `Absent × RowPresent`. The coordinate entry is gone while the
    /// durable row still names a relayed offset: the frontier the poll reports
    /// is the `unwrap_or` zero, not a measurement.
    CoordEntryAbsentWithDurableRow,
    /// H2 — `PresentZero × RowPresent`. The entry exists and never advanced,
    /// while the row says a relay happened. `RowPresent` already excludes a
    /// generation disagreement, so this is the design's "gen 일치" arm.
    CoordNeverAdvancedWithDurableRow,
    /// H3 — the parent and thread ends of one axis disagree about the
    /// coordinate, i.e. the frontier is being read under the wrong `ChannelId`.
    ChannelAxisSplit,
    /// The witnesses name no row of the table.
    Indeterminate,
}

impl FrontierProvenance {
    pub(in crate::services::discord) fn observe(
        coord_observation: CoordFrontierObservation,
        durable_observation: DurableFrontierObservation,
    ) -> Self {
        Self {
            coord_observation,
            durable_observation,
        }
    }

    /// The discrimination table.
    ///
    /// `counterpart_coord` is the other end of the parent/thread axis when the
    /// caller resolved one. H3 is checked FIRST and is the only load-bearing
    /// ordering: if the two ends of the axis disagree, this channel's own pair
    /// is being read under a `ChannelId` that may not own the coordinate, so
    /// the H1/H2 rows below cannot be attributed. A single row can never spell
    /// H3, which is why the counterpart is a parameter rather than a field.
    pub(in crate::services::discord) fn hypothesis(
        self,
        counterpart_coord: Option<CoordFrontierObservation>,
    ) -> FrontierHypothesis {
        if counterpart_coord
            .is_some_and(|counterpart| !self.coord_observation.same_kind(counterpart))
        {
            return FrontierHypothesis::ChannelAxisSplit;
        }
        match (self.coord_observation, self.durable_observation) {
            (CoordFrontierObservation::Absent, DurableFrontierObservation::RowPresent { .. }) => {
                FrontierHypothesis::CoordEntryAbsentWithDurableRow
            }
            (
                CoordFrontierObservation::PresentZero,
                DurableFrontierObservation::RowPresent { .. },
            ) => FrontierHypothesis::CoordNeverAdvancedWithDurableRow,
            _ => FrontierHypothesis::Indeterminate,
        }
    }
}

/// The provenance as the health detail publishes it: the two independent
/// fields, flattened so they stay two fields on the wire, plus the derived
/// hypothesis. Serialization only — nothing reads this back.
#[derive(Debug, Serialize)]
pub(in crate::services::discord) struct FrontierProvenanceReport {
    #[serde(flatten)]
    provenance: FrontierProvenance,
    hypothesis: FrontierHypothesis,
}

impl FrontierProvenanceReport {
    pub(in crate::services::discord) fn of(
        provenance: FrontierProvenance,
        counterpart_coord: Option<CoordFrontierObservation>,
    ) -> Self {
        Self {
            provenance,
            hypothesis: provenance.hypothesis(counterpart_coord),
        }
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
    use poise::serenity_prelude::ChannelId;

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

    // -----------------------------------------------------------------------
    // #5071 relay-tail S1 (I-4): the frontier-provenance discrimination table
    // -----------------------------------------------------------------------

    const GENERATION_NS: i64 = 1_700_491_601_000_000_000;
    const PARENT_CHANNEL: u64 = 41;
    const THREAD_CHANNEL: u64 = 42;

    /// A durable row that named a relayed offset, with nothing contradicting
    /// its generation — the `RowPresent` both H1 and H2 are conditioned on.
    fn row_present() -> DurableFrontierObservation {
        DurableFrontierObservation::observe(Some(4_096), Some(GENERATION_NS), None)
    }

    /// A parent channel and the thread hung off it, each carrying its OWN
    /// coordinate reading. H3 is a claim about a PAIR of `ChannelId`s, so the
    /// fixture is two rows: one health poll observes one of them and can never
    /// produce this shape by itself (r2 review).
    fn parent_thread_rows(
        parent: CoordFrontierObservation,
        thread: CoordFrontierObservation,
    ) -> [(ChannelId, FrontierProvenance); 2] {
        [
            (
                ChannelId::new(PARENT_CHANNEL),
                FrontierProvenance::observe(parent, row_present()),
            ),
            (
                ChannelId::new(THREAD_CHANNEL),
                FrontierProvenance::observe(thread, row_present()),
            ),
        ]
    }

    /// The E2 witnesses stay two fields, and `Absent` stays distinguishable
    /// from a coordinate that is present at zero — the collapse
    /// `unwrap_or((0, 0, 0))` performs today.
    #[test]
    fn an_absent_coordinate_entry_is_not_a_zero_frontier() {
        assert_eq!(
            CoordFrontierObservation::observe(None),
            CoordFrontierObservation::Absent
        );
        assert_eq!(
            CoordFrontierObservation::observe(Some(0)),
            CoordFrontierObservation::PresentZero
        );
        assert_eq!(
            CoordFrontierObservation::observe(Some(7)),
            CoordFrontierObservation::Advanced { offset: 7 }
        );
        assert_ne!(
            CoordFrontierObservation::observe(None),
            CoordFrontierObservation::observe(Some(0)),
            "H1 and H2 differ only here; equating them is what hid E2"
        );
    }

    /// A mismatch is a disagreement between two WITNESSED generations. A
    /// coordinate that never advanced witnesses none, and must not be able to
    /// declare a durable row stale by saying nothing.
    #[test]
    fn a_durable_row_needs_two_witnessed_generations_to_be_a_mismatch() {
        assert_eq!(
            DurableFrontierObservation::observe(None, Some(GENERATION_NS), Some(GENERATION_NS + 1)),
            DurableFrontierObservation::RowAbsent,
            "no relayed offset is no row, whatever the generations say"
        );
        assert_eq!(
            DurableFrontierObservation::observe(Some(9), None, Some(GENERATION_NS)),
            DurableFrontierObservation::RowPresent {
                relayed_start: 9,
                generation_ns: None,
            }
        );
        assert_eq!(
            DurableFrontierObservation::observe(Some(9), Some(GENERATION_NS), None),
            DurableFrontierObservation::RowPresent {
                relayed_start: 9,
                generation_ns: Some(GENERATION_NS),
            }
        );
        assert_eq!(
            DurableFrontierObservation::observe(
                Some(9),
                Some(GENERATION_NS),
                Some(GENERATION_NS + 1)
            ),
            DurableFrontierObservation::GenerationMismatch {
                relayed_start: 9,
                row_generation_ns: GENERATION_NS,
                live_generation_ns: GENERATION_NS + 1,
            }
        );
    }

    /// Design §2.3's table, single-row half: H1 is `Absent × RowPresent`, H2 is
    /// `PresentZero × RowPresent`, and every neighbouring pair is neither.
    ///
    /// The coordinate side is built through [`CoordFrontierObservation::observe`]
    /// rather than by naming variants, so folding the map's miss back into its
    /// zero — the collapse this slice exists to undo — turns the H1 row into H2
    /// and fails here, not only in the `observe` test above.
    #[test]
    fn frontier_provenance_discriminates_h1_from_h2() {
        let table = [
            (
                CoordFrontierObservation::observe(None),
                row_present(),
                FrontierHypothesis::CoordEntryAbsentWithDurableRow,
            ),
            (
                CoordFrontierObservation::observe(Some(0)),
                row_present(),
                FrontierHypothesis::CoordNeverAdvancedWithDurableRow,
            ),
            // No durable witness: "never relayed" is not "relayed and lost".
            (
                CoordFrontierObservation::observe(None),
                DurableFrontierObservation::RowAbsent,
                FrontierHypothesis::Indeterminate,
            ),
            (
                CoordFrontierObservation::observe(Some(0)),
                DurableFrontierObservation::RowAbsent,
                FrontierHypothesis::Indeterminate,
            ),
            // An advancing coordinate is the healthy shape, not a hypothesis.
            (
                CoordFrontierObservation::observe(Some(8_192)),
                row_present(),
                FrontierHypothesis::Indeterminate,
            ),
            // §2.3 conditions H2 on the generations agreeing: an offset from an
            // earlier incarnation explains nothing about this one.
            (
                CoordFrontierObservation::observe(Some(0)),
                DurableFrontierObservation::observe(
                    Some(4_096),
                    Some(GENERATION_NS),
                    Some(GENERATION_NS + 1),
                ),
                FrontierHypothesis::Indeterminate,
            ),
        ];

        for (coord, durable, expected) in table {
            assert_eq!(
                FrontierProvenance::observe(coord, durable).hypothesis(None),
                expected,
                "{coord:?} x {durable:?}"
            );
        }
    }

    /// H3's half of the table. It takes the parent/thread pair: each end read
    /// alone lands somewhere in the single-row table, and only the two together
    /// name the split.
    #[test]
    fn the_parent_thread_pair_is_what_discriminates_h3() {
        let [(parent_id, parent_row), (thread_id, thread_row)] = parent_thread_rows(
            CoordFrontierObservation::Advanced { offset: 8_192 },
            CoordFrontierObservation::Absent,
        );
        assert_ne!(parent_id, thread_id, "H3 is a claim about two ChannelIds");

        assert_eq!(
            parent_row.hypothesis(None),
            FrontierHypothesis::Indeterminate,
            "the parent's own row looks healthy"
        );
        assert_eq!(
            thread_row.hypothesis(None),
            FrontierHypothesis::CoordEntryAbsentWithDurableRow,
            "the thread's own row is indistinguishable from H1 without its counterpart"
        );

        for (row, counterpart) in [
            (parent_row, thread_row.coord_observation),
            (thread_row, parent_row.coord_observation),
        ] {
            assert_eq!(
                row.hypothesis(Some(counterpart)),
                FrontierHypothesis::ChannelAxisSplit,
                "the split is nameable from either end of the axis"
            );
        }
    }

    /// The counterpart must not turn every pair into a split: agreement is
    /// about the KIND of observation, and two channels relaying different byte
    /// counts agree.
    #[test]
    fn an_agreeing_parent_thread_pair_leaves_the_single_row_table_standing() {
        let [(_, parent_row), (_, thread_row)] = parent_thread_rows(
            CoordFrontierObservation::Advanced { offset: 8_192 },
            CoordFrontierObservation::Advanced { offset: 12 },
        );
        assert_eq!(
            parent_row.hypothesis(Some(thread_row.coord_observation)),
            FrontierHypothesis::Indeterminate
        );

        let [(_, absent_parent), (_, absent_thread)] = parent_thread_rows(
            CoordFrontierObservation::Absent,
            CoordFrontierObservation::Absent,
        );
        assert_eq!(
            absent_parent.hypothesis(Some(absent_thread.coord_observation)),
            FrontierHypothesis::CoordEntryAbsentWithDurableRow,
            "both ends losing the entry is H1 on both, not an axis split"
        );
    }

    /// The detail surface publishes the two witnesses as two fields — the
    /// hypothesis rides beside them, never in place of them.
    #[test]
    fn frontier_provenance_report_publishes_both_witnesses_and_the_hypothesis() {
        let value = serde_json::to_value(FrontierProvenanceReport::of(
            FrontierProvenance::observe(CoordFrontierObservation::Absent, row_present()),
            None,
        ))
        .unwrap();

        assert_eq!(value["coord_observation"]["kind"], "absent");
        assert_eq!(value["durable_observation"]["kind"], "row_present");
        assert_eq!(value["durable_observation"]["relayed_start"], 4_096);
        assert_eq!(value["hypothesis"], "coord_entry_absent_with_durable_row");
    }
}
