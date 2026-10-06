use super::*;

pub(in crate::services::discord) async fn complete_force_clean_watcher_recovery(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
    repair_started_at: Instant,
) {
    let Ok(recovery) = discord::live_bridge::try_respawn_recovery(provider, channel_id.get())
    else {
        return;
    };
    recovery
        .run(complete_force_clean_watcher_recovery_admitted(
            registry,
            provider,
            shared,
            channel_id,
            snapshot,
            now_unix_secs,
            repair_started_at,
        ))
        .await;
}

pub(in crate::services::discord) async fn retry_pending_watcher_respawn(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    channel_id: ChannelId,
    now_unix_secs: i64,
) -> bool {
    let Ok(recovery) = discord::live_bridge::try_respawn_recovery(provider, channel_id.get())
    else {
        // A live Claude original owns the turn; a concurrent recovery keeps this entry's budget.
        if matches!(provider, ProviderKind::Claude)
            && discord::live_bridge::is_live(provider, channel_id.get())
        {
            clear_watcher_absence(provider, channel_id);
        }
        return false;
    };
    recovery
        .run(retry_pending_watcher_respawn_admitted(
            registry,
            provider,
            runtimes,
            channel_id,
            now_unix_secs,
        ))
        .await
}
