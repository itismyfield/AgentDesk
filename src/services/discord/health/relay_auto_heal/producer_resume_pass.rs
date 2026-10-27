use super::*;
use crate::services::discord::health::tick_mailboxes::TickMailboxes;

pub(in crate::services::discord::health) async fn run_orphan_token_auto_heal_pass(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    tick: &TickMailboxes,
) -> usize {
    let mut applied = 0usize;
    for (shared, mailboxes) in tick.mailboxes() {
        let mut redrive_channels = std::collections::HashSet::new();
        for (&channel_id, mailbox) in mailboxes {
            redrive_channels.insert(channel_id);
            #[cfg(unix)]
            if provider == &ProviderKind::Codex {
                crate::services::discord::recovery_engine::producer_resume::schedule(
                    shared.clone(),
                    channel_id,
                );
            }
            // An actor that did not answer is unknown, not idle: no cleanup or redrive this tick.
            let Some(mailbox) = mailbox
                .as_ref()
                .ok()
                .filter(|_| !tick.is_unknown(channel_id))
            else {
                continue;
            };
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
            #[cfg(unix)]
            if provider == &ProviderKind::Codex {
                crate::services::discord::recovery_engine::producer_resume::schedule(
                    shared.clone(),
                    channel_id,
                );
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
    applied + relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows(registry, provider).await
}
