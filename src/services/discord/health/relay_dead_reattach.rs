use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::{HealthRegistry, recovery, stall_liveness};
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::{self, SharedData};
use crate::services::provider::ProviderKind;

fn outbound_activity_is_recent(
    last_outbound_activity_ms: Option<i64>,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(last_outbound_activity_ms) = last_outbound_activity_ms else {
        return false;
    };
    let now_ms = now_unix_secs.saturating_mul(1000);
    if last_outbound_activity_ms >= now_ms {
        return true;
    }
    let age_ms = now_ms.saturating_sub(last_outbound_activity_ms) as u64;
    age_ms < threshold_secs.saturating_mul(1000)
}

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
        || outbound_activity_is_recent(
            snapshot.relay_health.last_outbound_activity_ms,
            now_unix_secs,
            recovery::STALL_WATCHDOG_THRESHOLD_SECS,
        )
    {
        return false;
    }
    if !recovery::stall_watchdog_should_force_clean(
        snapshot.attached,
        true,
        snapshot.inflight_terminal_delivery_committed,
        snapshot.inflight_started_at.as_deref(),
        now_unix_secs,
        recovery::STALL_WATCHDOG_THRESHOLD_SECS,
        boot_unix_secs,
    ) {
        return false;
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
            inflight_terminal_delivery_committed: false,
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
    fn relay_dead_watcher_reattach_requires_stale_turn_and_stale_jsonl() {
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
        let recent_boot = now - 5;

        for (name, candidate, activity, boot_unix_secs) in [
            ("wrong stall state", wrong_state, stale_activity, boot),
            ("wrong owner", wrong_owner, stale_activity, boot),
            ("terminal committed", committed, stale_activity, boot),
            ("fresh inflight", snapshot(&fresh), stale_activity, boot),
            (
                "fresh runtime activity",
                snapshot(&stale),
                fresh_activity,
                boot,
            ),
            ("fresh outbound", fresh_outbound, stale_activity, boot),
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
    }
}
