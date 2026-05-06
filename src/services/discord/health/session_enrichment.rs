use poise::serenity_prelude::ChannelId;

use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;

pub(super) const WATCHER_STATE_DESYNC_STALE_MS: i64 = 30_000;

#[derive(Debug)]
pub(super) struct SessionEnrichment {
    pub inflight: Option<discord::inflight::InflightTurnState>,
    pub attached: bool,
    pub watcher_attached: bool,
    pub has_relay_coord: bool,
    pub watcher_owner_channel_id: Option<u64>,
    pub tmux_session: Option<String>,
    pub inflight_state_present: bool,
    pub tmux_session_mismatch: bool,
    pub last_relay_offset: u64,
    pub last_relay_ts_ms: i64,
    pub reconnect_count: u64,
    pub last_capture_offset: Option<u64>,
    pub unread_bytes: Option<u64>,
    pub relay_stale: bool,
    pub capture_lagged: bool,
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
        let tmux_session = watcher_binding
            .map(|binding| binding.tmux_session_name)
            .or(inflight_tmux_session);
        let (last_relay_offset, last_relay_ts_ms, reconnect_count) = shared
            .tmux_relay_coords
            .get(&channel)
            .map(|coord| {
                (
                    coord
                        .confirmed_end_offset
                        .load(std::sync::atomic::Ordering::Acquire),
                    coord
                        .last_relay_ts_ms
                        .load(std::sync::atomic::Ordering::Acquire),
                    coord
                        .reconnect_count
                        .load(std::sync::atomic::Ordering::Acquire),
                )
            })
            .unwrap_or((0, 0, 0));
        let output_path_for_metadata = inflight
            .as_ref()
            .and_then(|state| state.output_path.as_deref())
            .map(str::to_string);
        let last_capture_offset = match output_path_for_metadata {
            Some(path) => tokio::task::spawn_blocking(move || {
                std::fs::metadata(path).ok().map(|meta| meta.len())
            })
            .await
            .unwrap_or(None),
            None => None,
        };
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

        Self {
            inflight,
            attached,
            watcher_attached,
            has_relay_coord,
            watcher_owner_channel_id,
            tmux_session,
            inflight_state_present,
            tmux_session_mismatch,
            last_relay_offset,
            last_relay_ts_ms,
            reconnect_count,
            last_capture_offset,
            unread_bytes,
            relay_stale,
            capture_lagged,
        }
    }

    pub async fn tmux_session_alive(&self) -> Option<bool> {
        match self.tmux_session.as_ref() {
            Some(name) => {
                let probe_target = name.clone();
                let alive = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::has_session(&probe_target)
                })
                .await
                .unwrap_or(false);
                Some(alive)
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
        self.capture_lagged
            || live_tmux_orphaned
            || (self.tmux_session_mismatch && self.relay_stale)
    }

    pub fn inflight_started_at(&self) -> Option<String> {
        self.inflight.as_ref().map(|state| state.started_at.clone())
    }

    pub fn inflight_updated_at(&self) -> Option<String> {
        self.inflight.as_ref().map(|state| state.updated_at.clone())
    }

    pub fn inflight_user_msg_id(&self) -> Option<u64> {
        self.inflight
            .as_ref()
            .map(|state| state.user_msg_id)
            .filter(|id| *id != 0)
    }

    pub fn inflight_current_msg_id(&self) -> Option<u64> {
        self.inflight
            .as_ref()
            .map(|state| state.current_msg_id)
            .filter(|id| *id != 0)
    }

    pub fn watcher_owns_live_relay(&self) -> bool {
        self.inflight
            .as_ref()
            .is_some_and(|state| state.watcher_owns_live_relay)
    }

    pub fn active_dispatch_present(&self) -> bool {
        self.inflight
            .as_ref()
            .and_then(|state| state.dispatch_id.as_deref())
            .is_some()
    }
}
