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
    stop_turn_with_policy(
        health_registry,
        target,
        reason,
        crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
            restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
        },
    )
    .await
}

pub(crate) async fn force_kill_turn(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    termination_reason_code: &'static str,
) -> TurnLifecycleStopResult {
    stop_turn_with_policy(
        health_registry,
        target,
        reason,
        crate::services::discord::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(termination_reason_code),
        },
    )
    .await
}

async fn stop_turn_with_policy(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    cleanup_policy: crate::services::discord::TmuxCleanupPolicy,
) -> TurnLifecycleStopResult {
    let mut lifecycle_path = "direct-fallback";
    let mut queue_depth = None;
    let mut termination_recorded = false;
    let tmux_was_alive = crate::services::platform::tmux::has_session(&target.tmux_name);
    let cleanup_tmux = cleanup_policy.should_cleanup_tmux();

    if let (Some(registry), Some(provider), Some(channel_id)) =
        (health_registry, target.provider.as_ref(), target.channel_id)
    {
        let runtime = if cleanup_tmux {
            let termination_reason_code = match cleanup_policy {
                crate::services::discord::TmuxCleanupPolicy::CleanupSession {
                    termination_reason_code,
                } => termination_reason_code.unwrap_or("force_kill"),
                crate::services::discord::TmuxCleanupPolicy::PreserveSession
                | crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                    ..
                } => "force_kill",
            };
            crate::services::discord::health::force_kill_provider_channel_runtime(
                registry,
                provider.as_str(),
                channel_id,
                reason,
                termination_reason_code,
            )
            .await
        } else {
            crate::services::discord::health::stop_provider_channel_runtime_with_policy(
                registry,
                provider.as_str(),
                channel_id,
                reason,
                cleanup_policy,
            )
            .await
        };
        if let Some(runtime) = runtime {
            lifecycle_path = runtime.lifecycle_path;
            queue_depth = Some(runtime.queue_depth);
            termination_recorded = runtime.termination_recorded;
        }
    }

    let tmux_killed = if cleanup_tmux {
        #[cfg(unix)]
        if crate::services::platform::tmux::has_session(&target.tmux_name) {
            record_tmux_exit_reason(&target.tmux_name, &format!("explicit cleanup via {reason}"));
        }

        let killed_now = if crate::services::platform::tmux::has_session(&target.tmux_name) {
            crate::services::platform::tmux::kill_session_with_reason(
                &target.tmux_name,
                &format!("explicit cleanup via {reason}"),
            )
        } else {
            tmux_was_alive
        };
        // Delete persistent + legacy session temp files alongside the kill
        // so /tmp and ~/.adk/release/runtime/sessions/ don't accumulate
        // stale jsonl/FIFO/owner markers after forced termination (#892).
        if killed_now {
            crate::services::tmux_common::cleanup_session_temp_files(&target.tmux_name);
        }
        killed_now
    } else {
        false
    };

    let inflight_cleared = cleanup_policy.should_clear_inflight()
        && target
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

#[cfg(test)]
mod tests {
    #[test]
    fn preserve_session_handoff_policy_keeps_inflight_metadata() {
        assert!(
            !crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
            }
            .should_clear_inflight()
        );
        assert!(
            crate::services::discord::TmuxCleanupPolicy::PreserveSession.should_clear_inflight()
        );
        assert!(
            crate::services::discord::TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: None,
            }
            .should_clear_inflight()
        );
    }
}
