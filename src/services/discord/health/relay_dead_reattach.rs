use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::{HealthRegistry, recovery, stall_liveness};
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::{self, SharedData};
use crate::services::provider::ProviderKind;

fn should_reattach_relay_dead_watcher(
    snapshot: &WatcherStateSnapshot,
    channel_id: ChannelId,
    latest_runtime_activity_unix_nanos: i64,
    now_unix_secs: i64,
    boot_unix_secs: i64,
) -> bool {
    if snapshot.relay_stall_state != RelayStallState::TmuxAliveRelayDead
        || !snapshot.attached
        || snapshot.watcher_owner_channel_id != Some(channel_id.get())
        || snapshot.tmux_session_alive != Some(true)
    {
        return false;
    }
    if !recovery::stall_watchdog_should_force_clean(
        snapshot.attached,
        true,
        false,
        snapshot.inflight_terminal_delivery_committed,
        snapshot.inflight_started_at.as_deref(),
        now_unix_secs,
        recovery::STALL_WATCHDOG_THRESHOLD_SECS,
        boot_unix_secs,
    ) {
        return false;
    }
    // Fresh runtime activity is a blocker for destructive cleanup, but for a
    // relay frontier that never moved it is evidence that a non-destructive
    // watcher reattach can recover live output.
    if snapshot
        .relay_health
        .relay_frontier_never_advanced_with_unread_tail()
    {
        return true;
    }
    !stall_liveness::stall_watchdog_jsonl_liveness_defers_force_clean(
        latest_runtime_activity_unix_nanos,
        now_unix_secs,
        recovery::STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS,
    )
}

pub(super) async fn try_apply(
    registry: &HealthRegistry,
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
) -> bool {
    let Some(latest_activity_unix_nanos) = snapshot
        .tmux_session
        .as_deref()
        .map(crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos)
    else {
        return false;
    };
    if !should_reattach_relay_dead_watcher(
        snapshot,
        channel_id,
        latest_activity_unix_nanos,
        now_unix_secs,
        registry.started_at_unix(),
    ) {
        return false;
    }
    match discord::relay_recovery::auto_apply_relay_recovery_for_shared(
        registry,
        shared,
        provider,
        channel_id.get(),
        discord::relay_recovery::RelayRecoveryActionKind::ReattachWatcher,
        discord::relay_recovery::RelayRecoveryApplySource::StallWatchdog,
    )
    .await
    {
        Ok(response) => response.applied,
        Err(error) => {
            tracing::warn!(
                target: "agentdesk::discord::relay_recovery",
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                status = error.status_str(),
                body = %error.body(),
                "relay-dead watcher reattach skipped"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
    use chrono::TimeZone;

    fn local_string(unix: i64) -> String {
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn snapshot(started_at: &str) -> WatcherStateSnapshot {
        WatcherStateSnapshot {
            provider: "codex".to_string(),
            attached: true,
            tmux_session: Some("AgentDesk-codex-test".to_string()),
            watcher_owner_channel_id: Some(42),
            last_relay_offset: 0,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: Some(128),
            capture_coordinate: crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                offset: Some(128),
                path_hash: 0,
                file_id: None,
                status: crate::services::discord::health::liveness_authority::CoordinateStatus::Observed,
            },
            unread_bytes: Some(128),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some(started_at.to_string()),
            inflight_updated_at: Some(started_at.to_string()),
            inflight_user_msg_id: Some(1),
            inflight_current_msg_id: Some(2),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(1),
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some("/tmp/AgentDesk-codex-test.jsonl".to_string()),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: "codex".to_string(),
                channel_id: 42,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some("AgentDesk-codex-test".to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(42),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(2),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(1),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(2),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(128),
                last_relay_offset: 0,
                unread_bytes: Some(128),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    #[test]
    fn relay_dead_watcher_reattach_handles_dead_frontier_liveness() {
        let now = chrono::Utc::now().timestamp();
        let stale = local_string(now - (recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1);
        let fresh = local_string(now - 5);
        let stale_activity = (now - (recovery::STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS as i64) - 1)
            .saturating_mul(1_000_000_000);
        let fresh_activity = (now - 5).saturating_mul(1_000_000_000);

        let boot = now - (recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 100;
        assert!(should_reattach_relay_dead_watcher(
            &snapshot(&stale),
            ChannelId::new(42),
            stale_activity,
            now,
            boot,
        ));
        let mut wrong_state = snapshot(&stale);
        wrong_state.relay_stall_state = RelayStallState::ActiveForegroundStream;
        let mut wrong_owner = snapshot(&stale);
        wrong_owner.watcher_owner_channel_id = Some(99);
        let mut committed = snapshot(&stale);
        committed.inflight_terminal_delivery_committed = true;
        let mut fresh_outbound = snapshot(&stale);
        fresh_outbound.relay_health.last_outbound_activity_ms = Some((now - 5) * 1000);
        let mut advanced_frontier = snapshot(&stale);
        advanced_frontier.last_relay_ts_ms = (now - 30) * 1000;
        advanced_frontier.last_relay_offset = 64;
        advanced_frontier.last_capture_offset = Some(128);
        advanced_frontier.unread_bytes = Some(64);
        advanced_frontier.relay_health.last_relay_ts_ms = Some((now - 30) * 1000);
        advanced_frontier.relay_health.last_relay_offset = 64;
        advanced_frontier.relay_health.last_capture_offset = Some(128);
        advanced_frontier.relay_health.unread_bytes = Some(64);
        let recent_boot = now - 5;

        for (name, candidate, activity, boot_unix_secs) in [
            ("wrong stall state", wrong_state, stale_activity, boot),
            ("wrong owner", wrong_owner, stale_activity, boot),
            ("terminal committed", committed, stale_activity, boot),
            ("fresh inflight", snapshot(&fresh), stale_activity, boot),
            (
                "fresh advanced relay frontier",
                advanced_frontier.clone(),
                fresh_activity,
                boot,
            ),
            (
                "post-restart grace",
                snapshot(&stale),
                stale_activity,
                recent_boot,
            ),
        ] {
            assert!(
                !should_reattach_relay_dead_watcher(
                    &candidate,
                    ChannelId::new(42),
                    activity,
                    now,
                    boot_unix_secs,
                ),
                "{name}"
            );
        }
        assert!(
            should_reattach_relay_dead_watcher(
                &snapshot(&stale),
                ChannelId::new(42),
                fresh_activity,
                now,
                boot,
            ),
            "fresh runtime activity is positive liveness for the dead-frontier reattach signature"
        );
        assert!(
            should_reattach_relay_dead_watcher(
                &advanced_frontier,
                ChannelId::new(42),
                stale_activity,
                now,
                boot,
            ),
            "advanced relay frontiers still require stale runtime activity before reattach"
        );
        assert!(
            should_reattach_relay_dead_watcher(
                &fresh_outbound,
                ChannelId::new(42),
                stale_activity,
                now,
                boot,
            ),
            "recent outbound activity must not block non-destructive watcher reattach"
        );
    }
}
