use std::path::Path;
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{SharedData, inflight, mailbox_snapshot};
use crate::services::provider::ProviderKind;

const DESTRUCTIVE_CANCEL_REPROBE_DELAY: Duration = Duration::from_millis(250);

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
            output_path,
            output_len,
            relay_frontier,
        }
    }
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

    if let Some(tmux_session) = snapshot.pin.tmux_session_name.as_deref() {
        match shared.tmux_watchers.tmux_session_is_stale(tmux_session) {
            Some(false) => return DestructiveCancelGate::Denied("fresh_watcher_heartbeat"),
            Some(true) => return DestructiveCancelGate::Allowed("watcher_heartbeat_stale"),
            None => {}
        }
    } else if let Some(watcher) = shared.tmux_watchers.get(&watcher_owner_channel) {
        if !watcher.heartbeat_stale() {
            return DestructiveCancelGate::Denied("fresh_watcher_heartbeat");
        }
        return DestructiveCancelGate::Allowed("watcher_heartbeat_stale");
    }

    if snapshot.output_path.as_deref().is_some_and(|path| {
        crate::services::tui_turn_state::jsonl_turn_end_terminator_idle(provider, Path::new(path))
    }) {
        return DestructiveCancelGate::Allowed("terminal_envelope_present");
    }

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
    if current.updated_at != snapshot.updated_at {
        return DestructiveCancelGate::Denied("inflight_refreshed_on_reprobe");
    }

    let output_len_now = snapshot
        .output_path
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len());
    let output_halted = output_len_now.is_some() && output_len_now == snapshot.output_len;
    let relay_frontier_halted = snapshot
        .relay_frontier
        .is_none_or(|frontier| shared.committed_relay_offset(watcher_owner_channel) == frontier);
    if output_halted && relay_frontier_halted {
        return DestructiveCancelGate::Allowed("capture_and_jsonl_halted");
    }

    DestructiveCancelGate::Denied("death_evidence_missing")
}
