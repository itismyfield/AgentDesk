use std::path::Path;
use std::time::Duration;

// Child module (file lives alongside in discord/) — declared here instead of
// the ratcheted discord/mod.rs; this gate is its only consumer.
#[path = "destructive_cancel_liveness.rs"]
mod destructive_cancel_liveness;
use super::{SharedData, inflight, mailbox_snapshot};
use destructive_cancel_liveness::{
    RelayForfeitArm, WatcherRelayLivenessEvidence, fresh_watcher_heartbeat_blocks_rebind,
    relay_forfeit_arm, relay_liveness_forfeited, turn_age_secs,
};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::json;

use crate::services::provider::ProviderKind;

#[cfg(not(test))]
const DESTRUCTIVE_CANCEL_REPROBE_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const DESTRUCTIVE_CANCEL_REPROBE_DELAY: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const DESTRUCTIVE_CANCEL_REPROBE_ATTEMPTS: usize = 3;
#[cfg(test)]
const DESTRUCTIVE_CANCEL_REPROBE_ATTEMPTS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct DestructiveCancelIdentityPin {
    pub finalizer_turn_id: u64,
    pub mailbox_active_user_msg_id: Option<u64>,
    pub tmux_session_name: Option<String>,
}

impl DestructiveCancelIdentityPin {
    pub(in crate::services::discord) fn from_state(
        state: &inflight::InflightTurnState,
        mailbox_active_user_msg_id: Option<u64>,
    ) -> Self {
        Self {
            finalizer_turn_id: state.effective_finalizer_turn_id(),
            mailbox_active_user_msg_id,
            tmux_session_name: state.tmux_session_name.clone(),
        }
    }

    pub(in crate::services::discord) fn matches_state(
        &self,
        state: &inflight::InflightTurnState,
    ) -> bool {
        self.finalizer_turn_id == state.effective_finalizer_turn_id()
            && self.tmux_session_name == state.tmux_session_name
    }
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct DestructiveCancelProbeSnapshot {
    pub pin: DestructiveCancelIdentityPin,
    pub inflight_identity: inflight::InflightTurnIdentity,
    pub updated_at: String,
    pub save_generation: u64,
    pub output_path: Option<String>,
    pub output_len: Option<u64>,
    pub relay_frontier: Option<u64>,
}

impl DestructiveCancelProbeSnapshot {
    pub(in crate::services::discord) fn from_state(
        shared: &SharedData,
        state: &inflight::InflightTurnState,
        mailbox_active_user_msg_id: Option<u64>,
        watcher_owner_channel: ChannelId,
    ) -> Self {
        let pin = DestructiveCancelIdentityPin::from_state(state, mailbox_active_user_msg_id);
        Self::from_pinned_state(shared, state, pin, watcher_owner_channel)
    }

    pub(in crate::services::discord) fn from_pinned_state(
        shared: &SharedData,
        state: &inflight::InflightTurnState,
        pin: DestructiveCancelIdentityPin,
        watcher_owner_channel: ChannelId,
    ) -> Self {
        let output_path = state
            .output_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string);
        let output_len = output_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len());
        let relay_frontier = relay_frontier_for_current_generation(
            shared,
            watcher_owner_channel,
            pin.tmux_session_name.as_deref(),
        );
        Self {
            pin,
            inflight_identity: inflight::InflightTurnIdentity::from_state(state),
            updated_at: state.updated_at.clone(),
            save_generation: state.save_generation,
            output_path,
            output_len,
            relay_frontier,
        }
    }
}

/// #4353: the frontier lives in tmux session files, and `super::tmux` is
/// `cfg(unix)`. Without a session there is no frontier, which is exactly the
/// answer on a platform that cannot host one.
#[cfg(unix)]
fn relay_frontier_for_current_generation(
    shared: &SharedData,
    watcher_owner_channel: ChannelId,
    tmux_session_name: Option<&str>,
) -> Option<u64> {
    tmux_session_name.and_then(|tmux_session_name| {
        super::tmux::committed_frontier_for_current_generation(
            shared,
            watcher_owner_channel,
            tmux_session_name,
        )
    })
}

#[cfg(not(unix))]
fn relay_frontier_for_current_generation(
    _shared: &SharedData,
    _watcher_owner_channel: ChannelId,
    _tmux_session_name: Option<&str>,
) -> Option<u64> {
    None
}

pub(in crate::services::discord) fn terminal_envelope_present(
    provider: &ProviderKind,
    snapshot: &DestructiveCancelProbeSnapshot,
) -> bool {
    snapshot.output_path.as_deref().is_some_and(|path| {
        crate::services::tui_turn_state::jsonl_turn_end_terminator_idle(provider, Path::new(path))
    })
}

fn liveness_evidence<'a>(
    shared: &SharedData,
    watcher_owner_channel: ChannelId,
    snapshot: &DestructiveCancelProbeSnapshot,
    state: &'a inflight::InflightTurnState,
    watcher_output_path: &str,
) -> WatcherRelayLivenessEvidence<'a> {
    let now_unix = inflight::now_unix();
    let output_len_now = std::fs::metadata(watcher_output_path)
        .ok()
        .map(|metadata| metadata.len());
    let output_len_at_snapshot = snapshot
        .output_path
        .as_deref()
        .filter(|path| Path::new(path) == Path::new(watcher_output_path))
        .and(snapshot.output_len);
    WatcherRelayLivenessEvidence {
        output_len_at_snapshot,
        output_len_now,
        output_mtime_age_secs: output_mtime_age_secs(watcher_output_path),
        relay_frontier_at_snapshot: snapshot.relay_frontier,
        relay_frontier_now: relay_frontier_for_current_generation(
            shared,
            watcher_owner_channel,
            snapshot.pin.tmux_session_name.as_deref(),
        ),
        last_watcher_relayed_offset: state.last_watcher_relayed_offset,
        last_watcher_relayed_at_unix: state.last_watcher_relayed_at_unix,
        terminal_delivery_committed: state.terminal_delivery_committed,
        full_response: &state.full_response,
        response_sent_offset: state.response_sent_offset,
        prior_delivery_evidence: prior_delivery_evidence(state),
        turn_age_secs: turn_age_secs(state, now_unix),
        now_unix,
    }
}

fn prior_delivery_evidence(state: &inflight::InflightTurnState) -> bool {
    state.last_watcher_relayed_offset.is_some()
        || state.last_watcher_relayed_at_unix.is_some()
        || state.session_bound_delivered
        || state.anchor_reposted
        || !state.streaming_rollover_frozen_msg_ids.is_empty()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayLivenessForfeitSeam {
    FreshWatcherHeartbeat,
    CaptureProgressOnReprobe,
}

impl RelayLivenessForfeitSeam {
    fn as_str(self) -> &'static str {
        match self {
            Self::FreshWatcherHeartbeat => "fresh_watcher_heartbeat",
            Self::CaptureProgressOnReprobe => "capture_progress_on_reprobe",
        }
    }
}

fn relay_liveness_forfeit_decision(
    evidence: WatcherRelayLivenessEvidence<'_>,
    provider: &ProviderKind,
    channel: ChannelId,
    pin: &DestructiveCancelIdentityPin,
    seam: RelayLivenessForfeitSeam,
) -> bool {
    let would_forfeit = relay_liveness_forfeited(evidence);
    if would_forfeit {
        let arm = relay_forfeit_arm(evidence.prior_delivery_evidence);
        let turn_id = pin.finalizer_turn_id.to_string();
        // Observation-only until #5007 E0 (identity-locked final verification)
        // and E1 (atomic verify-and-cancel) land. Never enable destructive allow
        // from this signal before both prerequisites are deployed.
        let _ = crate::services::observability::record_invariant_check_with_severity(
            false,
            crate::services::observability::InvariantViolation {
                provider: Some(provider.as_str()),
                channel_id: Some(channel.get()),
                dispatch_id: None,
                session_key: pin.tmux_session_name.as_deref(),
                turn_id: Some(turn_id.as_str()),
                invariant: "relay_liveness_would_forfeit",
                code_location: "destructive_cancel_gate.rs:relay_liveness_forfeit_decision",
                message: "relay liveness model would forfeit, but destructive use is disabled pending #5007 E0+E1",
                details: json!({
                    "channel_id": channel.get(),
                    "tmux_session_name": pin.tmux_session_name.as_deref(),
                    "turn_pin": {
                        "finalizer_turn_id": pin.finalizer_turn_id,
                        "mailbox_active_user_msg_id": pin.mailbox_active_user_msg_id,
                    },
                    "seam": seam.as_str(),
                    "arm": match arm {
                        RelayForfeitArm::ZeroDelivery => "zero_delivery",
                        RelayForfeitArm::StalledDelivery => "stalled_delivery",
                    },
                    "evidence": {
                        "output_len_at_snapshot": evidence.output_len_at_snapshot,
                        "output_len_now": evidence.output_len_now,
                        "output_mtime_age_secs": evidence.output_mtime_age_secs,
                        "relay_frontier_at_snapshot": evidence.relay_frontier_at_snapshot,
                        "relay_frontier_now": evidence.relay_frontier_now,
                        "last_watcher_relayed_offset": evidence.last_watcher_relayed_offset,
                        "last_watcher_relayed_at_unix": evidence.last_watcher_relayed_at_unix,
                        "terminal_delivery_committed": evidence.terminal_delivery_committed,
                        "full_response_utf8_bytes": evidence.full_response.len(),
                        "full_response_chars": evidence.full_response.chars().count(),
                        "response_sent_offset": evidence.response_sent_offset,
                        "prior_delivery_evidence": evidence.prior_delivery_evidence,
                        "turn_age_secs": evidence.turn_age_secs,
                        "now_unix": evidence.now_unix,
                    },
                }),
            },
            crate::services::observability::InvariantSeverity::Warn,
        );
    }
    false
}

fn fresh_watcher_heartbeat_should_block(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    watcher_owner_channel: ChannelId,
    snapshot: &DestructiveCancelProbeSnapshot,
    state: &inflight::InflightTurnState,
    watcher_output_path: &str,
) -> bool {
    let evidence = liveness_evidence(
        shared,
        watcher_owner_channel,
        snapshot,
        state,
        watcher_output_path,
    );
    let forfeited = relay_liveness_forfeit_decision(
        evidence,
        provider,
        channel,
        &snapshot.pin,
        RelayLivenessForfeitSeam::FreshWatcherHeartbeat,
    );
    fresh_watcher_heartbeat_blocks_rebind(evidence, forfeited)
}

fn output_mtime_age_secs(output_path: &str) -> Option<i64> {
    let modified = std::fs::metadata(output_path).ok()?.modified().ok()?;
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_secs(),
    )
    .ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum DestructiveCancelGate {
    Allowed(&'static str),
    Denied(&'static str),
}

impl DestructiveCancelGate {
    pub(in crate::services::discord) fn allowed_reason(self) -> Option<&'static str> {
        match self {
            Self::Allowed(reason) => Some(reason),
            Self::Denied(_) => None,
        }
    }

    pub(in crate::services::discord) fn denied_reason(self) -> Option<&'static str> {
        match self {
            Self::Allowed(_) => None,
            Self::Denied(reason) => Some(reason),
        }
    }

    pub(in crate::services::discord) fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed(_))
    }
}

pub(in crate::services::discord) async fn evaluate(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    watcher_owner_channel: ChannelId,
    snapshot: &DestructiveCancelProbeSnapshot,
) -> DestructiveCancelGate {
    if snapshot.pin.finalizer_turn_id == 0 {
        return DestructiveCancelGate::Denied("missing_finalizer_turn_id");
    }
    let Some(initial_state) = inflight::load_inflight_state(provider, channel.get()) else {
        return DestructiveCancelGate::Denied("inflight_missing_before_probe");
    };
    if !snapshot.pin.matches_state(&initial_state) {
        return DestructiveCancelGate::Denied("identity_mismatch_before_probe");
    }

    // Fresh watcher heartbeat evidence wins before terminal-envelope evidence.
    // Preserve base parity: capture growth, relay-frontier growth, or capture
    // mtime inside the 600-second reclaim floor keeps the live watcher authoritative.
    // If those signals disappear, the next gate pass can accept the envelope.
    let watcher_heartbeat_stale = if let Some(tmux_session) =
        snapshot.pin.tmux_session_name.as_deref()
    {
        match shared.tmux_watchers.tmux_session_is_stale(tmux_session) {
            Some(false) => {
                if let Some(output_path) = shared.tmux_watchers.watcher_output_path(tmux_session) {
                    if fresh_watcher_heartbeat_should_block(
                        shared,
                        provider,
                        channel,
                        watcher_owner_channel,
                        snapshot,
                        &initial_state,
                        &output_path,
                    ) {
                        return DestructiveCancelGate::Denied("fresh_watcher_heartbeat");
                    }
                }
                false
            }
            Some(true) => true,
            None => false,
        }
    } else if let Some(watcher) = shared.tmux_watchers.get(&watcher_owner_channel) {
        let watcher_heartbeat_stale = watcher.heartbeat_stale();
        if !watcher_heartbeat_stale
            && fresh_watcher_heartbeat_should_block(
                shared,
                provider,
                channel,
                watcher_owner_channel,
                snapshot,
                &initial_state,
                &watcher.output_path,
            )
        {
            return DestructiveCancelGate::Denied("fresh_watcher_heartbeat");
        }
        watcher_heartbeat_stale
    } else {
        false
    };

    if terminal_envelope_present(provider, snapshot) {
        return DestructiveCancelGate::Allowed("terminal_envelope_present");
    }

    let Some(expected_output_path) = snapshot.output_path.as_deref() else {
        return DestructiveCancelGate::Denied("halt_evidence_incomplete");
    };
    let Some(expected_output_len) = snapshot.output_len else {
        return DestructiveCancelGate::Denied("halt_evidence_incomplete");
    };
    let mut previous_relay_frontier = snapshot.relay_frontier;
    for _ in 0..DESTRUCTIVE_CANCEL_REPROBE_ATTEMPTS {
        tokio::time::sleep(DESTRUCTIVE_CANCEL_REPROBE_DELAY).await;

        let Some(current) = inflight::load_inflight_state(provider, channel.get()) else {
            return DestructiveCancelGate::Denied("inflight_missing_on_reprobe");
        };
        let mailbox_active_user_msg_id = mailbox_snapshot(shared, channel)
            .await
            .active_user_message_id
            .map(MessageId::get);
        if !snapshot.pin.matches_state(&current)
            || mailbox_active_user_msg_id != snapshot.pin.mailbox_active_user_msg_id
        {
            return DestructiveCancelGate::Denied("identity_mismatch_on_reprobe");
        }
        if current.updated_at != snapshot.updated_at
            || current.save_generation != snapshot.save_generation
        {
            return DestructiveCancelGate::Denied("inflight_refreshed_on_reprobe");
        }
        if current
            .output_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            != Some(expected_output_path)
        {
            return DestructiveCancelGate::Denied("output_path_changed_on_reprobe");
        }

        let output_len_now = std::fs::metadata(expected_output_path)
            .ok()
            .map(|metadata| metadata.len());
        let current_relay_frontier = relay_frontier_for_current_generation(
            shared,
            watcher_owner_channel,
            snapshot.pin.tmux_session_name.as_deref(),
        );
        if relay_frontier_advanced(previous_relay_frontier, current_relay_frontier) {
            return DestructiveCancelGate::Denied("relay_frontier_progress_on_reprobe");
        }
        if output_len_now != Some(expected_output_len) {
            let now_unix = inflight::now_unix();
            let forfeited = relay_liveness_forfeit_decision(
                WatcherRelayLivenessEvidence {
                    output_len_at_snapshot: Some(expected_output_len),
                    output_len_now,
                    output_mtime_age_secs: output_mtime_age_secs(expected_output_path),
                    relay_frontier_at_snapshot: snapshot.relay_frontier,
                    relay_frontier_now: current_relay_frontier,
                    last_watcher_relayed_offset: current.last_watcher_relayed_offset,
                    last_watcher_relayed_at_unix: current.last_watcher_relayed_at_unix,
                    terminal_delivery_committed: current.terminal_delivery_committed,
                    full_response: &current.full_response,
                    response_sent_offset: current.response_sent_offset,
                    prior_delivery_evidence: prior_delivery_evidence(&current),
                    turn_age_secs: turn_age_secs(&current, now_unix),
                    now_unix,
                },
                provider,
                channel,
                &snapshot.pin,
                RelayLivenessForfeitSeam::CaptureProgressOnReprobe,
            );
            if !forfeited {
                return DestructiveCancelGate::Denied("capture_progress_on_reprobe");
            }
        }
        previous_relay_frontier =
            relay_frontier_high_water(previous_relay_frontier, current_relay_frontier);
    }

    let Some(tmux_session) = snapshot.pin.tmux_session_name.as_deref() else {
        return DestructiveCancelGate::Denied("tmux_readiness_evidence_missing");
    };
    if !super::relay_recovery::idle_tmux_repair_ready_for_input(
        provider,
        channel.get(),
        tmux_session,
    ) {
        return DestructiveCancelGate::Denied("tmux_pane_not_ready_for_input");
    }

    if watcher_heartbeat_stale {
        return DestructiveCancelGate::Allowed("capture_and_jsonl_halted_with_stale_watcher");
    }
    DestructiveCancelGate::Allowed("capture_and_jsonl_halted")
}

fn relay_frontier_advanced(previous: Option<u64>, current: Option<u64>) -> bool {
    destructive_cancel_liveness::relay_frontier_advanced(previous, current)
}

fn relay_frontier_high_water(previous: Option<u64>, current: Option<u64>) -> Option<u64> {
    match (previous, current) {
        (Some(previous), Some(current)) => Some(previous.max(current)),
        (Some(previous), None) => Some(previous),
        (None, Some(current)) => Some(current),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn current_thread_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
    }

    fn write_jsonl(path: &std::path::Path, lines: &[&str]) -> u64 {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(path, body).expect("write jsonl");
        std::fs::metadata(path).expect("jsonl metadata").len()
    }

    #[test]
    fn relay_frontier_flap_to_none_then_same_value_is_not_progress() {
        let mut previous = Some(4096);
        assert!(!relay_frontier_advanced(previous, None));
        previous = relay_frontier_high_water(previous, None);
        assert_eq!(previous, Some(4096));
        assert!(!relay_frontier_advanced(previous, Some(4096)));
        assert!(relay_frontier_advanced(previous, Some(4097)));
    }

    #[test]
    fn destructive_cancel_forfeit_decision_is_observation_only_and_persists_context() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let events = crate::services::observability::events::global();
        events.clear();
        let pin = DestructiveCancelIdentityPin {
            finalizer_turn_id: 4_992_777,
            mailbox_active_user_msg_id: Some(4_992_778),
            tmux_session_name: Some("tmux-4992-observation".to_string()),
        };
        let evidence = WatcherRelayLivenessEvidence {
            output_len_at_snapshot: Some(100),
            output_len_now: Some(101),
            output_mtime_age_secs: Some(0),
            relay_frontier_at_snapshot: None,
            relay_frontier_now: None,
            last_watcher_relayed_offset: None,
            last_watcher_relayed_at_unix: None,
            terminal_delivery_committed: false,
            full_response: "unsent",
            response_sent_offset: 0,
            prior_delivery_evidence: false,
            turn_age_secs: Some(destructive_cancel_liveness::RELAY_FORFEIT_MIN_AGE_SECS + 1),
            now_unix: 100_000,
        };

        assert!(relay_liveness_forfeited(evidence));
        assert!(!relay_liveness_forfeit_decision(
            evidence,
            &ProviderKind::Claude,
            ChannelId::new(4_992_779),
            &pin,
            RelayLivenessForfeitSeam::CaptureProgressOnReprobe,
        ));

        let event = events
            .recent(4)
            .into_iter()
            .rev()
            .find(|event| {
                event.event_type == "invariant_violation"
                    && event.payload["invariant"] == "relay_liveness_would_forfeit"
            })
            .expect("would-forfeit event must enter the durable JSONL event buffer");
        assert_eq!(event.channel_id, Some(4_992_779));
        assert_eq!(
            event.payload["details"]["seam"],
            "capture_progress_on_reprobe"
        );
        assert_eq!(event.payload["details"]["arm"], "zero_delivery");
        assert_eq!(
            event.payload["details"]["turn_pin"]["finalizer_turn_id"],
            4_992_777
        );
        assert_eq!(
            event.payload["details"]["tmux_session_name"],
            "tmux-4992-observation"
        );
        assert_eq!(event.payload["details"]["evidence"]["output_len_now"], 101);
    }

    fn save_gate_state(
        provider: ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
        tmux: &str,
        output_path: &std::path::Path,
        last_offset: u64,
    ) -> inflight::InflightTurnState {
        let current_msg_id = if user_msg_id == 0 {
            channel_id + 1
        } else {
            user_msg_id + 1
        };
        let mut state = inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            1,
            user_msg_id,
            current_msg_id,
            "gate fixture".to_string(),
            None,
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            last_offset,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        inflight::save_inflight_state(&state).expect("save inflight state");
        inflight::load_inflight_state(&provider, channel_id).expect("saved inflight state")
    }

    fn qualify_zero_delivery_forfeit(state: &mut inflight::InflightTurnState) {
        state.full_response = "captured but unsent response".to_string();
        state.response_sent_offset = 0;
        state.terminal_delivery_committed = false;
        state.last_watcher_relayed_offset = None;
        state.started_at = (chrono::Local::now()
            - chrono::Duration::seconds(
                destructive_cancel_liveness::RELAY_FORFEIT_MIN_AGE_SECS + 60,
            ))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    }

    #[test]
    fn destructive_cancel_delivery_signals_partition_all_production_combinations() {
        for bits in 0_u8..32 {
            let output_path = std::path::Path::new("/tmp/unused-partition.jsonl");
            let mut state = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                4_992_032 + u64::from(bits),
                None,
                1,
                4_992_100 + u64::from(bits),
                4_992_200 + u64::from(bits),
                "partition fixture".to_string(),
                None,
                Some("tmux-4992-partition".to_string()),
                Some(output_path.to_string_lossy().to_string()),
                None,
                0,
            );
            state.last_watcher_relayed_offset = (bits & 1 != 0).then_some(1);
            state.last_watcher_relayed_at_unix = (bits & 2 != 0).then_some(2);
            state.session_bound_delivered = bits & 4 != 0;
            state.anchor_reposted = bits & 8 != 0;
            state.streaming_rollover_frozen_msg_ids =
                (bits & 16 != 0).then_some(vec![3]).unwrap_or_default();
            let expected = bits != 0;
            let production_prior = prior_delivery_evidence(&state);
            assert_eq!(
                production_prior, expected,
                "bits={bits:05b} production prior-delivery wiring mismatch"
            );
            assert_eq!(
                relay_forfeit_arm(production_prior),
                if expected {
                    RelayForfeitArm::StalledDelivery
                } else {
                    RelayForfeitArm::ZeroDelivery
                },
                "bits={bits:05b} selected the wrong arm"
            );
        }
    }

    fn stale_mtime(path: &std::path::Path) {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(700),
            ),
        )
        .expect("set stale mtime");
    }

    fn write_generation_marker(tmux: &str) -> std::path::PathBuf {
        let path = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
            tmux,
            "generation",
        ));
        std::fs::create_dir_all(path.parent().expect("generation parent"))
            .expect("create generation parent");
        std::fs::write(&path, b"1").expect("write generation");
        path
    }

    fn fresh_watcher_handle(
        tmux_session_name: &str,
        output_path: &std::path::Path,
    ) -> super::super::TmuxWatcherHandle {
        super::super::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string_lossy().to_string(),
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: std::sync::Arc::new(std::sync::Mutex::new(None)),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::tmux_watcher_now_ms(),
            )),
        }
    }

    #[test]
    fn destructive_cancel_zero_origin_capture_growth_blocks_before_terminal_allow() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_992_004);
            let tmux = "tmux-4992-growing-busy";
            let output_path = root.path().join("growing-busy.jsonl");
            let len = write_jsonl(
                &output_path,
                &[
                    r#"{"type":"system","subtype":"init","session_id":"s"}"#,
                    r#"{"type":"result","subtype":"success","result":"done"}"#,
                ],
            );
            let mut state =
                save_gate_state(provider.clone(), channel.get(), 0, tmux, &output_path, len);
            state.full_response.clear();
            state.response_sent_offset = 0;
            inflight::save_inflight_state(&state).expect("save zero-origin state");
            let state = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert!(terminal_envelope_present(&provider, &snapshot));
            let mut grown = std::fs::read(&output_path).expect("read terminal capture");
            grown.push(b'\n');
            assert_eq!(grown.len(), usize::try_from(len + 1).unwrap());
            std::fs::write(&output_path, grown).expect("grow live terminal capture");
            stale_mtime(&output_path);

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(gate.denied_reason(), Some("fresh_watcher_heartbeat"));
            assert!(inflight::load_inflight_state(&provider, channel.get()).is_some());
        });
    }

    #[test]
    fn destructive_cancel_growing_capture_ready_forfeit_remains_disabled() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_992_005);
            let tmux = "tmux-4992-growing-ready";
            let output_path = root.path().join("growing-ready.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            let mut state =
                save_gate_state(provider.clone(), channel.get(), 0, tmux, &output_path, len);
            qualify_zero_delivery_forfeit(&mut state);
            inflight::save_inflight_state(&state).expect("save forfeitable state");
            let state = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            let grown_len = write_jsonl(
                &output_path,
                &[
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"captured response"}]}}"#,
                    r#"{"type":"system","subtype":"init","session_id":"s"}"#,
                ],
            );
            assert!(grown_len > len, "capture must grow before reprobe");

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.denied_reason(),
                Some("fresh_watcher_heartbeat"),
                "would-forfeit observation must not enable destructive cancel: {gate:?}"
            );
        });
    }

    #[test]
    fn destructive_cancel_rewound_offset_after_rollover_delivery_still_denies() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_992_012);
            let tmux = "tmux-4992-rewound-delivery";
            let output_path = root.path().join("rewound-delivery-ready.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            let mut state =
                save_gate_state(provider.clone(), channel.get(), 0, tmux, &output_path, len);
            qualify_zero_delivery_forfeit(&mut state);
            state.streaming_rollover_frozen_msg_ids = vec![9_001];
            inflight::save_inflight_state(&state).expect("save rewound state");
            let state = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            let grown_len = write_jsonl(
                &output_path,
                &[
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"authoritative body"}]}}"#,
                    r#"{"type":"system","subtype":"init","session_id":"s"}"#,
                ],
            );
            assert!(grown_len > len);

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(gate.denied_reason(), Some("fresh_watcher_heartbeat"));
        });
    }

    #[test]
    fn destructive_cancel_partial_delivery_terminal_envelope_recent_mtime_denies() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_992_013);
            let tmux = "tmux-4992-partial-terminal";
            let output_path = root.path().join("partial-terminal.jsonl");
            let len = write_jsonl(
                &output_path,
                &[
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"prefix and suffix"}]}}"#,
                    r#"{"type":"result","subtype":"success","result":"done"}"#,
                ],
            );
            let mut state =
                save_gate_state(provider.clone(), channel.get(), 0, tmux, &output_path, len);
            state.full_response = "delivered prefix and undelivered suffix".to_string();
            state.response_sent_offset = "delivered prefix".len();
            assert!(state.full_response.is_char_boundary(state.response_sent_offset));
            state.last_watcher_relayed_offset = Some(1);
            state.last_watcher_relayed_at_unix = Some(inflight::now_unix());
            inflight::save_inflight_state(&state).expect("save partial-delivery state");
            let state = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert!(terminal_envelope_present(&provider, &snapshot));

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.denied_reason(),
                Some("fresh_watcher_heartbeat"),
                "recent mtime must block before terminal-envelope allow: {gate:?}"
            );
        });
    }

    #[test]
    fn destructive_cancel_tool_only_ready_readiness_still_denies() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_992_011);
            let tmux = "tmux-4992-tool-only-ready";
            let output_path = root.path().join("tool-only-ready.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            let mut state =
                save_gate_state(provider.clone(), channel.get(), 0, tmux, &output_path, len);
            qualify_zero_delivery_forfeit(&mut state);
            state.full_response.clear();
            inflight::save_inflight_state(&state).expect("save tool-only state");
            let state = inflight::load_inflight_state(&provider, channel.get()).unwrap();
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            std::fs::write(&output_path, vec![b'x'; usize::try_from(len + 1).unwrap()])
                .expect("grow tool-only capture");

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(gate.denied_reason(), Some("fresh_watcher_heartbeat"));
        });
    }

    #[test]
    fn destructive_cancel_frontier_absence_does_not_prove_stalled_delivery() {
        let evidence = WatcherRelayLivenessEvidence {
            output_len_at_snapshot: Some(100),
            output_len_now: Some(100),
            output_mtime_age_secs: Some(601),
            relay_frontier_at_snapshot: None,
            relay_frontier_now: None,
            last_watcher_relayed_offset: Some(100),
            last_watcher_relayed_at_unix: Some(90_000),
            terminal_delivery_committed: false,
            full_response: "delivered prefix and undelivered suffix",
            response_sent_offset: "delivered prefix".len(),
            prior_delivery_evidence: true,
            turn_age_secs: Some(20_000),
            now_unix: 100_000,
        };
        assert!(!relay_liveness_forfeited(evidence));
        assert!(fresh_watcher_heartbeat_blocks_rebind(evidence, false));
    }

    #[test]
    fn frozen_capture_for_zero_origin_busy_turn_still_denies_destructive_cancel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_974_011);
            let output_path = root.path().join("zero-origin-busy-frozen.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"tool still running"}]}}"#],
            );
            stale_mtime(&output_path);
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                0,
                "tmux-4974-zero-origin-busy-frozen",
                &output_path,
                len,
            );
            assert!(!state.rebind_origin);
            let snapshot = DestructiveCancelProbeSnapshot::from_state(
                &shared,
                &state,
                None,
                channel,
            );

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.denied_reason(),
                Some("tmux_pane_not_ready_for_input"),
                "a frozen capture/frontier is not death evidence while structured state says the pane is busy"
            );
            assert!(
                inflight::load_inflight_state(&provider, channel.get()).is_some(),
                "denied destructive gate must preserve the live zero-origin row"
            );
        });
    }

    #[test]
    fn ready_pane_reprobe_freeze_allows_destructive_cancel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_035_011);
            let output_path = root.path().join("ready.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                4_035_111,
                "tmux-4035-ready",
                &output_path,
                len,
            );
            let snapshot = DestructiveCancelProbeSnapshot::from_state(
                &shared,
                &state,
                None,
                channel,
            );

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.allowed_reason(),
                Some("capture_and_jsonl_halted"),
                "ready-for-input evidence plus frozen capture/frontier is sufficient no-progress evidence"
            );
        });
    }

    // #4353: reads tmux generation files via `super::super::tmux` (cfg(unix)).
    #[cfg(unix)]
    #[test]
    fn generation_mismatched_relay_frontier_does_not_fake_reprobe_progress() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_035_012);
            let tmux = "tmux-4035-stale-frontier";
            let output_path = root.path().join("stale-frontier-ready.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            write_generation_marker(tmux);
            let current_generation = super::super::tmux::read_generation_file_mtime_ns(tmux);
            assert!(
                current_generation > 0,
                "generation marker mtime is observable"
            );
            let coord = shared.tmux_relay_coord(channel);
            coord
                .confirmed_end_offset
                .store(4096, std::sync::atomic::Ordering::Release);
            coord.confirmed_end_generation_mtime_ns.store(
                current_generation.saturating_sub(1),
                std::sync::atomic::Ordering::Release,
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                4_035_112,
                tmux,
                &output_path,
                len,
            );
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert_eq!(snapshot.relay_frontier, None);

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.allowed_reason(),
                Some("capture_and_jsonl_halted"),
                "a stale prior-generation relay frontier must not become reprobe progress evidence"
            );
        });
    }

    // #4353: reads tmux generation files via `super::super::tmux` (cfg(unix)).
    #[cfg(unix)]
    #[test]
    fn current_generation_relay_frontier_after_empty_snapshot_denies_destructive_cancel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_035_014);
            let tmux = "tmux-4035-frontier-after-snapshot";
            let output_path = root.path().join("frontier-after-snapshot.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                4_035_114,
                tmux,
                &output_path,
                len,
            );
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert_eq!(snapshot.relay_frontier, None);

            let generation_path = write_generation_marker(tmux);
            let current_generation = super::super::tmux::read_generation_file_mtime_ns(tmux);
            assert!(
                current_generation > 0,
                "generation marker mtime is observable"
            );
            let coord = shared.tmux_relay_coord(channel);
            coord
                .confirmed_end_offset
                .store(4096, std::sync::atomic::Ordering::Release);
            coord
                .confirmed_end_generation_mtime_ns
                .store(current_generation, std::sync::atomic::Ordering::Release);

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.denied_reason(),
                Some("relay_frontier_progress_on_reprobe"),
                "a current-generation frontier appearing after a None snapshot is progress"
            );
            let _ = std::fs::remove_file(generation_path);
        });
    }

    #[test]
    fn fresh_heartbeat_with_stale_capture_falls_through_without_stale_reason() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_035_013);
            let tmux = "tmux-4035-fresh-heartbeat-stale-capture";
            let output_path = root.path().join("fresh-heartbeat-ready.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"system","subtype":"init","session_id":"s"}"#],
            );
            stale_mtime(&output_path);
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                4_035_113,
                tmux,
                &output_path,
                len,
            );
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.allowed_reason(),
                Some("capture_and_jsonl_halted"),
                "a turn without an unsent payload keeps the bounded halted-recovery path"
            );
        });
    }
}
