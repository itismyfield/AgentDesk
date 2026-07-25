use super::*;

#[derive(Debug, serde::Serialize)]
struct WatcherForcedKillLog<'a> {
    timestamp: String,
    session: &'a str,
    pane_id: Option<String>,
    pane_pid: Option<u32>,
    current_offset: u64,
    decision_reason: &'a str,
    live_background_workers: Vec<String>,
}

pub(super) fn write_watcher_forced_kill_log(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    current_offset: u64,
    decision_reason: &str,
) {
    let record = WatcherForcedKillLog {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session: tmux_session_name,
        pane_id: crate::services::platform::tmux::active_pane_id(tmux_session_name),
        pane_pid: crate::services::platform::tmux::pane_pid(tmux_session_name),
        current_offset,
        decision_reason,
        live_background_workers: shared
            .ui
            .placeholder_live_events
            .live_background_worker_inventory(channel_id),
    };
    let path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "forced_kill_log");
    let serialized = match serde_json::to_string(&record) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                tmux_session = %tmux_session_name,
                error = %error,
                "failed to serialize watcher forced-kill log"
            );
            return;
        }
    };
    if let Err(error) = std::fs::write(&path, format!("{serialized}\n")) {
        tracing::error!(
            tmux_session = %tmux_session_name,
            path = %path,
            error = %error,
            "failed to persist watcher forced-kill log"
        );
    }
}

pub(super) fn watcher_session_is_main_orchestration(
    tmux_session_name: &str,
    channel_id: serenity::ChannelId,
) -> bool {
    let Some((_provider, channel_segment)) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)
    else {
        return true;
    };
    !channel_segment.ends_with(&format!("-t{}", channel_id.get()))
}

#[cfg(test)]
mod tests {
    use super::watcher_session_is_main_orchestration;
    use poise::serenity_prelude::ChannelId;

    #[test]
    fn main_orchestration_session_is_never_automatic_kill_target() {
        let channel_id = ChannelId::new(1_504_468_805_772_902_471);
        assert!(watcher_session_is_main_orchestration(
            "AgentDesk-claude-adk-cc",
            channel_id,
        ));
        assert!(!watcher_session_is_main_orchestration(
            "AgentDesk-claude-adk-cc-t1504468805772902471",
            channel_id,
        ));
    }

    #[test]
    fn unparseable_session_role_fails_closed_as_main() {
        assert!(watcher_session_is_main_orchestration(
            "operator-created-session",
            ChannelId::new(42),
        ));
    }
}
