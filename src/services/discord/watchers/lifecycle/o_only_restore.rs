use super::*;

pub(in crate::services::discord) async fn install_mapped_retired_o_watcher(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    session_name: &str,
) {
    let now = chrono::Utc::now().timestamp();
    if !crate::services::discord::health::retired_o_install_due(provider, channel_id, now) {
        return;
    }
    let outcome = install_retired_o_watcher(OOnlyInstallRequest {
        runtime: shared.clone(),
        http: http.clone(),
        provider: provider.clone(),
        channel_id,
        session_name: session_name.to_owned(),
    })
    .await;
    crate::services::discord::health::record_retired_o_install_outcome(
        provider, channel_id, outcome, now,
    );
}

pub struct RetiredOWatcherRestore {
    pub(crate) http: Arc<serenity::Http>,
    pub(crate) shared: Arc<SharedData>,
}

/// Prepares cold-registry O watchers from durable bindings; boot callers remain unconnected.
pub async fn restore_retired_o_watchers(request: &RetiredOWatcherRestore) {
    let http = &request.http;
    let shared = &request.shared;
    let provider = shared.settings.read().await.provider.clone();
    let sessions = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(sessions))) => sessions,
        _ => return,
    };
    for session in sessions {
        let session = session.trim();
        let Some((session_provider, _)) = parse_provider_and_channel_from_tmux_name(session) else {
            continue;
        };
        if session_provider != provider {
            continue;
        }
        let Some(channel) = crate::services::tmux_common::read_tmux_channel_binding(session) else {
            continue;
        };
        if !crate::services::discord::health::legacy_supervision::is_retired(
            provider.as_str(),
            channel,
        ) {
            continue;
        }
        install_mapped_retired_o_watcher(http, shared, &provider, ChannelId::new(channel), session)
            .await;
    }
}

#[cfg(all(test, unix))]
#[path = "n4d2_restore_tests.rs"]
mod tests;
