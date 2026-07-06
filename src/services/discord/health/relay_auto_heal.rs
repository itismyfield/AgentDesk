use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::ChannelId;

use super::HealthRegistry;
use super::snapshot::WatcherStateSnapshot;
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

    if !has_live_undelivered_backlog(&snapshot) {
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
