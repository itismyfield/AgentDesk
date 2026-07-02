use std::path::Path;
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{SharedData, inflight, mailbox_snapshot};
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
    pub updated_at: String,
    pub save_generation: u64,
    pub output_path: Option<String>,
    pub output_len: Option<u64>,
    pub relay_frontier: Option<u64>,
}

impl DestructiveCancelProbeSnapshot {
    pub(in crate::services::discord) fn from_state(
        state: &inflight::InflightTurnState,
        mailbox_active_user_msg_id: Option<u64>,
        relay_frontier: Option<u64>,
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
        Self {
            pin: DestructiveCancelIdentityPin::from_state(state, mailbox_active_user_msg_id),
            updated_at: state.updated_at.clone(),
            save_generation: state.save_generation,
            output_path,
            output_len,
            relay_frontier,
        }
    }

    pub(in crate::services::discord) fn from_pinned_state(
        state: &inflight::InflightTurnState,
        pin: DestructiveCancelIdentityPin,
        relay_frontier: Option<u64>,
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
        Self {
            pin,
            updated_at: state.updated_at.clone(),
            save_generation: state.save_generation,
            output_path,
            output_len,
            relay_frontier,
        }
    }
}

pub(in crate::services::discord) fn terminal_envelope_present(
    provider: &ProviderKind,
    snapshot: &DestructiveCancelProbeSnapshot,
) -> bool {
    snapshot.output_path.as_deref().is_some_and(|path| {
        crate::services::tui_turn_state::jsonl_turn_end_terminator_idle(provider, Path::new(path))
    })
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
    let watcher_heartbeat_stale =
        if let Some(tmux_session) = snapshot.pin.tmux_session_name.as_deref() {
            match shared.tmux_watchers.tmux_session_is_stale(tmux_session) {
                Some(false) => return DestructiveCancelGate::Denied("fresh_watcher_heartbeat"),
                Some(true) => true,
                None => false,
            }
        } else if let Some(watcher) = shared.tmux_watchers.get(&watcher_owner_channel) {
            if !watcher.heartbeat_stale() {
                return DestructiveCancelGate::Denied("fresh_watcher_heartbeat");
            }
            true
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
    let Some(expected_relay_frontier) = snapshot.relay_frontier else {
        return DestructiveCancelGate::Denied("halt_evidence_incomplete");
    };

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
        if shared.committed_relay_offset(watcher_owner_channel) != expected_relay_frontier {
            return DestructiveCancelGate::Denied("relay_frontier_progress_on_reprobe");
        }
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

    fn save_gate_state(
        provider: ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
        tmux: &str,
        output_path: &std::path::Path,
        last_offset: u64,
    ) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            1,
            user_msg_id,
            user_msg_id + 1,
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

    #[test]
    fn busy_pane_reprobe_freeze_denies_destructive_cancel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::TempDir::new().expect("runtime root");
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        current_thread_rt().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(4_035_010);
            let output_path = root.path().join("busy.jsonl");
            let len = write_jsonl(
                &output_path,
                &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"tool still running"}]}}"#],
            );
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                4_035_110,
                "tmux-4035-busy",
                &output_path,
                len,
            );
            let snapshot = DestructiveCancelProbeSnapshot::from_state(
                &state,
                None,
                Some(shared.committed_relay_offset(channel)),
            );

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.denied_reason(),
                Some("tmux_pane_not_ready_for_input"),
                "a frozen capture/frontier is not death evidence while structured state says the pane is busy"
            );
            assert!(
                inflight::load_inflight_state(&provider, channel.get()).is_some(),
                "denied destructive gate must preserve the live row"
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
                &state,
                None,
                Some(shared.committed_relay_offset(channel)),
            );

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;

            assert_eq!(
                gate.allowed_reason(),
                Some("capture_and_jsonl_halted"),
                "ready-for-input evidence plus frozen capture/frontier is sufficient no-progress evidence"
            );
        });
    }
}
