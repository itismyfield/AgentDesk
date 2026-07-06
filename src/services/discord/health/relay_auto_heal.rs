use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::{HealthRegistry, stall_liveness};
use crate::services::discord::SharedData;
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::relay_recovery::{
    self, RelayRecoveryActionKind, RelayRecoveryApplySource, RelayRecoveryError,
};
use crate::services::provider::ProviderKind;

pub(super) async fn apply_watchdog_orphan_token_cleanup(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
) -> bool {
    match apply_orphan_pending_token_cleanup(
        registry,
        provider,
        shared,
        channel_id,
        RelayRecoveryApplySource::StallWatchdog,
    )
    .await
    {
        Ok(applied) => applied,
        Err(error) => {
            trace_orphan_auto_heal_error(provider, channel_id, &error);
            false
        }
    }
}

pub(super) async fn run_orphan_token_auto_heal_pass(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
) -> usize {
    let mut applied = 0usize;
    for shared in runtimes {
        let mut redrive_channels = std::collections::HashSet::new();
        let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
        for (channel_id, mailbox) in mailbox_snapshots {
            redrive_channels.insert(channel_id);
            if mailbox.cancel_token.is_some() {
                match apply_orphan_pending_token_cleanup(
                    registry,
                    provider,
                    shared.clone(),
                    channel_id,
                    RelayRecoveryApplySource::ProbeAutoHeal,
                )
                .await
                {
                    Ok(true) => applied += 1,
                    Ok(false) => {}
                    Err(RelayRecoveryError::SnapshotNotFound { .. }) => {}
                    Err(error) => trace_orphan_auto_heal_error(provider, channel_id, &error),
                }
            }

            match redrive_undelivered_backlog(registry, provider, shared.clone(), channel_id).await
            {
                Ok(true) => applied += 1,
                Ok(false) => {}
                Err(RelayRecoveryError::SnapshotNotFound { .. }) => {}
                Err(error) => trace_orphan_auto_heal_error(provider, channel_id, &error),
            }
        }

        let watcher_owner_channels: Vec<ChannelId> = shared
            .tmux_watchers
            .iter()
            .filter_map(|entry| {
                shared
                    .tmux_watchers
                    .owner_channel_for_tmux_session(entry.key())
            })
            .collect();
        for channel_id in watcher_owner_channels {
            if !redrive_channels.insert(channel_id) {
                continue;
            }
            match redrive_undelivered_backlog(registry, provider, shared.clone(), channel_id).await
            {
                Ok(true) => applied += 1,
                Ok(false) => {}
                Err(RelayRecoveryError::SnapshotNotFound { .. }) => {}
                Err(error) => trace_orphan_auto_heal_error(provider, channel_id, &error),
            }
        }
    }
    applied
}

async fn redrive_undelivered_backlog(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
) -> Result<bool, RelayRecoveryError> {
    let Some(snapshot) = registry
        .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
        .await
    else {
        return Ok(false);
    };

    let now_unix_secs = chrono::Utc::now().timestamp();
    if !should_redrive_undelivered_backlog(provider, channel_id, &snapshot, now_unix_secs) {
        return Ok(false);
    }
    if live_relay_frontier_advanced_since_snapshot(&shared, channel_id, &snapshot) {
        return Ok(false);
    }

    if nudge_existing_watcher_for_backlog(&shared, provider, &snapshot, channel_id, now_unix_secs) {
        return Ok(true);
    }
    if live_relay_frontier_advanced_since_snapshot(&shared, channel_id, &snapshot) {
        return Ok(false);
    }

    let response = relay_recovery::auto_apply_relay_recovery_for_shared(
        registry,
        shared,
        provider,
        channel_id.get(),
        RelayRecoveryActionKind::ReattachWatcher,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await?;

    Ok(response.applied)
}

fn has_live_undelivered_backlog(snapshot: &WatcherStateSnapshot) -> bool {
    snapshot.unread_bytes.is_some_and(|bytes| bytes > 0)
        && snapshot.tmux_session_alive == Some(true)
        && !snapshot.inflight_terminal_delivery_committed
}

fn should_redrive_undelivered_backlog(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
) -> bool {
    has_live_undelivered_backlog(snapshot)
        && stall_liveness::stalled_undelivered_backlog_for_redrive(
            provider,
            channel_id,
            snapshot,
            now_unix_secs,
        )
}

fn live_relay_frontier_advanced_since_snapshot(
    shared: &SharedData,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
) -> bool {
    shared.committed_relay_offset(channel_id) > snapshot.last_relay_offset
}

fn nudge_existing_watcher_for_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    snapshot: &WatcherStateSnapshot,
    channel_id: ChannelId,
    now_unix_secs: i64,
) -> bool {
    if !should_redrive_undelivered_backlog(provider, channel_id, snapshot, now_unix_secs) {
        return false;
    }

    let owner_channel_id = snapshot
        .watcher_owner_channel_id
        .map(ChannelId::new)
        .unwrap_or(channel_id);
    let Some(watcher) = shared.tmux_watchers.get(&owner_channel_id) else {
        return false;
    };
    if snapshot.tmux_session.as_deref() != Some(watcher.tmux_session_name.as_str()) {
        return false;
    }
    if snapshot.inflight_output_path.as_deref() != Some(watcher.output_path.as_str()) {
        return false;
    }
    if !nudge_watcher_handle_for_backlog(shared, snapshot, watcher.value(), channel_id) {
        return false;
    }

    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        channel_id = channel_id.get(),
        watcher_owner_channel_id = owner_channel_id.get(),
        tmux_session = %watcher.tmux_session_name,
        output_path = %watcher.output_path,
        last_relay_offset = snapshot.last_relay_offset,
        unread_bytes = ?snapshot.unread_bytes,
        "redrive nudged existing tmux watcher to re-read undelivered backlog from confirmed frontier"
    );
    true
}

fn nudge_watcher_handle_for_backlog(
    shared: &SharedData,
    snapshot: &WatcherStateSnapshot,
    watcher: &crate::services::discord::TmuxWatcherHandle,
    channel_id: ChannelId,
) -> bool {
    if watcher.cancel.load(Ordering::Relaxed)
        || watcher.heartbeat_stale()
        || watcher.paused.load(Ordering::Relaxed)
    {
        return false;
    }
    let Ok(mut resume_offset) = watcher.resume_offset.lock() else {
        return false;
    };
    if live_relay_frontier_advanced_since_snapshot(shared, channel_id, snapshot) {
        return false;
    }
    *resume_offset = Some(snapshot.last_relay_offset);
    watcher.turn_delivered.store(false, Ordering::Release);
    true
}

async fn apply_orphan_pending_token_cleanup(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    source: RelayRecoveryApplySource,
) -> Result<bool, RelayRecoveryError> {
    if source == RelayRecoveryApplySource::StallWatchdog
        && let Some((_, watcher)) = shared.tmux_watchers.remove(&channel_id)
    {
        watcher.cancel.store(true, Ordering::Relaxed);
    }

    if source == RelayRecoveryApplySource::ProbeAutoHeal {
        let Some(snapshot) = registry
            .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
            .await
        else {
            return Ok(false);
        };
        if snapshot.relay_stall_state != RelayStallState::OrphanPendingToken {
            return Ok(false);
        }
    }

    let response = relay_recovery::auto_apply_relay_recovery_for_shared(
        registry,
        shared,
        provider,
        channel_id.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        source,
    )
    .await?;

    Ok(response.applied
        && response
            .apply_result
            .as_ref()
            .is_some_and(|result| result.removed_mailbox_token))
}

fn trace_orphan_auto_heal_error(
    provider: &ProviderKind,
    channel_id: ChannelId,
    error: &RelayRecoveryError,
) {
    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        status = error.status_str(),
        body = %error.body(),
        "relay recovery auto-heal skipped"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};

    use super::*;

    fn watcher_handle(
        tmux_session_name: &str,
        output_path: &str,
        resume_offset: Arc<Mutex<Option<u64>>>,
        turn_delivered: Arc<AtomicBool>,
    ) -> crate::services::discord::TmuxWatcherHandle {
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset,
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered,
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        }
    }

    fn backlog_snapshot(
        channel_id: ChannelId,
        tmux_session: &str,
        output_path: &str,
        last_relay_offset: u64,
        capture_offset: u64,
    ) -> WatcherStateSnapshot {
        let unread_bytes = capture_offset.saturating_sub(last_relay_offset);
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some(tmux_session.to_string()),
            watcher_owner_channel_id: Some(channel_id.get()),
            last_relay_offset,
            inflight_state_present: true,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: Some(capture_offset),
            unread_bytes: Some(unread_bytes),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_updated_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(9001),
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some(output_path.to_string()),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: ProviderKind::Codex.as_str().to_string(),
                channel_id: channel_id.get(),
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(channel_id.get()),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(9002),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(9001),
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(9002),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: Some(1_700_000_000_000),
                last_outbound_activity_ms: None,
                last_capture_offset: Some(capture_offset),
                last_relay_offset,
                unread_bytes: Some(unread_bytes),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    #[test]
    fn redrive_nudge_skips_healthy_advancing_backlog() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_178_301);
        let tmux_session = "AgentDesk-codex-4178-healthy-drain";
        let output_path = "/tmp/agentdesk-4178-healthy-drain.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, capture_offset);
        assert!(!nudge_existing_watcher_for_backlog(
            &shared, &provider, &snapshot, channel_id, now,
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        let advanced_snapshot =
            backlog_snapshot(channel_id, tmux_session, output_path, 256, capture_offset);
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &advanced_snapshot,
            channel_id,
            now + 30,
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }

    #[test]
    fn redrive_nudge_requires_matching_output_path() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_178_302);
        let tmux_session = "AgentDesk-codex-4178-output-path";
        let watcher_output_path = "/tmp/agentdesk-4178-watcher.jsonl";
        let inflight_output_path = "/tmp/agentdesk-4178-inflight.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            watcher_output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(
            channel_id,
            tmux_session,
            inflight_output_path,
            128,
            capture_offset,
        );
        assert!(!nudge_existing_watcher_for_backlog(
            &shared, &provider, &snapshot, channel_id, now,
        ));
        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }

    #[test]
    fn redrive_nudge_skips_if_live_frontier_advanced_after_snapshot() {
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_178_303);
        let tmux_session = "AgentDesk-codex-4178-live-frontier";
        let output_path = "/tmp/agentdesk-4178-live-frontier.jsonl";
        let shared = crate::services::discord::make_shared_data_for_tests();
        let resume_offset = Arc::new(Mutex::new(None));
        let turn_delivered = Arc::new(AtomicBool::new(true));
        let watcher = watcher_handle(
            tmux_session,
            output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        );
        shared.tmux_watchers.insert(channel_id, watcher);
        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );

        let capture_offset = 301_613;
        let now = 1_800_000_000;
        let snapshot = backlog_snapshot(channel_id, tmux_session, output_path, 128, capture_offset);
        assert!(!nudge_existing_watcher_for_backlog(
            &shared, &provider, &snapshot, channel_id, now,
        ));
        shared
            .tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .store(256, Ordering::Release);

        assert!(!nudge_existing_watcher_for_backlog(
            &shared,
            &provider,
            &snapshot,
            channel_id,
            now + stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
        ));
        assert_eq!(*resume_offset.lock().unwrap(), None);
        assert!(turn_delivered.load(Ordering::Acquire));

        stall_liveness::clear_stall_watchdog_liveness_state(
            &provider,
            channel_id,
            Some(tmux_session),
        );
    }
}
