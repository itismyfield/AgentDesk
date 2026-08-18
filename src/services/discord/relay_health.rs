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
    /// A row records one, BOTH generations were witnessed, and they agree.
    ///
    /// r1 review (legA P1-1, legB P2-2): this used to also mean "nothing on
    /// hand contradicts the generation", which folded row-generation-only,
    /// live-generation-only and neither-witnessed into the same value — and
    /// design §2.3 conditions H2 on `gen 일치`, so every one of those
    /// uncompared shapes was promoted to H2 on the strength of a comparison
    /// that never ran. Agreement is now a fact the type carries: an unwitnessed
    /// generation lands in [`Self::GenerationUnresolved`] instead.
    RowPresent {
        relayed_start: u64,
        generation_ns: i64,
    },
    /// A row records one, but the two generations cannot be compared because
    /// one of the witnesses said nothing.
    ///
    /// Not a mismatch and not an agreement — silence from either side is
    /// neither. Both readings are kept so the pair says WHICH witness was
    /// missing.
    GenerationUnresolved {
        relayed_start: u64,
        row_generation_ns: Option<i64>,
        live_generation_ns: Option<i64>,
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
    /// never advanced cannot manufacture a mismatch against a row that did —
    /// nor, since the r1 review, an agreement with one.
    pub(in crate::services::discord) fn observe(
        relayed_start: Option<u64>,
        row_generation_ns: Option<i64>,
        live_generation_ns: Option<i64>,
    ) -> Self {
        let Some(relayed_start) = relayed_start else {
            return Self::RowAbsent;
        };
        match (row_generation_ns, live_generation_ns) {
            (Some(row), Some(live)) if row == live => Self::RowPresent {
                relayed_start,
                generation_ns: row,
            },
            (Some(row), Some(live)) => Self::GenerationMismatch {
                relayed_start,
                row_generation_ns: row,
                live_generation_ns: live,
            },
            (row_generation_ns, live_generation_ns) => Self::GenerationUnresolved {
                relayed_start,
                row_generation_ns,
                live_generation_ns,
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
///
/// H3 (`channel_axis_split`) is NOT a member. Design §1.4 and §9's S1 row both
/// defer H3 to a later slice — "H3 판별은 S1 인수조건에서 제외" — and the r1
/// review measured what shipping it early cost: an unconditional H3 pre-check
/// swallowed `Absent × RowPresent`, i.e. H1 itself, whenever the polled channel
/// happened to sit on a parent/thread axis whose other end read differently.
/// The counterpart's raw coordinate observation is still published on
/// [`FrontierProvenanceReport`] so the evidence survives for that slice; what
/// this slice does not do is name a hypothesis from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum FrontierHypothesis {
    /// H1 — the coordinate entry is gone while the durable row still names a
    /// relayed offset: the frontier the poll reports is the `unwrap_or` zero,
    /// not a measurement.
    CoordEntryAbsentWithDurableRow,
    /// H2 — `PresentZero × RowPresent`. The entry exists and never advanced,
    /// while the row says a relay happened in the SAME incarnation. Since the
    /// r1 review `RowPresent` means both generations were witnessed and agree,
    /// which is the design's "gen 일치" arm as written.
    CoordNeverAdvancedWithDurableRow,
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

    /// The discrimination table — design §2.3's single-row half, which is all
    /// of it that S1 owns.
    ///
    /// It reads this channel's own pair and nothing else. No counterpart
    /// parameter: H3 is deferred (see [`FrontierHypothesis`]), and while it was
    /// a parameter its pre-check ran ahead of every row below and could rename
    /// an H1 reading as an axis split.
    ///
    /// The generation condition is H2's alone, as §2.3 writes it — H1 is
    /// `Absent × <the row names an offset>` with no generation clause. That
    /// asymmetry is forced, not cosmetic: `live_generation_ns` comes off the
    /// coordinate entry, so an ABSENT entry witnesses no generation by
    /// construction, and demanding one for H1 would make H1 unobservable on the
    /// production poll — the one shape this slice exists to see. H2's entry
    /// does exist and can carry a generation, so there the comparison is real
    /// and is required.
    ///
    /// What that comparison is NOT, r2 review (legA P1):
    /// `tmux_session_files::reset_relay_watermark_on_generation_change` parks
    /// the offset at zero and stamps the new generation in two writes that are
    /// not atomic together, so `PresentZero` beside a stamp is not by itself
    /// evidence that the stamp belongs to the parked zero.
    /// `health::session_enrichment::observe_frontier_triple` fences the offset
    /// load between two stamp loads and withholds the witness when they
    /// disagree, which narrows the window to the reads that fall entirely
    /// inside it. It does not close the window: a poll whose loads all land
    /// between the two writes reads a stale-equal pair (`offset = 0` beside
    /// the old stamp) that both stamp loads agree on. The pair is
    /// process-local (`TmuxRelayCoord` atomics die with the process, and no
    /// await or failable operation separates the writer's two stores), so the
    /// exposure lasts only until the second store lands — plus a concurrent
    /// reset that observes `watermark == 0` in that instant and early-returns
    /// without restamping. H2 off such a transient pair is not guaranteed
    /// against, here or by the fence; nothing about it survives a crash.
    pub(in crate::services::discord) fn hypothesis(self) -> FrontierHypothesis {
        match (self.coord_observation, self.durable_observation) {
            // `RowPresent` is design §2.3's H1 row as written, and is kept
            // spelled out for it. The production poll cannot reach it — r2
            // review (legB P2): `Absent` IS the map miss, so the entry that
            // would have witnessed a live generation is gone and
            // `DurableFrontierObservation::observe` can only answer
            // `GenerationUnresolved`. Dropping it would silently move a shape
            // the design table calls H1 into `Indeterminate`.
            (
                CoordFrontierObservation::Absent,
                DurableFrontierObservation::RowPresent { .. }
                | DurableFrontierObservation::GenerationUnresolved { .. },
            ) => FrontierHypothesis::CoordEntryAbsentWithDurableRow,
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
/// hypothesis. No verdict, classifier or recovery consumes it; `cli::doctor`'s
/// `frontier_provenance_evidence` reads it back as display-only evidence.
#[derive(Debug, Serialize)]
pub(in crate::services::discord) struct FrontierProvenanceReport {
    #[serde(flatten)]
    provenance: FrontierProvenance,
    /// The other end of this channel's parent/thread axis, when the caller
    /// resolved one, exactly as that channel's coordinate reads.
    ///
    /// Raw evidence, deliberately inert: H3 is the hypothesis this witness
    /// exists for and S1 does not produce it (design §1.4, §9's S1 row). It is
    /// published anyway so the two-row shape H3 needs is already on the wire
    /// when the slice that discriminates it lands, rather than having to be
    /// reconstructed from separate polls after the fact. `None` when this
    /// channel is not part of a parent/thread pair.
    counterpart_coord_observation: Option<CoordFrontierObservation>,
    hypothesis: FrontierHypothesis,
}

impl FrontierProvenanceReport {
    pub(in crate::services::discord) fn of(
        provenance: FrontierProvenance,
        counterpart_coord: Option<CoordFrontierObservation>,
    ) -> Self {
        Self {
            provenance,
            counterpart_coord_observation: counterpart_coord,
            hypothesis: provenance.hypothesis(),
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
    pub(in crate::services::discord) fn test_snapshot() -> Self {
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

    /// The three watcher terms that each say this binding is NOT a live relay
    /// owner: no handle attached, an attached handle whose heartbeat is stale
    /// (`watcher_attached_stale`), or an in-flight row that does not record the
    /// watcher as owning the live relay. `watcher_owns_live_relay` is read off
    /// the row (`SessionEnrichment::watcher_owns_live_relay`) and not off the
    /// handle, so the first and third terms are independent observations rather
    /// than one restated twice.
    ///
    /// This is exactly the guard `RelayStallClassifier::classify` and
    /// `relay_recovery::decision::eligible_reattach_watcher` already place to
    /// the LEFT of their
    /// [`Self::relay_frontier_never_advanced_with_unread_tail`] call — both now
    /// spell it through this method. That shared spelling is what makes the
    /// absorption keeping those two call sites invariant under #5071 relay-tail
    /// S3 a property of one term instead of a coincidence between three
    /// duplicated ones: with `A` this method and `U` a measured waiting tail,
    /// `A || (rest && (U || (!measured && A)))` reduces to `A || (rest && U)`,
    /// which is what each site decided before S3.
    pub(in crate::services::discord) fn watcher_binding_is_not_a_live_relay_owner(&self) -> bool {
        !self.watcher_attached || self.watcher_attached_stale || !self.watcher_owns_live_relay
    }

    /// True for the restart/desync signature where a watcher handle still looks
    /// live and may even own the tmux session, but the relay frontier never
    /// advanced while the transcript/capture accumulated bytes.
    ///
    /// #5071 relay-tail S3: the tail term is a disjunction because on its own it
    /// now UNDER-fires. `unread_bytes` became three-valued when S1 withdrew the
    /// durable-frontier synthesis, and its `None` — the enumeration is on
    /// [`Self::idle_witness_tail_is_not_waiting`] — makes `is_some_and(> 0)`
    /// false without anything having been measured, silencing dead frontiers
    /// that are otherwise fully witnessed. Of that three-arm enumeration, the
    /// only arm this disjunct can actually reach in production is the third
    /// (row/binding tmux mismatch → frontier unattributable): the other two
    /// arms leave `last_capture_offset` at `None` as well, so they already fall
    /// out of the `capture > relay` conjunct above and stay permanently silent
    /// after S3. In that reachable arm `desynced` is implied (the mismatch is
    /// one of its disjuncts in `session_enrichment`), so the effective gate is
    /// mismatch + live pane + capture ahead of a never-moved frontier + an
    /// independent witness. The second disjunct fires those from
    /// evidence that never passes through the tail:
    /// [`Self::watcher_binding_is_not_a_live_relay_owner`]. It is gated on the
    /// tail being unmeasured, so a measured `Some(0)` keeps its S2 meaning — a
    /// real drained-tail measurement, which this predicate does not overrule —
    /// and an unmeasured tail still never fires this predicate by itself.
    ///
    /// `last_capture_offset > 0` is deliberately NOT restated in that disjunct:
    /// the `capture > self.last_relay_offset` conjunct above already entails it
    /// for every `u64` value of `last_relay_offset`, and the `last_relay_offset
    /// == 0` conjunct entails it a second time.
    ///
    /// A dead tmux session is not available as one of those witnesses:
    /// `tmux_alive == Some(true)` is a conjunct here, so every shape this
    /// predicate describes is a live pane whose relay is dead.
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
            && (self.unread_bytes.is_some_and(|bytes| bytes > 0)
                || (self.unread_bytes.is_none()
                    && self.watcher_binding_is_not_a_live_relay_owner()))
    }

    /// #5071 relay-tail S2: the reading of `unread_bytes` that 4987 §-1.4's
    /// second alive witness needs — see the `pane_idle_confirmed` call site in
    /// `health::snapshot`.
    ///
    /// The field is three-valued and its `None` is UNMEASURED, not
    /// measured-empty. `SessionEnrichment::load` measures the in-flight ROW's
    /// capture coordinate against the channel's relay frontier, so — the same
    /// enumeration the sibling doc on
    /// `relay_recovery::unread_tail_is_proven_drained` gives — `None` is produced
    /// when the row carries no `output_path` (`seed_runtime` leaves it absent for
    /// a remote profile, a provider with no managed-tmux backend, an unavailable
    /// tmux binary, an absent tmux session name, a Claude-TUI prelaunch, and
    /// non-unix: the four conjuncts of `provider_isolation::seed_runtime`'s
    /// guard, then the Claude-TUI prelaunch seeding it delegates to
    /// (`prelaunch_inflight_runtime_seed_from_paths`) and its non-unix arm), when
    /// `std::fs::metadata` on that path failed, or when the row's tmux session
    /// and the watcher binding's disagree so the frontier is not attributable to
    /// the row.
    ///
    /// The first arm is the channel with NO row, where none of those three can
    /// even be asked: no row, no `output_path`, no capture coordinate, so the
    /// field is `None` structurally and never carried a measurement. Requiring
    /// `Some(0)` there would deny the witness to every rowless channel rather
    /// than reject evidence, so the arm keeps them.
    ///
    /// The second arm is NOT "an active turn" and does not claim a row implies a
    /// readable path. A row present with `active_turn == None` is a live shape —
    /// `health::snapshot::relay_active_turn_from_inflight` reports `None` for a
    /// #3631 rebind-origin row and for an ownerless TUI-direct `ExternalInput`
    /// row — and such a row may itself have no `output_path` at all. All this
    /// arm requires is that the measurement was attempted and read empty: with a
    /// row present the `None` is unknown, and an unknown tail cannot witness
    /// liveness whether the stat failed, the path was absent or the frontier was
    /// unattributable. That is the fail-closed reading.
    /// [`Self::relay_frontier_never_advanced_with_unread_tail`] takes the same
    /// one from the other side, in the scope #5071 relay-tail S3 left it: an
    /// unmeasured tail never carries that FAILURE by itself there either. S3
    /// did give it a second disjunct that can fire while `unread_bytes` is
    /// `None`, but what fires it is an independent watcher witness and the
    /// `None` only permits it — the field still testifies to nothing on either
    /// side. The row-present `None` that
    /// `reachability::composite_tests::section_6_2_enrichment` builds is the
    /// failed-stat one. What `Some(0)` itself does and does not prove is in
    /// `relay_recovery::unread_tail_is_proven_drained`.
    pub(in crate::services::discord) fn idle_witness_tail_is_not_waiting(&self) -> bool {
        if !self.bridge_inflight_present {
            return true;
        }
        self.unread_bytes == Some(0)
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
        if snapshot.tmux_alive == Some(true)
            && snapshot.desynced
            && (snapshot.watcher_binding_is_not_a_live_relay_owner()
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

    /// #5071 relay-tail S2: the full three-valued table for the alive witness's
    /// tail term, on both sides of the row-present split.
    ///
    /// The two rows that matter: `None` with no row keeps the witness (that is
    /// the rowless idle population), and `None` WITH a row — an unknown tail,
    /// not a drained one — no longer helps assert liveness.
    #[test]
    fn idle_witness_tail_reads_unmeasured_as_waiting_only_when_a_row_is_present() {
        let cases: Vec<(&str, bool, Option<u64>, bool)> = vec![
            (
                "no row, unmeasured: nothing could be waiting",
                false,
                None,
                true,
            ),
            ("row present, measured drained", true, Some(0), true),
            (
                "row present, unmeasured tail withholds the witness",
                true,
                None,
                false,
            ),
            ("row present, measured tail waiting", true, Some(128), false),
            // No row is the only producer of `None` for a rowless channel, so
            // the two rows below are unreachable through
            // `SessionEnrichment::load`; they pin the predicate's polarity
            // rather than a live shape.
            ("no row, measured drained", false, Some(0), true),
            ("no row, measured tail waiting", false, Some(128), true),
        ];
        for (label, bridge_inflight_present, unread_bytes, expected) in cases {
            let snapshot = RelayHealthSnapshot {
                bridge_inflight_present,
                unread_bytes,
                ..RelayHealthSnapshot::test_snapshot()
            };
            assert_eq!(
                snapshot.idle_witness_tail_is_not_waiting(),
                expected,
                "{label}"
            );
        }
    }

    /// The shape every #5071 relay-tail S3 row shares: a live pane, a desync,
    /// and a capture coordinate ahead of a relay frontier that never moved. Only
    /// the tail term and the three watcher terms vary across the rows.
    fn dead_frontier_shape(
        unread_bytes: Option<u64>,
        watcher_attached: bool,
        watcher_attached_stale: bool,
        watcher_owns_live_relay: bool,
    ) -> RelayHealthSnapshot {
        RelayHealthSnapshot {
            desynced: true,
            tmux_alive: Some(true),
            last_relay_ts_ms: None,
            last_relay_offset: 0,
            last_capture_offset: Some(128),
            unread_bytes,
            watcher_attached,
            watcher_attached_stale,
            watcher_owns_live_relay,
            ..RelayHealthSnapshot::test_snapshot()
        }
    }

    /// #5071 relay-tail S3: the tail term alone under-fires once S1 made
    /// `unread_bytes` three-valued, so an independent watcher witness fires the
    /// predicate on an unmeasured tail — and only on an unmeasured one.
    ///
    /// Deleting the `watcher_binding_is_not_a_live_relay_owner()` disjunct from
    /// the predicate kills the three witness rows; deleting the
    /// `unread_bytes.is_none()` gate beside it kills the drained-tail row.
    #[test]
    fn dead_frontier_fires_on_an_unmeasured_tail_only_with_an_independent_witness() {
        let cases: Vec<(&str, Option<u64>, bool, bool, bool, bool)> = vec![
            (
                "measured waiting tail under a live owning watcher: the pre-S3 path, unchanged",
                Some(128),
                true,
                false,
                true,
                true,
            ),
            (
                "unmeasured tail with no other dead witness stays silent",
                None,
                true,
                false,
                true,
                false,
            ),
            (
                "unmeasured tail plus a stale watcher heartbeat fires",
                None,
                true,
                true,
                true,
                true,
            ),
            (
                "unmeasured tail plus a row that does not own the live relay fires",
                None,
                true,
                false,
                false,
                true,
            ),
            (
                "unmeasured tail plus no attached handle fires",
                None,
                false,
                false,
                true,
                true,
            ),
            (
                "a measured drained tail is a real measurement and the witness does not overrule it",
                Some(0),
                true,
                true,
                true,
                false,
            ),
            (
                "a measured waiting tail fires on the tail alone, witness or not",
                Some(128),
                true,
                true,
                true,
                true,
            ),
        ];
        for (label, unread_bytes, attached, stale, owns_live_relay, expected) in cases {
            let snapshot = dead_frontier_shape(unread_bytes, attached, stale, owns_live_relay);
            assert_eq!(
                snapshot.relay_frontier_never_advanced_with_unread_tail(),
                expected,
                "{label}"
            );
        }
    }

    /// S3 widened the tail term and nothing else: the shared conjuncts still
    /// gate every firing, the witness-driven one included.
    #[test]
    fn dead_frontier_shared_conjuncts_still_gate_the_witness_disjunct() {
        let witnessed = || dead_frontier_shape(None, true, true, true);
        assert!(
            witnessed().relay_frontier_never_advanced_with_unread_tail(),
            "the witness row this test negates must itself fire"
        );

        let cases: Vec<(&str, RelayHealthSnapshot)> = vec![
            (
                "no desync",
                RelayHealthSnapshot {
                    desynced: false,
                    ..witnessed()
                },
            ),
            (
                "tmux is not alive, so this is not a live-pane dead relay",
                RelayHealthSnapshot {
                    tmux_alive: Some(false),
                    ..witnessed()
                },
            ),
            (
                "tmux liveness unknown",
                RelayHealthSnapshot {
                    tmux_alive: None,
                    ..witnessed()
                },
            ),
            (
                "the relay did emit once, so the frontier is not never-advanced",
                RelayHealthSnapshot {
                    last_relay_ts_ms: Some(1_777_001_234_000),
                    ..witnessed()
                },
            ),
            (
                "the relay frontier already moved off zero",
                RelayHealthSnapshot {
                    last_relay_offset: 128,
                    ..witnessed()
                },
            ),
            (
                "no capture coordinate to be ahead of the frontier",
                RelayHealthSnapshot {
                    last_capture_offset: None,
                    ..witnessed()
                },
            ),
            (
                "the capture coordinate never passed the frontier",
                RelayHealthSnapshot {
                    last_capture_offset: Some(0),
                    ..witnessed()
                },
            ),
        ];
        for (label, snapshot) in cases {
            assert!(
                !snapshot.relay_frontier_never_advanced_with_unread_tail(),
                "{label}"
            );
        }
    }

    /// #5071 relay-tail S3 blast radius at the one consumer that CLASSIFIES
    /// rather than repairs. `classify` guards its predicate read with
    /// `watcher_binding_is_not_a_live_relay_owner()`, which is exactly what the
    /// new disjunct requires, so `A || (rest && (U || (!measured && A)))`
    /// absorbs back to the pre-S3 `A || (rest && U)`. This enumerates the whole
    /// watcher x tail cross-product on the dead-frontier shape and finds no
    /// input whose classification moved.
    #[test]
    fn s3_widening_moves_no_relay_stall_classification() {
        for attached in [false, true] {
            for stale in [false, true] {
                for owns_live_relay in [false, true] {
                    for unread_bytes in [None, Some(0_u64), Some(128_u64)] {
                        let snapshot =
                            dead_frontier_shape(unread_bytes, attached, stale, owns_live_relay);
                        let label = format!(
                            "attached={attached} stale={stale} owns_live_relay={owns_live_relay} unread_bytes={unread_bytes:?}"
                        );

                        // Every shared conjunct of the predicate holds on this
                        // shape, so its pre-S3 value here was exactly the tail
                        // term — which is what makes this a faithful stand-in
                        // for the branch `classify` took before S3.
                        let pre_s3_relay_dead = snapshot
                            .watcher_binding_is_not_a_live_relay_owner()
                            || snapshot.unread_bytes.is_some_and(|bytes| bytes > 0);
                        let s3_relay_dead = snapshot.watcher_binding_is_not_a_live_relay_owner()
                            || snapshot.relay_frontier_never_advanced_with_unread_tail();
                        assert_eq!(
                            s3_relay_dead, pre_s3_relay_dead,
                            "S3 moved the relay-dead branch for {label}"
                        );

                        let expected = if pre_s3_relay_dead {
                            RelayStallState::TmuxAliveRelayDead
                        } else {
                            RelayStallState::Healthy
                        };
                        assert_eq!(
                            RelayStallClassifier::classify(&snapshot),
                            expected,
                            "{label}"
                        );
                    }
                }
            }
        }
    }

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

    /// A durable row that named a relayed offset with BOTH generations
    /// witnessed and agreeing — the `RowPresent` H2 is conditioned on.
    ///
    /// r1 review: the third argument used to be `None`, so this fixture stood
    /// for "nothing contradicts the generation" and H2 was reachable without a
    /// comparison ever running. It now supplies the live witness it claims.
    fn row_present() -> DurableFrontierObservation {
        DurableFrontierObservation::observe(Some(4_096), Some(GENERATION_NS), Some(GENERATION_NS))
    }

    /// The durable row an ABSENT coordinate produces: the row names an offset
    /// and its own generation, and there is no entry left to witness a live one.
    /// This — not [`row_present`] — is H1's production shape.
    fn row_with_unresolved_generation() -> DurableFrontierObservation {
        DurableFrontierObservation::observe(Some(4_096), Some(GENERATION_NS), None)
    }

    /// The durable row an END with this coordinate reading actually polls
    /// with. r2 review (legB P2): pairing every end with [`row_present`]
    /// gave the `Absent` ends `Absent × RowPresent`, a shape the production
    /// poll cannot produce — an absent entry witnesses no live generation — so
    /// the H1 assertions below were being made against the one H1 row nothing
    /// reaches.
    fn row_for(coord: CoordFrontierObservation) -> DurableFrontierObservation {
        match coord {
            CoordFrontierObservation::Absent => row_with_unresolved_generation(),
            _ => row_present(),
        }
    }

    /// A parent channel and the thread hung off it, each carrying its OWN
    /// coordinate reading — the two-row shape H3 would need. S1 does not
    /// discriminate H3 (design §1.4); the fixture stays so the deferral is
    /// asserted against the shape rather than against its absence.
    fn parent_thread_rows(
        parent: CoordFrontierObservation,
        thread: CoordFrontierObservation,
    ) -> [(ChannelId, FrontierProvenance); 2] {
        [
            (
                ChannelId::new(PARENT_CHANNEL),
                FrontierProvenance::observe(parent, row_for(parent)),
            ),
            (
                ChannelId::new(THREAD_CHANNEL),
                FrontierProvenance::observe(thread, row_for(thread)),
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

    /// Both a mismatch and an agreement are claims about two WITNESSED
    /// generations. A witness that says nothing can produce neither, so the
    /// one-witness and no-witness readings are their own observation (r1
    /// review, legA P1-1 / legB P2-2) rather than being folded into either.
    #[test]
    fn a_durable_row_needs_two_witnessed_generations_to_agree_or_to_mismatch() {
        assert_eq!(
            DurableFrontierObservation::observe(None, Some(GENERATION_NS), Some(GENERATION_NS + 1)),
            DurableFrontierObservation::RowAbsent,
            "no relayed offset is no row, whatever the generations say"
        );
        assert_eq!(
            DurableFrontierObservation::observe(Some(9), None, Some(GENERATION_NS)),
            DurableFrontierObservation::GenerationUnresolved {
                relayed_start: 9,
                row_generation_ns: None,
                live_generation_ns: Some(GENERATION_NS),
            },
            "a live generation alone compares against nothing"
        );
        assert_eq!(
            DurableFrontierObservation::observe(Some(9), Some(GENERATION_NS), None),
            DurableFrontierObservation::GenerationUnresolved {
                relayed_start: 9,
                row_generation_ns: Some(GENERATION_NS),
                live_generation_ns: None,
            },
            "a row generation alone compares against nothing"
        );
        assert_eq!(
            DurableFrontierObservation::observe(Some(9), None, None),
            DurableFrontierObservation::GenerationUnresolved {
                relayed_start: 9,
                row_generation_ns: None,
                live_generation_ns: None,
            },
            "neither witness is not an agreement either"
        );
        assert_eq!(
            DurableFrontierObservation::observe(Some(9), Some(GENERATION_NS), Some(GENERATION_NS)),
            DurableFrontierObservation::RowPresent {
                relayed_start: 9,
                generation_ns: GENERATION_NS,
            },
            "two witnesses agreeing is the only agreement"
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

    /// Design §2.3's table, single-row half — which after the r1 review is the
    /// whole of it that S1 produces.
    ///
    /// The coordinate side is built through [`CoordFrontierObservation::observe`]
    /// rather than by naming variants, so folding the map's miss back into its
    /// zero — the collapse this slice exists to undo — turns the H1 rows into
    /// H2 and fails here, not only in the `observe` test above.
    #[test]
    fn frontier_provenance_discriminates_h1_from_h2() {
        let table = [
            // H1's production shape: no entry, so no live generation to resolve
            // the row's against. H1 is not conditioned on one.
            (
                CoordFrontierObservation::observe(None),
                row_with_unresolved_generation(),
                FrontierHypothesis::CoordEntryAbsentWithDurableRow,
            ),
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
            // r1 review (legA P1-1, legB P2-2): the single-witness rows. H2's
            // entry EXISTS, so its generation is a witness that was available
            // and silent — "agreeing" cannot be inferred from that, and the
            // three uncompared shapes below used to be promoted to H2 anyway.
            (
                CoordFrontierObservation::observe(Some(0)),
                DurableFrontierObservation::observe(Some(4_096), Some(GENERATION_NS), None),
                FrontierHypothesis::Indeterminate,
            ),
            (
                CoordFrontierObservation::observe(Some(0)),
                DurableFrontierObservation::observe(Some(4_096), None, Some(GENERATION_NS)),
                FrontierHypothesis::Indeterminate,
            ),
            (
                CoordFrontierObservation::observe(Some(0)),
                DurableFrontierObservation::observe(Some(4_096), None, None),
                FrontierHypothesis::Indeterminate,
            ),
        ];

        for (coord, durable, expected) in table {
            assert_eq!(
                FrontierProvenance::observe(coord, durable).hypothesis(),
                expected,
                "{coord:?} x {durable:?}"
            );
        }
    }

    /// H3 is deferred, and this is what deferring it has to mean: a
    /// parent/thread pair that DISAGREES about the coordinate changes no
    /// channel's hypothesis (r1 review, legB P1-2).
    ///
    /// The pre-check that named the disagreement ran ahead of every row of the
    /// table, so the thread end below — `Absent × <row names an offset>`, H1
    /// exactly — was reported as `channel_axis_split` purely because it sat on
    /// an axis. Each end keeps its own single-row reading; the counterpart is
    /// carried as evidence and decides nothing.
    #[test]
    fn a_disagreeing_parent_thread_pair_names_no_hypothesis_in_s1() {
        let [(parent_id, parent_row), (thread_id, thread_row)] = parent_thread_rows(
            CoordFrontierObservation::Advanced { offset: 8_192 },
            CoordFrontierObservation::Absent,
        );
        assert_ne!(
            parent_id, thread_id,
            "H3 would be a claim about two ChannelIds"
        );

        assert_eq!(
            parent_row.hypothesis(),
            FrontierHypothesis::Indeterminate,
            "the parent's own row looks healthy"
        );
        assert_eq!(
            thread_row.hypothesis(),
            FrontierHypothesis::CoordEntryAbsentWithDurableRow,
            "the thread's own row is H1 and must stay H1 while H3 is deferred"
        );

        for (row, counterpart) in [
            (parent_row, thread_row.coord_observation),
            (thread_row, parent_row.coord_observation),
        ] {
            let report =
                serde_json::to_value(FrontierProvenanceReport::of(row, Some(counterpart))).unwrap();
            assert_eq!(
                report["hypothesis"],
                serde_json::to_value(row.hypothesis()).unwrap(),
                "publishing the counterpart must not restate the hypothesis"
            );
            assert_ne!(
                report["hypothesis"], "channel_axis_split",
                "S1 produces no H3 (design §1.4, §9's S1 row)"
            );
            assert!(
                !report["counterpart_coord_observation"].is_null(),
                "the counterpart survives as evidence for the slice that does discriminate H3"
            );
        }
    }

    /// An AGREEING pair is the other half of the same deferral: the counterpart
    /// is inert in both directions, so agreement adds nothing either.
    #[test]
    fn an_agreeing_parent_thread_pair_leaves_the_single_row_table_standing() {
        let [(_, parent_row), (_, thread_row)] = parent_thread_rows(
            CoordFrontierObservation::Advanced { offset: 8_192 },
            CoordFrontierObservation::Advanced { offset: 12 },
        );
        assert_eq!(parent_row.hypothesis(), FrontierHypothesis::Indeterminate);
        assert_eq!(
            thread_row.hypothesis(),
            FrontierHypothesis::Indeterminate,
            "two ends relaying different byte counts are both just advancing"
        );

        let [(_, absent_parent), (_, absent_thread)] = parent_thread_rows(
            CoordFrontierObservation::Absent,
            CoordFrontierObservation::Absent,
        );
        assert_eq!(
            absent_parent.hypothesis(),
            FrontierHypothesis::CoordEntryAbsentWithDurableRow,
            "both ends losing the entry is H1 on both"
        );
        assert_eq!(
            absent_thread.hypothesis(),
            FrontierHypothesis::CoordEntryAbsentWithDurableRow
        );
    }

    /// The detail surface publishes the two witnesses as two fields — the
    /// hypothesis rides beside them, never in place of them — and the
    /// counterpart rides beside all three as raw evidence.
    #[test]
    fn frontier_provenance_report_publishes_both_witnesses_and_the_hypothesis() {
        let value = serde_json::to_value(FrontierProvenanceReport::of(
            FrontierProvenance::observe(
                CoordFrontierObservation::Absent,
                row_with_unresolved_generation(),
            ),
            Some(CoordFrontierObservation::Advanced { offset: 8_192 }),
        ))
        .unwrap();

        assert_eq!(value["coord_observation"]["kind"], "absent");
        assert_eq!(
            value["durable_observation"]["kind"],
            "generation_unresolved"
        );
        assert_eq!(value["durable_observation"]["relayed_start"], 4_096);
        assert_eq!(value["counterpart_coord_observation"]["kind"], "advanced");
        assert_eq!(value["hypothesis"], "coord_entry_absent_with_durable_row");
    }
}
