use poise::serenity_prelude::ChannelId;

use crate::services::discord::relay_health::{
    CoordFrontierObservation, DurableFrontierObservation, FrontierProvenance,
};
use crate::services::discord::{self as discord, SharedData};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;

use super::liveness_authority::{CaptureCoordinateObservation, CoordinateStatus};

pub(super) const WATCHER_STATE_DESYNC_STALE_MS: i64 = 30_000;

/// #5071 relay-tail S1 (I-4): one channel's relay coordinate, read once.
///
/// Both the offsets [`SessionEnrichment::load`] already took from this entry
/// and the observation that says whether the entry existed at all come out of
/// the SAME lookup, so the two cannot disagree and — the r1 review's point
/// (legB P1-1) — there is exactly one place where a missing entry becomes a
/// reading. Collapsing that miss into a zero is therefore one edit, and it is
/// an edit the production poll path is tested through.
///
/// r2 review (legA P2): "the same lookup" scopes the fields below and
/// nothing else. [`SessionEnrichment::has_relay_coord`] is a SEPARATE
/// `contains_key` on the same map, so an insert landing between the two can
/// still publish `has_relay_coord = false` beside a `PresentZero` observation
/// inside one poll. That window predates this slice and stays; what is
/// corrected here is the claim, not the read.
struct CoordFrontierReading {
    observation: CoordFrontierObservation,
    /// `confirmed_end_offset`, `last_relay_ts_ms` and `reconnect_count` as of
    /// that lookup. `None` when the map held no entry.
    entry: Option<(u64, i64, u64)>,
    /// The generation the offset above is attributable to. `None` when the
    /// entry witnesses none — never stamped, or the stamp moved under the read
    /// (see [`observe_frontier_triple`]).
    live_generation_ns: Option<i64>,
}

/// #5071 relay-tail S1 r3 (legA P1): what one entry's raw
/// `(generation, offset, generation)` triple observes — `None` for the map
/// miss, which keeps a missing entry becoming a reading in exactly one place.
///
/// `tmux_session_files::reset_relay_watermark_on_generation_change` CASes
/// `confirmed_end_offset` to 0 and only THEN stores the new wrapper's
/// `.generation` stamp. A poll loading the offset between those two writes sees
/// `0` paired with the OLD generation, and against a durable row from that same
/// old incarnation the pair reads `PresentZero × RowPresent` — H2 — for a reset
/// in flight rather than for a coordinate that never advanced.
///
/// Same fence as `tmux_session_files::committed_frontier_for_current_generation`
/// (#3358 r2): the stamp has to agree on both sides of the offset load. When it
/// does not, the live generation is no witness for THIS offset, and `None`
/// sends the durable side to `GenerationUnresolved` — Indeterminate — rather
/// than to a hypothesis. `0` is the atomic's "never stamped", which is not a
/// witness either.
///
/// The fence sees only a transition it straddles. A poll whose loads all land
/// between the writer's two adjacent stores reads a stale-equal pair
/// (`offset = 0` beside the old stamp) that both stamp loads agree on. The
/// pair lives in `TmuxRelayCoord`'s process-local atomics — no await or
/// failable operation separates the two stores, and a crash destroys the pair
/// with the process rather than persisting it — so the exposure is transient —
/// see [`crate::services::discord::relay_health::FrontierProvenance::hypothesis`].
fn observe_frontier_triple(
    triple: Option<(i64, u64, i64)>,
) -> (CoordFrontierObservation, Option<i64>) {
    let Some((generation_before, confirmed_end_offset, generation_after)) = triple else {
        return (CoordFrontierObservation::observe(None), None);
    };
    let live_generation_ns = (generation_after != 0 && generation_before == generation_after)
        .then_some(generation_after);
    (
        CoordFrontierObservation::observe(Some(confirmed_end_offset)),
        live_generation_ns,
    )
}

/// The single map read. Lock-free `.get()`; nothing is inserted, so a channel
/// that has no entry keeps not having one.
fn read_coord_frontier(shared: &SharedData, channel: ChannelId) -> CoordFrontierReading {
    use std::sync::atomic::Ordering::Acquire;
    let read = shared.tmux_relay_coords.get(&channel).map(|coord| {
        // Generation, offset, generation — the fence `observe_frontier_triple`
        // documents. The two remaining fields load after it, so nothing widens
        // the window between the stamp reads.
        let generation_before = coord.confirmed_end_generation_mtime_ns.load(Acquire);
        let confirmed_end_offset = coord.confirmed_end_offset.load(Acquire);
        let generation_after = coord.confirmed_end_generation_mtime_ns.load(Acquire);
        (
            (generation_before, confirmed_end_offset, generation_after),
            (
                confirmed_end_offset,
                coord.last_relay_ts_ms.load(Acquire),
                coord.reconnect_count.load(Acquire),
            ),
        )
    });
    let (observation, live_generation_ns) = observe_frontier_triple(read.map(|(triple, _)| triple));
    CoordFrontierReading {
        observation,
        entry: read.map(|(_, entry)| entry),
        live_generation_ns,
    }
}

/// The coordinate witness alone, for a channel this poll is not enriching —
/// the OTHER end of a parent/thread axis, which `health::snapshot` resolves and
/// no single channel's enrichment can see.
///
/// Same reader as the polled channel's own: [`SessionEnrichment::load`] calls
/// [`read_coord_frontier`] too, rather than spelling the lookup a second time.
pub(super) fn observe_coord_frontier(
    shared: &SharedData,
    channel: ChannelId,
) -> CoordFrontierObservation {
    read_coord_frontier(shared, channel).observation
}

#[derive(Debug)]
pub(super) struct SessionEnrichment {
    pub inflight: Option<discord::inflight::InflightTurnState>,
    pub attached: bool,
    pub watcher_attached: bool,
    /// #3277 (Defect D): the channel HAS a watcher binding, but the bound
    /// handle is heartbeat-stale. A set cancel flag is tracked by watcher-claim
    /// replacement paths, but is intentionally not reported as heartbeat stale:
    /// otherwise a fresh-heartbeat watcher whose cancel flag was raised first is
    /// mislabeled as `watcher_attached_stale`.
    pub watcher_attached_stale: bool,
    pub has_relay_coord: bool,
    pub watcher_owner_channel_id: Option<u64>,
    /// #5071 T4-B0 (#4987 S0): the transcript the LIVE watcher is tailing for
    /// this channel, taken off [`discord::TmuxWatcherBinding::output_path`].
    ///
    /// #4987 §-1.1 R1 makes this its first-priority transcript source, ahead of
    /// the in-flight row's `output_path`, because the row is not independently
    /// sourced from the watcher and #4986 observed the two disagreeing — the
    /// watcher had followed the provider-native TUI transcript while the row
    /// still named the prelaunch wrapper file. Health code was not blind to the
    /// watcher's path before this slice — `health::relay_auto_heal` reads it
    /// straight off the handle to gate its redrive nudge, fixed by
    /// `redrive_nudge_requires_matching_output_path`. What no enrichment field
    /// carried was the path itself, so the snapshot health hands out could not
    /// name the watcher's side of the split.
    ///
    /// `None` when the channel has no live watcher binding. Read-only: nothing
    /// here treats it as an authority, and no existing health field changed
    /// source because of it.
    pub watcher_output_path: Option<String>,
    pub tmux_session: Option<String>,
    pub inflight_state_present: bool,
    pub tmux_session_mismatch: bool,
    pub last_relay_offset: u64,
    pub last_relay_ts_ms: i64,
    pub reconnect_count: u64,
    pub last_capture_offset: Option<u64>,
    pub capture_coordinate: CaptureCoordinateObservation,
    pub unread_bytes: Option<u64>,
    pub relay_stale: bool,
    pub capture_lagged: bool,
    /// #5071 relay-tail S1 (I-4): which witnesses produced the frontier fields
    /// above. Descriptive — no field on this struct changed source or polarity
    /// because of it, and nothing but the health detail reads it.
    pub frontier_provenance: FrontierProvenance,
}

impl SessionEnrichment {
    pub async fn load(
        shared: &SharedData,
        provider_kind: Option<&ProviderKind>,
        channel: ChannelId,
    ) -> Self {
        let watcher_binding = shared.tmux_watchers.channel_binding(&channel);
        let inflight =
            provider_kind.and_then(|pk| discord::inflight::load_inflight_state(pk, channel.get()));
        let inflight_tmux_session = inflight
            .as_ref()
            .and_then(|state| state.tmux_session_name.clone());
        let inflight_owner_channel_id = inflight_tmux_session
            .as_deref()
            .and_then(|tmux| shared.tmux_watchers.owner_channel_for_tmux_session(tmux));
        let inflight_owner_matches_channel = inflight_owner_channel_id == Some(channel);
        let watcher_attached = watcher_binding.is_some();
        let attached = watcher_attached || inflight_owner_matches_channel;
        let watcher_binding_tmux_session = watcher_binding
            .as_ref()
            .map(|binding| binding.tmux_session_name.clone());
        let watcher_attached_stale = watcher_binding_tmux_session
            .as_deref()
            .is_some_and(|tmux| shared.tmux_watchers.tmux_session_is_stale(tmux) == Some(true));
        let relay_state_matches_inflight = match (
            inflight_tmux_session.as_deref(),
            watcher_binding_tmux_session.as_deref(),
        ) {
            (Some(inflight_tmux), Some(binding_tmux)) => inflight_tmux == binding_tmux,
            _ => true,
        };
        let has_relay_coord = shared.tmux_relay_coords.contains_key(&channel);
        let inflight_state_present = inflight.is_some();
        let tmux_session_mismatch = inflight_state_present
            && !relay_state_matches_inflight
            && watcher_binding_tmux_session.is_some()
            && inflight_tmux_session.is_some();
        let watcher_owner_channel_id = watcher_binding
            .as_ref()
            .map(|binding| binding.owner_channel_id)
            .or(inflight_owner_channel_id)
            .map(|id| id.get());
        let watcher_output_path = watcher_output_path_from_binding(watcher_binding.as_ref());
        // The active in-flight row identifies the producer whose liveness this
        // health snapshot classifies. A stale watcher binding must never retarget
        // that probe to another tmux session and lend its death to this turn.
        let tmux_session = liveness_probe_session(
            inflight_tmux_session.as_deref(),
            watcher_binding_tmux_session.as_deref(),
        );
        // #5071 relay-tail S1 (I-4): one lookup, two independent readings, and
        // — since the r1 review — through the same reader the counterpart's
        // observation goes through. The tuple below keeps its
        // `unwrap_or((0, 0, 0))` meaning byte for byte, S2 owns making an
        // unsourced frontier unknown, while `coord_observation` records whether
        // that zero came from an entry or from the miss.
        let coord = read_coord_frontier(shared, channel);
        let coord_observation = coord.observation;
        let live_generation_mtime_ns = coord.live_generation_ns;
        let (last_relay_offset, last_relay_ts_ms, reconnect_count) =
            coord.entry.unwrap_or((0, 0, 0));
        let durable_observation = DurableFrontierObservation::observe(
            inflight
                .as_ref()
                .and_then(|state| state.last_watcher_relayed_offset),
            inflight
                .as_ref()
                .and_then(|state| state.last_watcher_relayed_generation_mtime_ns)
                .filter(|generation| *generation != 0),
            live_generation_mtime_ns,
        );
        let output_path_for_metadata = inflight
            .as_ref()
            .and_then(|state| state.output_path.as_deref())
            .map(str::to_string);
        let capture_coordinate = match output_path_for_metadata {
            Some(path) => tokio::task::spawn_blocking(move || capture_coordinate_for_path(&path))
                .await
                .unwrap_or_else(|_| CaptureCoordinateObservation::missing(None)),
            None => CaptureCoordinateObservation::missing(None),
        };
        let last_capture_offset = capture_coordinate.offset;
        let unread_bytes = relay_state_matches_inflight
            .then(|| last_capture_offset.map(|capture| capture.saturating_sub(last_relay_offset)))
            .flatten();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let relay_stale_anchor_ms = if last_relay_ts_ms > 0 {
            Some(last_relay_ts_ms)
        } else {
            inflight
                .as_ref()
                .and_then(|state| discord::inflight::parse_started_at_unix(&state.started_at))
                .and_then(|seconds: i64| seconds.checked_mul(1000))
        };
        let relay_stale = relay_stale_anchor_ms
            .map(|anchor_ms| now_ms.saturating_sub(anchor_ms) >= WATCHER_STATE_DESYNC_STALE_MS)
            .unwrap_or(false);
        let capture_lagged = last_capture_offset
            .map(|capture| {
                relay_state_matches_inflight
                    && inflight_state_present
                    && capture != last_relay_offset
                    && relay_stale
            })
            .unwrap_or(false);

        let enrichment = Self {
            inflight,
            attached,
            watcher_attached,
            watcher_attached_stale,
            has_relay_coord,
            watcher_owner_channel_id,
            watcher_output_path,
            tmux_session,
            inflight_state_present,
            tmux_session_mismatch,
            last_relay_offset,
            last_relay_ts_ms,
            reconnect_count,
            last_capture_offset,
            capture_coordinate,
            unread_bytes,
            relay_stale,
            capture_lagged,
            frontier_provenance: FrontierProvenance::observe(
                coord_observation,
                durable_observation,
            ),
        };
        enrichment.record_transcript_source_divergence(channel);
        enrichment
    }

    /// #5071 T4-B0 (#4987 S0): report the single shape the newly exposed
    /// [`Self::watcher_output_path`] exists to make visible — the live watcher
    /// and the in-flight row naming DIFFERENT transcripts for one channel.
    ///
    /// That is #4986: the watcher was on the provider-native TUI transcript and
    /// the row still held the prelaunch wrapper path, and health reported
    /// neither the split nor the watcher's file. This records both paths and
    /// the one #4987 discovery would pick. It decides nothing — no verdict, no
    /// recovery, no health field, and no destructive path reads it.
    ///
    /// Silent unless the two disagree, so a deployment that never hits the
    /// split logs nothing new. When they do disagree it fires once per load,
    /// i.e. once per health poll for as long as the split lasts: this slice
    /// adds no dedupe state, and #4987 S1 owns the deduplicated discovery
    /// record.
    fn record_transcript_source_divergence(&self, channel: ChannelId) {
        let (Some(registry_output_path), Some(inflight_output_path)) = (
            self.watcher_output_path.as_deref(),
            self.inflight
                .as_ref()
                .and_then(|state| state.output_path.as_deref()),
        ) else {
            return;
        };
        if registry_output_path == inflight_output_path {
            return;
        }
        tracing::info!(
            counter = "relay_transcript_source_divergence",
            channel_id = channel.get(),
            session_key = self.tmux_session.as_deref().unwrap_or_default(),
            registry_output_path,
            inflight_output_path,
            discovery_output_path =
                transcript_discovery_path(Some(registry_output_path), Some(inflight_output_path)),
            "live watcher and in-flight row name different transcripts for the same channel"
        );
    }

    pub async fn probe_tmux_session_alive(tmux_session: Option<&str>) -> Option<bool> {
        match tmux_session {
            Some(name) => {
                let probe_target = name.to_string();
                let liveness = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::pane_liveness(&probe_target)
                })
                .await
                .ok()?;
                liveness_as_alive(liveness)
            }
            None => None,
        }
    }

    pub fn tmux_session_present(&self) -> bool {
        self.tmux_session
            .as_deref()
            .is_some_and(crate::services::platform::tmux::has_session)
    }

    pub fn process_present(&self) -> bool {
        self.tmux_session
            .as_deref()
            .is_some_and(|name| crate::services::platform::tmux::pane_pid(name).is_some())
    }

    pub fn desynced(&self, live_tmux_present: bool, attached: bool) -> bool {
        let live_tmux_orphaned =
            live_tmux_present && self.inflight_state_present && !attached && self.relay_stale;
        self.capture_lagged || live_tmux_orphaned || self.tmux_session_mismatch
    }

    pub fn inflight_started_at(&self) -> Option<String> {
        self.inflight.as_ref().map(|state| state.started_at.clone())
    }

    pub fn inflight_updated_at(&self) -> Option<String> {
        self.inflight.as_ref().map(|state| state.updated_at.clone())
    }

    pub fn inflight_user_msg_id(&self) -> Option<u64> {
        super::redaction::visible_inflight_user_msg_id(self.inflight.as_ref())
    }

    pub fn inflight_current_msg_id(&self) -> Option<u64> {
        super::redaction::visible_inflight_current_msg_id(self.inflight.as_ref())
    }

    pub fn watcher_owns_live_relay(&self) -> bool {
        self.inflight
            .as_ref()
            .is_some_and(|state| state.watcher_owns_live_relay)
    }

    /// #3126: `true` when the in-flight row records a turn whose terminal
    /// assistant response has already been committed to the outbound delivery
    /// path (`terminal_delivery_committed`). Such a row is NOT an active
    /// provider turn — it is a completed turn whose session is now idle
    /// (e.g. waiting on a `ScheduleWakeup` or loop wind-down). The stall
    /// watchdog must not mistake this for a hung turn and force-clean it.
    pub fn inflight_terminal_delivery_committed(&self) -> bool {
        self.inflight
            .as_ref()
            .is_some_and(|state| state.terminal_delivery_committed)
    }

    pub fn active_dispatch_present(&self) -> bool {
        self.inflight
            .as_ref()
            .and_then(|state| state.dispatch_id.as_deref())
            .is_some()
    }
}

fn capture_coordinate_for_path(path: &str) -> CaptureCoordinateObservation {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let path_hash = hasher.finish();
    let Ok(metadata) = std::fs::metadata(path) else {
        return CaptureCoordinateObservation {
            offset: None,
            path_hash,
            file_id: None,
            status: CoordinateStatus::Missing,
        };
    };
    #[cfg(unix)]
    let file_id = {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let file_id = None;
    CaptureCoordinateObservation {
        offset: Some(metadata.len()),
        path_hash,
        file_id,
        status: CoordinateStatus::Observed,
    }
}

fn liveness_probe_session(inflight: Option<&str>, watcher: Option<&str>) -> Option<String> {
    inflight.or(watcher).map(str::to_string)
}

/// The one place [`SessionEnrichment`] reads #4987's priority-1 transcript
/// source off the registry binding.
///
/// Split out so a test can drive the real registry → `channel_binding` →
/// enrichment hop without standing up a `SharedData`, which is what makes a
/// binding that drops the path fail here rather than only inside the registry's
/// own suite.
fn watcher_output_path_from_binding(
    binding: Option<&discord::TmuxWatcherBinding>,
) -> Option<String> {
    binding.and_then(|binding| binding.output_path.clone())
}

/// #4987 §-1.1 R1: which transcript discovery would read for this channel.
///
/// The live watcher's path outranks the in-flight row's. They are separately
/// sourced — `watchers::lifecycle::claims` protects a watcher that has been
/// promoted to the provider-native TUI transcript from being demoted back to
/// the prelaunch wrapper file, and the row has no equivalent guard — so when
/// they disagree the watcher's is the one that followed the handoff.
///
/// Pure and read-only. #5071 T4-B0 uses it for the divergence record in
/// [`SessionEnrichment::record_transcript_source_divergence`] only; nothing
/// selects a file to read from it yet.
fn transcript_discovery_path<'a>(
    watcher_output_path: Option<&'a str>,
    inflight_output_path: Option<&'a str>,
) -> Option<&'a str> {
    watcher_output_path.or(inflight_output_path)
}

fn liveness_as_alive(liveness: PaneLiveness) -> Option<bool> {
    match liveness {
        PaneLiveness::Live => Some(true),
        PaneLiveness::DeadOrAbsent => Some(false),
        PaneLiveness::ProbeError => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflight_a_alive_watcher_b_dead_mismatch_probes_a() {
        let inflight_a_liveness = liveness_as_alive(PaneLiveness::Live);
        let watcher_b_liveness = liveness_as_alive(PaneLiveness::DeadOrAbsent);
        assert_eq!(
            liveness_probe_session(Some("inflight-a"), Some("watcher-b")),
            Some("inflight-a".to_string())
        );
        assert_eq!(inflight_a_liveness, Some(true));
        assert_eq!(watcher_b_liveness, Some(false));
    }

    #[test]
    fn only_exact_dead_or_absent_maps_to_dead() {
        assert_eq!(liveness_as_alive(PaneLiveness::DeadOrAbsent), Some(false));
        assert_eq!(liveness_as_alive(PaneLiveness::ProbeError), None);
    }

    const NATIVE_TRANSCRIPT: &str = "/tmp/agentdesk-b0-native.jsonl";
    const WRAPPER_TRANSCRIPT: &str = "/tmp/agentdesk-b0-wrapper.jsonl";

    fn watcher_handle(tmux_session_name: &str, output_path: &str) -> discord::TmuxWatcherHandle {
        discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: std::sync::Arc::new(std::sync::Mutex::new(None)),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                discord::tmux_watcher_now_ms(),
            )),
        }
    }

    fn registry_with_watcher(
        tmux_session_name: &str,
        channel: ChannelId,
        output_path: &str,
    ) -> discord::TmuxWatcherRegistry {
        let registry = discord::TmuxWatcherRegistry::new();
        registry.insert(channel, watcher_handle(tmux_session_name, output_path));
        registry
    }

    // #5071 T4-B0 (#4987 S0): the health load reaches the live watcher's
    // transcript through the channel binding. A binding that resolves the
    // channel but drops the path leaves the enrichment/load snapshot without
    // the watcher path coordinate again, so that omission must fail here and
    // not merely inside the registry's own suite.
    #[test]
    fn health_reads_the_live_watcher_transcript_off_the_channel_binding() {
        let tmux_session_name = "AgentDesk-claude-adk-b0-enrichment";
        let channel = ChannelId::new(5_071_000_000_000_040);
        let registry = registry_with_watcher(tmux_session_name, channel, NATIVE_TRANSCRIPT);

        let binding = registry.channel_binding(&channel);
        assert!(
            binding.is_some(),
            "the channel must resolve to a binding before the path can be read off it"
        );
        assert_eq!(
            watcher_output_path_from_binding(binding.as_ref()).as_deref(),
            Some(NATIVE_TRANSCRIPT),
            "the enrichment must carry the live handle's transcript, not None"
        );
    }

    /// #5071 relay-tail S1 r3 (legA P1): parking the offset at 0 and stamping
    /// the new wrapper's generation are two writes, so a poll can load the 0
    /// while the stamp is still the OLD one. Against a durable row from that
    /// same old incarnation the pair reads as H2 — a coordinate that never
    /// advanced while the row relayed in this incarnation — for a reset in
    /// flight.
    ///
    /// Driven through the raw triple rather than through a race, because the
    /// fence IS the second generation load: read the stamp once and the torn
    /// triple below either witnesses `OLD` and names H2, or witnesses `NEW`
    /// and stops being `None` — either way this fails.
    #[test]
    fn a_generation_stamp_moving_under_the_offset_read_witnesses_nothing() {
        use crate::services::discord::relay_health::FrontierHypothesis;

        const OLD_GENERATION_NS: i64 = 1_700_491_601_000_000_000;
        const NEW_GENERATION_NS: i64 = 1_700_491_777_000_000_000;
        // The row relayed under the wrapper the reset is retiring.
        let hypothesis_of = |triple| {
            let (coord_observation, live_generation_ns) = observe_frontier_triple(triple);
            FrontierProvenance::observe(
                coord_observation,
                DurableFrontierObservation::observe(
                    Some(4_096),
                    Some(OLD_GENERATION_NS),
                    live_generation_ns,
                ),
            )
            .hypothesis()
        };

        assert_eq!(
            observe_frontier_triple(Some((OLD_GENERATION_NS, 0, NEW_GENERATION_NS))),
            (CoordFrontierObservation::PresentZero, None),
            "a stamp that moved across the offset load witnesses nothing for that offset"
        );
        assert_eq!(
            hypothesis_of(Some((OLD_GENERATION_NS, 0, NEW_GENERATION_NS))),
            FrontierHypothesis::Indeterminate,
            "a reset caught in flight is not a coordinate that never advanced"
        );
        assert_eq!(
            hypothesis_of(Some((OLD_GENERATION_NS, 0, OLD_GENERATION_NS))),
            FrontierHypothesis::CoordNeverAdvancedWithDurableRow,
            "a stamp that held still across the load is the witness H2 is conditioned on"
        );
        assert_eq!(
            observe_frontier_triple(Some((0, 4_096, 0))),
            (CoordFrontierObservation::Advanced { offset: 4_096 }, None),
            "the atomic's never-stamped zero is no generation, agreeing with itself or not"
        );
        assert_eq!(
            observe_frontier_triple(None),
            (CoordFrontierObservation::Absent, None),
            "the map miss still becomes a reading in exactly one place"
        );
    }

    /// #5071 relay-tail S1 (I-4), design §9's S1 acceptance sentence taken on
    /// the PRODUCTION POLL PATH: `SessionEnrichment::load` is what the health
    /// poll calls, so that is where "coord 부재 폴이 `Absent`" has to hold.
    ///
    /// r1 review (legB P1-1): the lock used to live only on the pure
    /// `CoordFrontierObservation::observe`, and `load` reached the same decision
    /// down a line of its own — so collapsing the miss back into a zero THERE
    /// left the whole repository green. This drives `load` itself. Passing no
    /// provider keeps it off the inflight/transcript I/O and leaves the
    /// coordinate as the only witness under test.
    #[tokio::test]
    async fn the_health_poll_reports_an_absent_coordinate_as_absent() {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(5_071_000_000_000_043);

        let missed = SessionEnrichment::load(&shared, None, channel).await;
        assert_eq!(
            missed.frontier_provenance.coord_observation,
            CoordFrontierObservation::Absent,
            "a channel with no coordinate entry must poll as Absent, not as a zero frontier"
        );
        assert_eq!(
            missed.last_relay_offset, 0,
            "the tuple's unwrap_or zero is unchanged — S2 owns making it unknown"
        );
        assert!(
            !shared.tmux_relay_coords.contains_key(&channel),
            "the poll must not create the entry it failed to find"
        );

        let coord = std::sync::Arc::new(discord::TmuxRelayCoord::new(channel));
        shared.tmux_relay_coords.insert(channel, coord);
        let present = SessionEnrichment::load(&shared, None, channel).await;
        assert_eq!(
            present.frontier_provenance.coord_observation,
            CoordFrontierObservation::PresentZero,
            "an entry that never advanced is a different fact from no entry"
        );
        assert_eq!(
            present.last_relay_offset, missed.last_relay_offset,
            "both readings still produce the same frontier — only the provenance separates them"
        );
    }

    /// The same acceptance at the counterpart reader, which observes a channel
    /// no enrichment is loading. It shares [`read_coord_frontier`] with `load`,
    /// so this and the poll test above fail together.
    #[test]
    fn a_channel_with_no_coordinate_entry_polls_as_absent() {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(5_071_000_000_000_042);

        assert_eq!(
            observe_coord_frontier(&shared, channel),
            CoordFrontierObservation::Absent
        );
        assert!(
            !shared.tmux_relay_coords.contains_key(&channel),
            "observing must not create the entry it failed to find"
        );

        let coord = std::sync::Arc::new(discord::TmuxRelayCoord::new(channel));
        shared.tmux_relay_coords.insert(channel, coord.clone());
        assert_eq!(
            observe_coord_frontier(&shared, channel),
            CoordFrontierObservation::PresentZero,
            "an entry that never advanced is a different fact from no entry"
        );

        coord
            .confirmed_end_offset
            .store(4_096, std::sync::atomic::Ordering::Release);
        assert_eq!(
            observe_coord_frontier(&shared, channel),
            CoordFrontierObservation::Advanced { offset: 4_096 }
        );
    }

    #[test]
    fn health_reports_no_watcher_transcript_for_a_channel_without_a_binding() {
        let registry = discord::TmuxWatcherRegistry::new();
        let binding = registry.channel_binding(&ChannelId::new(5_071_000_000_000_041));
        assert!(binding.is_none());
        assert_eq!(watcher_output_path_from_binding(binding.as_ref()), None);
    }

    // #4986: the watcher had been promoted to the provider-native transcript
    // while the in-flight row still named the prelaunch wrapper file. Discovery
    // must pick the watcher's side of that split.
    #[test]
    fn transcript_discovery_prefers_the_live_watcher_over_the_inflight_row() {
        assert_eq!(
            transcript_discovery_path(Some(NATIVE_TRANSCRIPT), Some(WRAPPER_TRANSCRIPT)),
            Some(NATIVE_TRANSCRIPT)
        );
    }

    #[test]
    fn transcript_discovery_falls_back_to_the_inflight_row_and_then_to_nothing() {
        assert_eq!(
            transcript_discovery_path(None, Some(WRAPPER_TRANSCRIPT)),
            Some(WRAPPER_TRANSCRIPT),
            "a channel with no live watcher still discovers the row's transcript"
        );
        assert_eq!(
            transcript_discovery_path(Some(NATIVE_TRANSCRIPT), None),
            Some(NATIVE_TRANSCRIPT)
        );
        assert_eq!(transcript_discovery_path(None, None), None);
    }

    #[derive(Clone)]
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Install an INFO-level capturing subscriber for the duration of `run`.
    ///
    /// `set_default` is thread-local, and the divergence record is emitted on
    /// the same thread that polls the future, so every test using this must
    /// stay on `flavor = "current_thread"`.
    async fn capture_info<F, R>(run: F) -> (R, String)
    where
        F: std::future::Future<Output = R>,
    {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturingWriter(buffer.clone()))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let result = run.await;
        let output =
            String::from_utf8_lossy(&buffer.lock().unwrap_or_else(|poison| poison.into_inner()))
                .into_owned();
        (result, output)
    }

    fn divergence_record_count(logs: &str) -> usize {
        logs.lines()
            .filter(|line| line.contains("counter=\"relay_transcript_source_divergence\""))
            .count()
    }

    fn inflight_row_naming(
        provider: &ProviderKind,
        channel: ChannelId,
        tmux_session_name: &str,
        output_path: &str,
    ) {
        let state = discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            5_071_000_000_000_000,
            5_071_000_000_000_001,
            5_071_000_000_000_002,
            "t4-b0 divergence fixture".to_string(),
            None,
            Some(tmux_session_name.to_string()),
            Some(output_path.to_string()),
            None,
            0,
        );
        discord::inflight::save_inflight_state(&state).expect("persist inflight fixture");
    }

    // #5071 T4-B0 r2 (M2): `load` must actually carry the binding's transcript
    // into the enrichment. The helper-level tests above prove the binding hop
    // in isolation, so severing the one `load` line that consumes it — pinning
    // `watcher_output_path` to `None` — left them all green. This drives the
    // real `load` and fails on exactly that cut.
    #[tokio::test(flavor = "current_thread")]
    async fn health_load_carries_the_live_watcher_transcript_into_the_enrichment() {
        let channel = ChannelId::new(5_071_000_000_000_042);
        let tmux_session_name = "AgentDesk-claude-adk-b0-load-wiring";
        let shared = discord::make_shared_data_for_tests();
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session_name, NATIVE_TRANSCRIPT),
        );

        // No provider kind: the in-flight row stays out of this assertion so
        // the only surviving source for the field is the registry binding.
        let enrichment = SessionEnrichment::load(&shared, None, channel).await;

        assert!(
            enrichment.watcher_attached,
            "the fixture must resolve a live watcher binding for the channel"
        );
        assert_eq!(
            enrichment.watcher_output_path.as_deref(),
            Some(NATIVE_TRANSCRIPT),
            "load must carry the live watcher's transcript, not None"
        );
    }

    // #5071 T4-B0 r2 (M4): the divergence record must actually be emitted from
    // `load`. Deleting the `record_transcript_source_divergence` call left the
    // suite green because nothing observed the emit. Both halves matter — the
    // split fires exactly once, and the agreeing fixture stays silent, which is
    // what makes the record a divergence signal rather than a per-poll log.
    #[tokio::test(flavor = "current_thread")]
    async fn health_load_records_transcript_source_divergence_only_when_the_paths_split() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("temp runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_071_000_000_000_043);
        let tmux_session_name = "AgentDesk-codex-adk-b0-divergence";
        let shared = discord::make_shared_data_for_tests();
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session_name, NATIVE_TRANSCRIPT),
        );

        // #4986 shape: the watcher followed the provider-native transcript
        // while the row still names the prelaunch wrapper file.
        inflight_row_naming(&provider, channel, tmux_session_name, WRAPPER_TRANSCRIPT);
        let (diverged, diverged_logs) =
            capture_info(SessionEnrichment::load(&shared, Some(&provider), channel)).await;
        assert_eq!(
            diverged.watcher_output_path.as_deref(),
            Some(NATIVE_TRANSCRIPT)
        );
        assert_eq!(
            diverged
                .inflight
                .as_ref()
                .and_then(|state| state.output_path.as_deref()),
            Some(WRAPPER_TRANSCRIPT),
            "the fixture must present the two sources naming different files"
        );
        assert_eq!(
            divergence_record_count(&diverged_logs),
            1,
            "a split must emit exactly one divergence record per load; got:\n{diverged_logs}"
        );
        assert!(
            diverged_logs.contains(NATIVE_TRANSCRIPT) && diverged_logs.contains(WRAPPER_TRANSCRIPT),
            "the record must name both sides of the split; got:\n{diverged_logs}"
        );

        // Same channel, same watcher, row realigned onto the watcher's file.
        inflight_row_naming(&provider, channel, tmux_session_name, NATIVE_TRANSCRIPT);
        let (agreed, agreed_logs) =
            capture_info(SessionEnrichment::load(&shared, Some(&provider), channel)).await;
        assert_eq!(
            agreed
                .inflight
                .as_ref()
                .and_then(|state| state.output_path.as_deref()),
            Some(NATIVE_TRANSCRIPT),
            "the realigned fixture must be observed, otherwise the silence is vacuous"
        );
        assert_eq!(
            divergence_record_count(&agreed_logs),
            0,
            "agreeing sources must stay silent; got:\n{agreed_logs}"
        );
    }
}
