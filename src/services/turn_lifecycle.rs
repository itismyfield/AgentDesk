use poise::serenity_prelude::ChannelId;

use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

#[derive(Debug, Clone)]
pub(crate) struct TurnLifecycleTarget {
    pub provider: Option<ProviderKind>,
    pub channel_id: Option<ChannelId>,
    pub tmux_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnLifecycleStopResult {
    pub lifecycle_path: &'static str,
    pub tmux_killed: bool,
    pub inflight_cleared: bool,
    pub queue_depth: Option<usize>,
    pub queue_preserved: bool,
    pub termination_recorded: bool,
}

pub(crate) async fn stop_turn_preserving_queue(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
) -> TurnLifecycleStopResult {
    let mut lifecycle_path = "direct-fallback";
    let mut queue_depth = None;
    let mut termination_recorded = false;
    let tmux_was_alive = crate::services::platform::tmux::has_session(&target.tmux_name);

    if let (Some(registry), Some(provider), Some(channel_id)) =
        (health_registry, target.provider.as_ref(), target.channel_id)
    {
        if let Some(runtime) = crate::services::discord::health::stop_provider_channel_runtime(
            registry,
            provider.as_str(),
            channel_id,
            reason,
        )
        .await
        {
            lifecycle_path = runtime.lifecycle_path;
            queue_depth = Some(runtime.queue_depth);
            termination_recorded = runtime.termination_recorded;
        }
    }

    #[cfg(unix)]
    if crate::services::platform::tmux::has_session(&target.tmux_name) {
        record_tmux_exit_reason(&target.tmux_name, &format!("explicit cleanup via {reason}"));
    }

    let tmux_killed = if crate::services::platform::tmux::has_session(&target.tmux_name) {
        crate::services::platform::tmux::kill_session(&target.tmux_name)
    } else {
        tmux_was_alive
    };

    let inflight_cleared = target
        .provider
        .as_ref()
        .is_some_and(|provider| clear_inflight_by_tmux_name(provider, &target.tmux_name));

    TurnLifecycleStopResult {
        lifecycle_path,
        tmux_killed,
        inflight_cleared,
        queue_depth,
        queue_preserved: true,
        termination_recorded,
    }
}

/// Scan inflight directory for the provider and delete the file matching the given tmux session.
pub(crate) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    let inflight_root = match crate::config::runtime_root() {
        Some(root) => root.join("runtime").join("discord_inflight"),
        None => return false,
    };

    let provider_dir = inflight_root.join(provider.as_str());
    let Ok(entries) = std::fs::read_dir(&provider_dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(data) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if state
            .get("tmux_session_name")
            .and_then(|value| value.as_str())
            == Some(tmux_name)
        {
            let _ = std::fs::remove_file(&path);
            return true;
        }
    }
    false
}
