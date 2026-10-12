use super::*;
use crate::services::discord::tmux::{
    OOnlyInstallOutcome, OOnlyInstallReason, OOnlyInstallRequest, install_retired_o_watcher,
};
use crate::services::tmux_common as tc;

pub(in crate::services::discord) fn retired_o_install_due(
    provider: &ProviderKind,
    channel: ChannelId,
    now: i64,
) -> bool {
    !WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(provider, channel))
        || respawn_attempt_due(provider, channel, now)
}

pub(in crate::services::discord) fn record_retired_o_install_outcome(
    provider: &ProviderKind,
    channel: ChannelId,
    outcome: OOnlyInstallOutcome,
    now: i64,
) {
    match outcome {
        OOnlyInstallOutcome::Spawned | OOnlyInstallOutcome::AlreadyLive => {
            clear_watcher_absence(provider, channel);
        }
        OOnlyInstallOutcome::Deferred(reason) | OOnlyInstallOutcome::Failed(reason) => {
            let mut entry = WATCHER_ABSENCE
                .entry(WatcherAbsenceKey::new(provider, channel))
                .or_insert_with(|| WatcherAbsenceState::newly_absent(now));
            entry.last_seen_unix_secs = now;
            entry.o_install_reason = Some(reason);
            if reason == OOnlyInstallReason::NotRetired {
                tracing::error!(
                    channel_id = channel.get(),
                    "retired O adapter lost its retirement membership"
                );
            }
            drop(entry);
            record_failed_respawn_attempt(provider, channel, now);
            observe_absence(provider, channel, now);
        }
    }
}

pub(super) fn observe_absence(provider: &ProviderKind, channel: ChannelId, now: i64) {
    let mut entry = WATCHER_ABSENCE
        .entry(WatcherAbsenceKey::new(provider, channel))
        .or_insert_with(|| WatcherAbsenceState::newly_absent(now));
    entry.last_seen_unix_secs = now;
    if saturating_age_secs(entry.first_seen_unix_secs, now) >= WATCHER_ABSENCE_DEADMAN_SECS
        && !entry.escalated
    {
        entry.escalated = true;
        tracing::error!(channel_id = channel.get(), provider = provider.as_str(),
            reason = ?entry.o_install_reason, "retired O watcher absence reached the dead-man deadline");
    }
}

async fn request(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    channel: ChannelId,
) -> Result<OOnlyInstallRequest, OOnlyInstallReason> {
    let live = tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names)
        .await
        .map_err(|_| OOnlyInstallReason::SourceUnavailable)?
        .map_err(|_| OOnlyInstallReason::SourceUnavailable)?;
    let kind = idle_relay_absence::tui_runtime_kind_for(provider)
        .ok_or(OOnlyInstallReason::InvalidTarget)?;
    let candidates: Vec<_> = live
        .into_iter()
        .filter(|session| {
            crate::services::provider::parse_provider_and_channel_from_tmux_name(session)
                .is_some_and(|(parsed, _)| parsed == *provider)
                && tc::read_tmux_channel_binding(session) == Some(channel.get())
                && tc::resolve_tmux_runtime_kind_marker(session) == Some(kind)
                && discord::tmux::session_belongs_to_current_runtime(
                    session,
                    &tc::current_tmux_owner_marker(),
                )
        })
        .collect();
    let session = match candidates.as_slice() {
        [session] => session.clone(),
        [] => return Err(OOnlyInstallReason::SourceUnavailable),
        _ => return Err(OOnlyInstallReason::InvalidTarget),
    };
    let (_, name) = crate::services::provider::parse_provider_and_channel_from_tmux_name(&session)
        .ok_or(OOnlyInstallReason::InvalidTarget)?;
    let mut owners = Vec::new();
    for runtime in runtimes {
        let settings = runtime
            .settings
            .try_read()
            .map_err(|_| OOnlyInstallReason::OwnershipUnavailable)?;
        if settings.provider == *provider
            && discord::validate_bot_channel_routing_with_provider_channel(
                &settings,
                provider,
                channel,
                Some(&name),
                Some(&name),
                name.starts_with("dm-"),
            )
            .is_ok()
        {
            owners.push(runtime.clone());
        }
    }
    let [runtime] = owners.as_slice() else {
        return Err(OOnlyInstallReason::OwnershipUnavailable);
    };
    let http = match runtime.serenity_http_or_token_fallback() {
        Some(http) => http,
        None => {
            let registered = registry.all_shared_for_provider(provider).await;
            if registered.len() != 1 || !Arc::ptr_eq(&registered[0], runtime) {
                return Err(OOnlyInstallReason::OwnershipUnavailable);
            }
            super::super::resolve_bot_http(registry, provider.as_str())
                .await
                .map_err(|_| OOnlyInstallReason::OwnershipUnavailable)?
        }
    };
    Ok(OOnlyInstallRequest {
        runtime: runtime.clone(),
        http,
        provider: provider.clone(),
        channel_id: channel,
        session_name: session,
    })
}

pub(super) async fn install_for_channel(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    channel: ChannelId,
    now: i64,
) -> bool {
    if !retired_o_install_due(provider, channel, now) {
        return false;
    }
    let outcome = match request(registry, provider, runtimes, channel).await {
        Ok(request) => install_retired_o_watcher(request).await,
        Err(OOnlyInstallReason::InvalidTarget) => {
            OOnlyInstallOutcome::Failed(OOnlyInstallReason::InvalidTarget)
        }
        Err(reason) => OOnlyInstallOutcome::Deferred(reason),
    };
    record_retired_o_install_outcome(provider, channel, outcome, now);
    true
}

pub(super) async fn retry(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    channel: ChannelId,
    now: i64,
) -> bool {
    // Only the installer's current source/owner validation can clear this rowless obligation.
    if !respawn_attempt_due(provider, channel, now) {
        return false;
    }
    install_for_channel(registry, provider, runtimes, channel, now).await
}

#[cfg(test)]
#[path = "o_only_retry_tests.rs"]
mod tests;
