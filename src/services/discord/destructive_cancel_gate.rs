use std::path::Path;
use std::time::Duration;

// Child module (file lives alongside in discord/) — declared here instead of
// the ratcheted discord/mod.rs; this gate is its only consumer.
#[path = "destructive_cancel_capture.rs"]
mod destructive_cancel_capture;
use super::{DeliveryLeaseKey, SharedData, inflight, mailbox_snapshot};
use destructive_cancel_capture::{CaptureProgressEvidence, fresh_watcher_heartbeat_blocks_rebind};
use poise::serenity_prelude::{ChannelId, MessageId};

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

/// #5071 relay-tail S4 (I-1) attribution for the lease key derived here; only
/// the `delivery lease id-0 turn missing disambiguators` warn log reads it.
const DESTRUCTIVE_FENCE_LEASE_SITE: &str = "destructive_fence";

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct DestructiveCancelProbeSnapshot {
    pub pin: DestructiveCancelIdentityPin,
    pub inflight_identity: inflight::InflightTurnIdentity,
    pub updated_at: String,
    pub save_generation: u64,
    pub output_path: Option<String>,
    pub output_len: Option<u64>,
    pub relay_frontier: Option<u64>,
    /// #5071 relay-tail S4 (I-1): the delivery-lease identity of the turn this
    /// probe pinned, derived from the SAME inflight read the identity pin came
    /// from. Carried on the snapshot rather than recomputed at the call sites so
    /// the key a `TerminalDeliveryFence` re-reads the lease against cannot drift
    /// from the row the rest of the gate is pinned to.
    pub delivery_lease_key: DeliveryLeaseKey,
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
        let delivery_lease_key = DeliveryLeaseKey::from_inflight_state_for_site(
            watcher_owner_channel,
            shared.restart.current_generation,
            state,
            DESTRUCTIVE_FENCE_LEASE_SITE,
        );
        Self {
            pin,
            inflight_identity: inflight::InflightTurnIdentity::from_state(state),
            updated_at: state.updated_at.clone(),
            save_generation: state.save_generation,
            output_path,
            output_len,
            relay_frontier,
            delivery_lease_key,
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

fn fresh_watcher_heartbeat_should_block(
    shared: &SharedData,
    watcher_owner_channel: ChannelId,
    snapshot: &DestructiveCancelProbeSnapshot,
    watcher_output_path: &str,
) -> bool {
    let output_len_now = std::fs::metadata(watcher_output_path)
        .ok()
        .map(|metadata| metadata.len());
    let output_len_at_snapshot = snapshot
        .output_path
        .as_deref()
        .filter(|path| Path::new(path) == Path::new(watcher_output_path))
        .and(snapshot.output_len);
    fresh_watcher_heartbeat_blocks_rebind(
        CaptureProgressEvidence {
            output_len_at_snapshot,
            output_len_now,
            output_mtime_age_secs: output_mtime_age_secs(watcher_output_path),
            relay_frontier_at_snapshot: snapshot.relay_frontier,
            relay_frontier_now: relay_frontier_for_current_generation(
                shared,
                watcher_owner_channel,
                snapshot.pin.tmux_session_name.as_deref(),
            ),
        },
        crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS,
    )
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

    // Fresh watcher heartbeat wins before terminal-envelope evidence. A live
    // watcher is the safer owner for a just-finished turn; if it disappears, the
    // next gate pass can still accept the terminal envelope.
    let watcher_heartbeat_stale = if let Some(tmux_session) =
        snapshot.pin.tmux_session_name.as_deref()
    {
        match shared.tmux_watchers.tmux_session_is_stale(tmux_session) {
            Some(false) => {
                if let Some(output_path) = shared.tmux_watchers.watcher_output_path(tmux_session) {
                    if fresh_watcher_heartbeat_should_block(
                        shared,
                        watcher_owner_channel,
                        snapshot,
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
                watcher_owner_channel,
                snapshot,
                &watcher.output_path,
            )
        {
            return DestructiveCancelGate::Denied("fresh_watcher_heartbeat");
        }
        watcher_heartbeat_stale
    } else {
        false
    };

    // #5071 relay-tail S4 (I-2a): the terminal envelope USED to short-circuit
    // here, ahead of every no-progress check below. It is evidence that the
    // provider WROTE its turn terminator — the terminal POST began — and not
    // that the relay finished delivering the bytes after it. Short-circuiting on
    // it meant a growing capture, an advancing relay frontier and a busy pane
    // were never consulted for exactly the turns most likely to have an
    // undelivered tail. It is now demoted to a fallback REASON at the bottom of
    // this function, reached only when no denial fired.
    //
    // The demotion has a LATENCY cost, and it is paid on the common case. An
    // envelope-present turn used to return `Allowed` from this line without
    // touching the disk; it now walks the whole no-progress ladder below, whose
    // reprobe loop sleeps `DESTRUCTIVE_CANCEL_REPROBE_DELAY` between attempts —
    // so the worst case for a turn that ends up Allowed anyway is
    // `DESTRUCTIVE_CANCEL_REPROBE_ATTEMPTS * DESTRUCTIVE_CANCEL_REPROBE_DELAY`
    // (3 x 1s = 3s in production; 2 x 10ms under `cfg(test)`) plus the metadata
    // and frontier reads each attempt makes. Both callers pay it: the
    // relay-recovery dead-frontier path and the TUI-direct claim path
    // (`tui_direct_pending_start`), where it lands inside the interactive
    // pending-start wait rather than a background sweep. That is the intended
    // trade — the delay buys the reprobe a chance to SEE the relay frontier
    // advance, which is the only evidence that distinguishes "the terminator was
    // written" from "the bytes after it were delivered" — but it is a real
    // regression in claim latency for every envelope-present turn, not a
    // free-by-construction reordering.
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
        if output_len_now != Some(expected_output_len) {
            return DestructiveCancelGate::Denied("capture_progress_on_reprobe");
        }
        let current_relay_frontier = relay_frontier_for_current_generation(
            shared,
            watcher_owner_channel,
            snapshot.pin.tmux_session_name.as_deref(),
        );
        if relay_frontier_advanced(previous_relay_frontier, current_relay_frontier) {
            return DestructiveCancelGate::Denied("relay_frontier_progress_on_reprobe");
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
    // #5071 relay-tail S4 (I-2a): the demoted envelope. Every denial gate above
    // has already passed, so this only chooses which allow REASON is logged as
    // `death_evidence`; it can no longer skip a gate. Kept as a distinct label
    // because "halted AND the provider wrote its terminator" is a materially
    // different triage picture from "halted with no terminator at all".
    if terminal_envelope_present(provider, snapshot) {
        return DestructiveCancelGate::Allowed("terminal_envelope_present");
    }
    DestructiveCancelGate::Allowed("capture_and_jsonl_halted")
}

fn relay_frontier_advanced(previous: Option<u64>, current: Option<u64>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => current > previous,
        (None, Some(current)) => current > 0,
        _ => false,
    }
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
    fn growing_capture_for_zero_origin_busy_turn_denies_destructive_cancel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_974_010);
            let tmux = "tmux-4974-zero-origin-busy";
            let output_path = root.path().join("zero-origin-busy.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"tool still running"}]}}"#],
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                0,
                tmux,
                &output_path,
                len,
            );
            assert!(!state.rebind_origin);
            shared
                .tmux_watchers
                .insert(channel, fresh_watcher_handle(tmux, &output_path));
            let snapshot = DestructiveCancelProbeSnapshot::from_state(
                &shared,
                &state,
                None,
                channel,
            );
            std::fs::write(&output_path, vec![b'x'; usize::try_from(len + 1).unwrap()])
                .expect("grow live capture");

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(gate.denied_reason(), Some("fresh_watcher_heartbeat"));
            assert!(
                inflight::load_inflight_state(&provider, channel.get()).is_some(),
                "a live zero-origin row with growing capture must be preserved"
            );
        });
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

    /// #5071 relay-tail S4 (I-2a). The transcript carries Claude's authoritative
    /// turn terminator, so before this slice the gate short-circuited to
    /// `Allowed("terminal_envelope_present")` without ever entering the reprobe
    /// loop. A relay frontier that ADVANCES during the reprobe is the relay
    /// actively delivering the tail that follows that terminator, which is
    /// exactly what the envelope cannot speak to. Restoring the early return
    /// ahead of the reprobe loop fails here.
    ///
    /// #4353: reads tmux generation files via `super::super::tmux` (cfg(unix)).
    #[cfg(unix)]
    #[test]
    fn terminal_envelope_does_not_outrank_relay_frontier_progress_on_reprobe() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(5_071_401);
            let tmux = "tmux-5071-envelope-vs-frontier";
            let output_path = root.path().join("envelope-vs-frontier.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"result","result":"done","session_id":"s"}"#],
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                5_071_501,
                tmux,
                &output_path,
                len,
            );
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert!(
                terminal_envelope_present(&provider, &snapshot),
                "fixture must actually carry a terminal envelope, or this test proves nothing"
            );
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
                "a terminal envelope is evidence the terminal POST began, not that the relay finished delivering after it"
            );
            let _ = std::fs::remove_file(generation_path);
        });
    }

    /// #5071 relay-tail S4 (I-2a), the other half: demoting the envelope must not
    /// DELETE it. Same fixture as `ready_pane_reprobe_freeze_allows_destructive_cancel`
    /// except that the transcript ends in Claude's turn terminator, so the ladder
    /// reaches the bottom with no denial and the envelope is what distinguishes
    /// the reported `death_evidence`.
    #[test]
    fn terminal_envelope_is_the_demoted_fallback_allow_reason() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(5_071_402);
            let output_path = root.path().join("envelope-fallback.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"result","result":"done","session_id":"s"}"#],
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                5_071_502,
                "tmux-5071-envelope-fallback",
                &output_path,
                len,
            );
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert!(
                terminal_envelope_present(&provider, &snapshot),
                "fixture must actually carry a terminal envelope, or this test proves nothing"
            );

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.allowed_reason(),
                Some("terminal_envelope_present"),
                "with every denial gate passed, the envelope is still the reported death evidence"
            );
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
                "fresh heartbeat plus stale capture is allowed, but must not be logged as stale watcher evidence"
            );
        });
    }
}
