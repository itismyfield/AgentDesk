use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;
use std::time::Instant;

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::stall_liveness::{
    self, STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS, STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
    STALL_WATCHDOG_TOOL_PHASE_FRESHNESS_SECS,
};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

#[allow(dead_code)]
pub(in crate::services::discord) const VOUCH_TTL_SECS: u64 = 90;
const TRANSIENT_UNKNOWN_SECS: u64 = 60;
const CAPTURE_GRACE_TICKS: u8 = 3;

static MONO_ANCHOR: LazyLock<Instant> = LazyLock::new(Instant::now);
static VERDICTS: LazyLock<DashMap<(String, u64), ProducerLivenessVerdict>> =
    LazyLock::new(DashMap::new);
static CAPTURE_COORDINATES: LazyLock<DashMap<LivenessBinding, CaptureCoordinateState>> =
    LazyLock::new(DashMap::new);

#[cfg(test)]
static CAPTURE_OBSERVE_CALLS: LazyLock<DashMap<LivenessBinding, usize>> =
    LazyLock::new(DashMap::new);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::services::discord) struct LivenessBinding {
    pub(in crate::services::discord) provider: String,
    pub(in crate::services::discord) channel_id: u64,
    pub(in crate::services::discord) tmux_session_name: Option<String>,
    pub(in crate::services::discord) user_msg_id: Option<u64>,
    pub(in crate::services::discord) started_at: Option<String>,
}

impl LivenessBinding {
    fn from_snapshot(
        provider: &ProviderKind,
        channel_id: ChannelId,
        snapshot: &WatcherStateSnapshot,
    ) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
            tmux_session_name: snapshot.tmux_session.clone(),
            user_msg_id: snapshot.inflight_user_msg_id,
            started_at: snapshot.inflight_started_at.clone(),
        }
    }

    fn matches_inflight(&self, authoritative: &InflightTurnState) -> bool {
        self.user_msg_id == Some(authoritative.user_msg_id)
            && self.started_at.as_deref() == Some(authoritative.started_at.as_str())
            && self.tmux_session_name == authoritative.tmux_session_name
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum CoordinateStatus {
    Missing,
    Observed,
    MissingAfterObserved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct CaptureCoordinateObservation {
    pub(in crate::services::discord) offset: Option<u64>,
    pub(in crate::services::discord) path_hash: u64,
    pub(in crate::services::discord) file_id: Option<(u64, u64)>,
    pub(in crate::services::discord) status: CoordinateStatus,
}

impl CaptureCoordinateObservation {
    pub(in crate::services::discord) fn missing(path: Option<&str>) -> Self {
        Self {
            offset: None,
            path_hash: path_hash(path),
            file_id: None,
            status: CoordinateStatus::Missing,
        }
    }
}

#[derive(Clone, Debug)]
struct CaptureCoordinateState {
    observation: CaptureCoordinateObservation,
    observed_advancing_before: bool,
    consecutive_non_advancing_ticks: u8,
    consecutive_missing_ticks: u8,
    advanced_at_unix_secs: Option<i64>,
    last_observed_at_unix_secs: i64,
    unknown_since_mono_secs: Option<i64>,
    _rebase_pending: bool,
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct CaptureAssessment {
    pub(in crate::services::discord) observation: CaptureCoordinateObservation,
    pub(in crate::services::discord) advancing: bool,
    pub(in crate::services::discord) transient_unknown: bool,
    pub(in crate::services::discord) advanced_at_unix_secs: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum ProducerLivenessClass {
    ProvenAlive,
    NoEvidence,
    TransientUnknown,
    AbsoluteBackstop,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::services::discord) struct LivenessReasons {
    pub(in crate::services::discord) capture_advancing: bool,
    pub(in crate::services::discord) open_tool_age_secs: Option<u64>,
    pub(in crate::services::discord) transcript_mtime_age_secs: Option<u64>,
    pub(in crate::services::discord) outbound_activity_age_secs: Option<u64>,
}

impl LivenessReasons {
    fn csv(&self) -> String {
        let mut reasons = Vec::new();
        if self.capture_advancing {
            reasons.push("capture_advancing");
        }
        if recent(
            self.open_tool_age_secs,
            STALL_WATCHDOG_TOOL_PHASE_FRESHNESS_SECS,
        ) {
            reasons.push("open_tool_execution_recent");
        }
        if recent(
            self.transcript_mtime_age_secs,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
        ) {
            reasons.push("transcript_mtime_recent");
        }
        if recent(
            self.outbound_activity_age_secs,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
        ) {
            reasons.push("outbound_activity_recent");
        }
        if reasons.is_empty() {
            "none".to_string()
        } else {
            reasons.join(",")
        }
    }

    fn proves_alive(&self) -> bool {
        self.capture_advancing
            || recent(
                self.open_tool_age_secs,
                STALL_WATCHDOG_TOOL_PHASE_FRESHNESS_SECS,
            )
            || recent(
                self.transcript_mtime_age_secs,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            )
            || recent(
                self.outbound_activity_age_secs,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            )
    }
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct ProducerLivenessVerdict {
    pub(in crate::services::discord) binding: LivenessBinding,
    pub(in crate::services::discord) class: ProducerLivenessClass,
    pub(in crate::services::discord) reasons: LivenessReasons,
    pub(in crate::services::discord) coordinate: CaptureCoordinateObservation,
    pub(in crate::services::discord) raw_turn_age_secs: Option<u64>,
    pub(in crate::services::discord) unknown_since_mono_secs: Option<i64>,
    pub(in crate::services::discord) published_at_mono_secs: i64,
    pub(in crate::services::discord) published_at_unix_secs: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum VouchDenial {
    Missing,
    Stale,
    IdentityMismatch,
    NoEvidence,
    PastAbsoluteCeiling,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum LivenessVouch {
    Vouched {
        reasons_csv: String,
        published_age_secs: u64,
    },
    DeferTransient {
        unknown_age_secs: u64,
    },
    NotVouched {
        reason: VouchDenial,
    },
}

#[allow(dead_code)]
pub(in crate::services::discord) trait LivenessAuthorityRead:
    Send + Sync
{
    fn vouch_for_inflight(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        authoritative: &InflightTurnState,
        now_mono_secs: i64,
    ) -> LivenessVouch;
}

#[allow(dead_code)]
pub(in crate::services::discord) struct ProcessLocalLivenessAuthority;

impl LivenessAuthorityRead for ProcessLocalLivenessAuthority {
    fn vouch_for_inflight(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        authoritative: &InflightTurnState,
        now_mono_secs: i64,
    ) -> LivenessVouch {
        vouch_for_inflight(provider, channel_id, authoritative, now_mono_secs)
    }
}

pub(in crate::services::discord) fn monotonic_now_secs() -> i64 {
    MONO_ANCHOR.elapsed().as_secs() as i64
}

pub(in crate::services::discord) fn observe_capture_coordinate(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
    now_mono_secs: i64,
) -> CaptureAssessment {
    let binding = LivenessBinding::from_snapshot(provider, channel_id, snapshot);
    #[cfg(test)]
    CAPTURE_OBSERVE_CALLS
        .entry(binding.clone())
        .and_modify(|calls| *calls += 1)
        .or_insert(1);
    let current = snapshot.capture_coordinate.clone();
    let previous = CAPTURE_COORDINATES.get(&binding).map(|entry| entry.clone());
    let same_coordinate = previous.as_ref().is_some_and(|state| {
        state.observation.path_hash == current.path_hash
            && state.observation.file_id == current.file_id
    });
    // S1 deliberately preserves the main watchdog's size-only tick semantics:
    // path/inode changes and observed decreases are recorded as discontinuities
    // but do not alter advancement or grace accounting. Consequently an in-tick
    // same-inode truncate followed by append past the old size is not detectable;
    // capture-generation hardening belongs to a follow-up slice. Verdict entries
    // are likewise process-local and bounded by provider/channel cardinality; GC
    // is deferred until that follow-up rather than expanding S1's behavior.
    let observed_advance = matches!(
        (
            previous
                .as_ref()
                .and_then(|state| state.observation.offset),
            current.offset
        ),
        (Some(before), Some(after)) if after > before
    );
    let discontinuity = previous.as_ref().is_some_and(|state| {
        !same_coordinate
            || matches!((state.observation.offset, current.offset), (Some(before), Some(after)) if after < before)
            || (state.observation.offset.is_some() && current.offset.is_none())
    });
    let observed_advancing_before = observed_advance
        || previous
            .as_ref()
            .is_some_and(|state| state.observed_advancing_before);
    let consecutive_non_advancing_ticks = if observed_advance {
        0
    } else {
        previous
            .as_ref()
            .map(|state| state.consecutive_non_advancing_ticks)
            .unwrap_or(0)
            .saturating_add(1)
    };
    let consecutive_missing_ticks = if current.offset.is_none() {
        previous
            .as_ref()
            .map(|state| state.consecutive_missing_ticks)
            .unwrap_or(0)
            .saturating_add(1)
    } else {
        0
    };
    let missing_after_observed = previous
        .as_ref()
        .is_some_and(|state| state.observation.offset.is_some())
        && current.offset.is_none();
    let mut observation = current.clone();
    if missing_after_observed {
        observation.status = CoordinateStatus::MissingAfterObserved;
    }
    let transient_unknown =
        observation.offset.is_none() && !missing_after_observed && consecutive_missing_ticks == 1;
    let unknown_since_mono_secs = if transient_unknown {
        Some(now_mono_secs)
    } else {
        previous
            .as_ref()
            .and_then(|state| state.unknown_since_mono_secs)
    };
    let advanced_at_unix_secs = if observed_advance {
        Some(now_unix_secs)
    } else {
        previous
            .as_ref()
            .and_then(|state| state.advanced_at_unix_secs)
    };
    let advancing = observed_advance
        || (observed_advancing_before && consecutive_non_advancing_ticks < CAPTURE_GRACE_TICKS);

    CAPTURE_COORDINATES.insert(
        binding,
        CaptureCoordinateState {
            observation: observation.clone(),
            observed_advancing_before,
            consecutive_non_advancing_ticks,
            consecutive_missing_ticks,
            advanced_at_unix_secs,
            last_observed_at_unix_secs: now_unix_secs,
            unknown_since_mono_secs,
            _rebase_pending: discontinuity,
        },
    );
    CaptureAssessment {
        observation,
        advancing,
        transient_unknown,
        advanced_at_unix_secs,
    }
}

pub(in crate::services::discord) fn observe_and_publish_from_tick(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    inflight: Option<&InflightTurnState>,
    now_unix_secs: i64,
    now_mono_secs: i64,
) -> CaptureAssessment {
    let capture =
        observe_capture_coordinate(provider, channel_id, snapshot, now_unix_secs, now_mono_secs);
    publish_from_tick(
        provider,
        channel_id,
        snapshot,
        inflight,
        &capture,
        now_unix_secs,
        now_mono_secs,
    );
    capture
}

fn publish_from_tick(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    inflight: Option<&InflightTurnState>,
    capture: &CaptureAssessment,
    now_unix_secs: i64,
    now_mono_secs: i64,
) {
    let reasons = LivenessReasons {
        capture_advancing: capture.advancing,
        open_tool_age_secs: stall_liveness::open_tool_execution_age_secs(
            snapshot,
            inflight,
            now_unix_secs,
        ),
        transcript_mtime_age_secs: stall_liveness::transcript_mtime_age_secs(
            inflight,
            now_unix_secs,
        ),
        outbound_activity_age_secs: stall_liveness::unix_millis_age_secs(
            snapshot.relay_health.last_outbound_activity_ms,
            now_unix_secs,
        ),
    };
    let raw_turn_age_secs = snapshot
        .inflight_started_at
        .as_deref()
        .and_then(crate::services::discord::inflight::parse_started_at_unix)
        .map(|started| now_unix_secs.saturating_sub(started).max(0) as u64);
    let class = if raw_turn_age_secs.is_some_and(|age| age >= STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS)
    {
        ProducerLivenessClass::AbsoluteBackstop
    } else if reasons.proves_alive() {
        ProducerLivenessClass::ProvenAlive
    } else if capture.transient_unknown {
        ProducerLivenessClass::TransientUnknown
    } else {
        ProducerLivenessClass::NoEvidence
    };
    let verdict = ProducerLivenessVerdict {
        binding: LivenessBinding::from_snapshot(provider, channel_id, snapshot),
        class,
        reasons,
        coordinate: capture.observation.clone(),
        raw_turn_age_secs,
        unknown_since_mono_secs: capture.transient_unknown.then_some(now_mono_secs),
        published_at_mono_secs: now_mono_secs,
        published_at_unix_secs: now_unix_secs,
    };
    store_verdict((provider.as_str().to_string(), channel_id.get()), verdict);
}

fn store_verdict(key: (String, u64), verdict: ProducerLivenessVerdict) {
    match VERDICTS.entry(key) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if entry.get().published_at_mono_secs <= verdict.published_at_mono_secs {
                entry.insert(verdict);
            }
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(verdict);
        }
    }
}

#[allow(dead_code)]
pub(in crate::services::discord) fn vouch_for_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    authoritative: &InflightTurnState,
    now_mono_secs: i64,
) -> LivenessVouch {
    let Some(verdict) = VERDICTS
        .get(&(provider.as_str().to_string(), channel_id.get()))
        .map(|entry| entry.clone())
    else {
        return LivenessVouch::NotVouched {
            reason: VouchDenial::Missing,
        };
    };
    let published_age_secs = now_mono_secs
        .saturating_sub(verdict.published_at_mono_secs)
        .max(0) as u64;
    if published_age_secs > VOUCH_TTL_SECS {
        return LivenessVouch::NotVouched {
            reason: VouchDenial::Stale,
        };
    }
    if !verdict.binding.matches_inflight(authoritative) {
        return LivenessVouch::NotVouched {
            reason: VouchDenial::IdentityMismatch,
        };
    }
    if verdict
        .raw_turn_age_secs
        .is_some_and(|age| age >= STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS)
    {
        return LivenessVouch::NotVouched {
            reason: VouchDenial::PastAbsoluteCeiling,
        };
    }
    match verdict.class {
        ProducerLivenessClass::ProvenAlive => LivenessVouch::Vouched {
            reasons_csv: verdict.reasons.csv(),
            published_age_secs,
        },
        ProducerLivenessClass::TransientUnknown => {
            let unknown_age_secs = now_mono_secs
                .saturating_sub(verdict.unknown_since_mono_secs.unwrap_or(now_mono_secs))
                .max(0) as u64;
            if unknown_age_secs <= TRANSIENT_UNKNOWN_SECS {
                LivenessVouch::DeferTransient { unknown_age_secs }
            } else {
                LivenessVouch::NotVouched {
                    reason: VouchDenial::NoEvidence,
                }
            }
        }
        ProducerLivenessClass::NoEvidence => LivenessVouch::NotVouched {
            reason: VouchDenial::NoEvidence,
        },
        ProducerLivenessClass::AbsoluteBackstop => LivenessVouch::NotVouched {
            reason: VouchDenial::PastAbsoluteCeiling,
        },
    }
}

pub(in crate::services::discord) fn clear_capture_state_for_session(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session: Option<&str>,
) {
    CAPTURE_COORDINATES.retain(|binding, _| {
        binding.provider != provider.as_str()
            || binding.channel_id != channel_id.get()
            || binding.tmux_session_name.as_deref() != tmux_session
    });
}

pub(in crate::services::discord) fn gc_capture_state(now_unix_secs: i64, ttl_secs: u64) {
    CAPTURE_COORDINATES.retain(|_, state| {
        now_unix_secs
            .saturating_sub(state.last_observed_at_unix_secs)
            .max(0) as u64
            <= ttl_secs
    });
}

pub(in crate::services::discord) fn capture_advanced_age_secs(
    provider: &str,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
) -> Option<u64> {
    let binding = LivenessBinding {
        provider: provider.to_string(),
        channel_id: channel_id.get(),
        tmux_session_name: snapshot.tmux_session.clone(),
        user_msg_id: snapshot.inflight_user_msg_id,
        started_at: snapshot.inflight_started_at.clone(),
    };
    CAPTURE_COORDINATES
        .get(&binding)
        .and_then(|state| state.advanced_at_unix_secs)
        .map(|advanced| now_unix_secs.saturating_sub(advanced).max(0) as u64)
}

fn recent(age: Option<u64>, budget: u64) -> bool {
    age.is_some_and(|age| age <= budget)
}

fn path_hash(path: Option<&str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.unwrap_or("").hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinate(
        offset: Option<u64>,
        path: &str,
        file_id: Option<(u64, u64)>,
    ) -> CaptureCoordinateObservation {
        CaptureCoordinateObservation {
            offset,
            path_hash: path_hash(Some(path)),
            file_id,
            status: if offset.is_some() {
                CoordinateStatus::Observed
            } else {
                CoordinateStatus::Missing
            },
        }
    }

    fn binding(channel_id: u64) -> LivenessBinding {
        LivenessBinding {
            provider: ProviderKind::Codex.as_str().to_string(),
            channel_id,
            tmux_session_name: Some(format!("tmux-{channel_id}")),
            user_msg_id: Some(7),
            started_at: Some("2026-07-20 00:00:00".to_string()),
        }
    }

    fn snapshot_for_test(
        key: &LivenessBinding,
        observation: CaptureCoordinateObservation,
    ) -> WatcherStateSnapshot {
        use crate::services::discord::relay_health::{
            RelayActiveTurn, RelayHealthSnapshot, RelayStallState,
        };

        WatcherStateSnapshot {
            provider: key.provider.clone(),
            attached: true,
            tmux_session: key.tmux_session_name.clone(),
            watcher_owner_channel_id: Some(key.channel_id),
            last_relay_offset: 0,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: observation.offset,
            capture_coordinate: observation,
            unread_bytes: None,
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: key.started_at.clone(),
            inflight_updated_at: key.started_at.clone(),
            inflight_user_msg_id: key.user_msg_id,
            inflight_current_msg_id: Some(8),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: key.user_msg_id,
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: None,
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: key.provider.clone(),
                channel_id: key.channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: key.tmux_session_name.clone(),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(key.channel_id),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(8),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: key.user_msg_id,
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(8),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_outbound_activity_ms: None,
                last_capture_offset: None,
                last_relay_offset: 0,
                unread_bytes: None,
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    fn observe_for_test(
        key: &LivenessBinding,
        current: CaptureCoordinateObservation,
        now_unix: i64,
        now_mono: i64,
    ) -> CaptureAssessment {
        let snapshot = snapshot_for_test(key, current);
        observe_capture_coordinate(
            &ProviderKind::Codex,
            ChannelId::new(key.channel_id),
            &snapshot,
            now_unix,
            now_mono,
        )
    }

    #[test]
    fn capture_decrease_preserves_main_two_tick_grace() {
        let key = binding(461_514);
        CAPTURE_COORDINATES.remove(&key);
        assert!(!observe_for_test(&key, coordinate(Some(100), "a", Some((1, 2))), 1, 1).advancing);
        assert!(observe_for_test(&key, coordinate(Some(200), "a", Some((1, 2))), 2, 2).advancing);
        assert!(observe_for_test(&key, coordinate(Some(0), "a", Some((1, 2))), 3, 3).advancing);
    }

    #[test]
    fn capture_some_to_none_records_disappearance_and_preserves_main_grace() {
        let key = binding(461_515);
        CAPTURE_COORDINATES.remove(&key);
        observe_for_test(&key, coordinate(Some(10), "a", Some((1, 2))), 1, 1);
        assert!(observe_for_test(&key, coordinate(Some(20), "a", Some((1, 2))), 2, 2).advancing);
        let missing = observe_for_test(&key, coordinate(None, "a", None), 3, 3);
        assert!(missing.advancing);
        assert_eq!(
            missing.observation.status,
            CoordinateStatus::MissingAfterObserved
        );
        assert!(!missing.transient_unknown);
    }

    #[test]
    fn bounded_none_transient_then_fail_closed() {
        let key = binding(461_516);
        CAPTURE_COORDINATES.remove(&key);
        assert!(observe_for_test(&key, coordinate(None, "a", None), 1, 1).transient_unknown);
        assert!(!observe_for_test(&key, coordinate(None, "a", None), 2, 2).transient_unknown);
    }

    fn authoritative_inflight(key: &LivenessBinding) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            key.channel_id,
            None,
            1,
            key.user_msg_id.unwrap_or(7),
            8,
            "test".to_string(),
            Some("session".to_string()),
            key.tmux_session_name.clone(),
            None,
            None,
            0,
        );
        state.started_at = key.started_at.clone().unwrap();
        state
    }

    #[test]
    fn vouch_enforces_ttl_identity_and_absolute_ceiling() {
        let key = binding(461_503);
        let store_key = (key.provider.clone(), key.channel_id);
        let base = ProducerLivenessVerdict {
            binding: key.clone(),
            class: ProducerLivenessClass::ProvenAlive,
            reasons: LivenessReasons {
                capture_advancing: true,
                ..Default::default()
            },
            coordinate: coordinate(Some(2), "a", Some((1, 2))),
            raw_turn_age_secs: Some(1),
            unknown_since_mono_secs: None,
            published_at_mono_secs: 100,
            published_at_unix_secs: 100,
        };
        store_verdict(store_key.clone(), base.clone());
        let inflight = authoritative_inflight(&key);
        assert!(matches!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &inflight,
                190
            ),
            LivenessVouch::Vouched { .. }
        ));
        assert_eq!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &inflight,
                191
            ),
            LivenessVouch::NotVouched {
                reason: VouchDenial::Stale
            }
        );

        let mut mismatch = inflight.clone();
        mismatch.user_msg_id += 1;
        assert_eq!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &mismatch,
                190
            ),
            LivenessVouch::NotVouched {
                reason: VouchDenial::IdentityMismatch
            }
        );

        store_verdict(
            store_key,
            ProducerLivenessVerdict {
                published_at_mono_secs: 200,
                raw_turn_age_secs: Some(STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS),
                ..base
            },
        );
        assert_eq!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &inflight,
                200
            ),
            LivenessVouch::NotVouched {
                reason: VouchDenial::PastAbsoluteCeiling
            }
        );
    }

    #[test]
    fn publish_first_tick_stateless_open_tool_vouches() {
        let key = binding(461_511);
        let store_key = (key.provider.clone(), key.channel_id);
        VERDICTS.remove(&store_key);
        CAPTURE_COORDINATES.remove(&key);
        let snapshot = snapshot_for_test(&key, coordinate(Some(10), "a", Some((1, 2))));
        let mut inflight = authoritative_inflight(&key);
        inflight.current_tool_line = Some("Bash: long-running command".to_string());
        inflight.has_post_tool_text = false;
        inflight.updated_at = key.started_at.clone().unwrap();
        let now_unix_secs =
            crate::services::discord::inflight::parse_updated_at_unix(&inflight.updated_at)
                .unwrap()
                + 10;

        observe_and_publish_from_tick(
            &ProviderKind::Codex,
            ChannelId::new(key.channel_id),
            &snapshot,
            Some(&inflight),
            now_unix_secs,
            10,
        );

        assert!(matches!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &inflight,
                10
            ),
            LivenessVouch::Vouched { ref reasons_csv, .. }
                if reasons_csv == "open_tool_execution_recent"
        ));
    }

    #[test]
    fn transient_unknown_expires_after_monotonic_budget() {
        let key = binding(461_512);
        let store_key = (key.provider.clone(), key.channel_id);
        VERDICTS.remove(&store_key);
        let inflight = authoritative_inflight(&key);
        store_verdict(
            store_key,
            ProducerLivenessVerdict {
                binding: key.clone(),
                class: ProducerLivenessClass::TransientUnknown,
                reasons: LivenessReasons::default(),
                coordinate: coordinate(None, "a", None),
                raw_turn_age_secs: Some(1),
                unknown_since_mono_secs: Some(100),
                published_at_mono_secs: 100,
                published_at_unix_secs: 100,
            },
        );

        assert_eq!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &inflight,
                160
            ),
            LivenessVouch::DeferTransient {
                unknown_age_secs: 60
            }
        );
        assert_eq!(
            vouch_for_inflight(
                &ProviderKind::Codex,
                ChannelId::new(key.channel_id),
                &inflight,
                161
            ),
            LivenessVouch::NotVouched {
                reason: VouchDenial::NoEvidence
            }
        );
    }

    #[test]
    fn publish_precedes_relay_dead_reattach_in_health_tick() {
        let recovery = include_str!("recovery.rs");
        let publish = recovery
            .find("observe_and_publish_from_tick(")
            .expect("health tick must publish a liveness verdict");
        let relay_dead_reattach = recovery
            .find("relay_dead_reattach::try_apply(")
            .expect("health tick must retain relay-dead reattach");
        assert!(publish < relay_dead_reattach);
    }

    #[test]
    fn single_observe_per_tick() {
        let key = binding(461_510);
        CAPTURE_COORDINATES.remove(&key);
        CAPTURE_OBSERVE_CALLS.remove(&key);
        observe_for_test(&key, coordinate(Some(1), "a", Some((1, 2))), 1, 1);
        assert_eq!(CAPTURE_OBSERVE_CALLS.get(&key).map(|calls| *calls), Some(1));
    }

    #[test]
    fn publish_monotonic_regression_ignored() {
        let key = (ProviderKind::Codex.as_str().to_string(), 461_513);
        VERDICTS.remove(&key);
        let original = ProducerLivenessVerdict {
            binding: binding(461_513),
            class: ProducerLivenessClass::ProvenAlive,
            reasons: LivenessReasons {
                capture_advancing: true,
                ..Default::default()
            },
            coordinate: coordinate(Some(2), "a", Some((1, 2))),
            raw_turn_age_secs: Some(1),
            unknown_since_mono_secs: None,
            published_at_mono_secs: 20,
            published_at_unix_secs: 20,
        };
        store_verdict(key.clone(), original);
        let stale = ProducerLivenessVerdict {
            published_at_mono_secs: 19,
            class: ProducerLivenessClass::NoEvidence,
            ..VERDICTS.get(&key).unwrap().clone()
        };
        store_verdict(key.clone(), stale);
        assert_eq!(
            VERDICTS.get(&key).unwrap().class,
            ProducerLivenessClass::ProvenAlive
        );
    }
}
