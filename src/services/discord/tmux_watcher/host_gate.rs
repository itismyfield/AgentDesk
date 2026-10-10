//! Host gate for the watcher's destructive exits: a death, kill or clear acts only on a
//! session the host evidence, the channel's inflight row and sessions row included, leaves tmux.

use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::discord::host_liveness;
use crate::services::discord::host_teardown_gate::shared_teardown;
use crate::services::discord::inflight::{KeyedTeardown, load_inflight_state_read_only_result};
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::HostPresence;

/// The channel's inflight row as evidence about `name`; a row naming another session is none.
fn channel_row(
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
) -> Result<Option<InflightTurnState>, String> {
    let row = load_inflight_state_read_only_result(provider, channel_id.get())?;
    Ok(row.filter(|row| row.tmux_session_name.as_deref().is_none_or(|n| n == name)))
}

/// The watcher's liveness probe with the host the channel's row records; an unread row keeps it.
pub(in crate::services::discord::tmux::tmux_watcher) async fn tmux_alive(
    shared: &SharedData,
    name: &str,
    channel_id: ChannelId,
    host: &HostSnapshot,
) -> bool {
    host_alive(shared, name, channel_id, host, row_probe(name, channel_id)).await
}

/// Retired watchers require independent tmux and native-source evidence before probing.
pub(in crate::services::discord::tmux::tmux_watcher) async fn tmux_alive_for_mode(
    shared: &SharedData,
    name: &str,
    channel_id: ChannelId,
    host: &HostSnapshot,
    legacy_mode: WatcherLegacyMode,
    output_path: &str,
) -> bool {
    #[cfg(test)]
    crate::services::discord::inflight::o_seed_observation::record_event(&watcher_provider(name), channel_id.get(), "host_probe");
    if legacy_mode.is_legacy() {
        return tmux_alive(shared, name, channel_id, host).await;
    }
    let provider = watcher_provider(name);
    if watch_host_of(shared, &provider, channel_id.get(), name).await != WatchHost::Legacy {
        return true;
    }
    let Some(evidence) = retired_tmux_evidence(name, output_path, host) else {
        return true;
    };
    let name_owned = name.to_string();
    #[cfg(test)]
    crate::services::discord::inflight::o_seed_observation::record_event(&watcher_provider(name), channel_id.get(), "rowless_host_probe");
    let observed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || host_liveness::observe_liveness(&name_owned, None)),
    )
    .await;
    if !matches!(observed, Ok(Ok(SessionLiveness::Missing))) {
        return true;
    }
    #[cfg(all(test, unix))]
    retired_probe_test::after_probe(channel_id.get()).await;
    watch_host_of(shared, &provider, channel_id.get(), name).await != WatchHost::Legacy
        || retired_tmux_evidence(name, output_path, host).as_ref() != Some(&evidence)
}

#[cfg(all(test, unix))]
pub(in crate::services::discord::tmux::tmux_watcher) mod retired_probe_test {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use tokio::sync::oneshot;

    type Pause = (oneshot::Sender<()>, oneshot::Receiver<()>);
    static PAUSES: LazyLock<Mutex<HashMap<u64, Pause>>> = LazyLock::new(Mutex::default);

    pub(in crate::services::discord::tmux::tmux_watcher) fn pause_after_probe(channel: u64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, entry) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        assert!(PAUSES.lock().unwrap().insert(channel, (entered, resume)).is_none());
        (entry, release)
    }

    pub(super) async fn after_probe(channel: u64) {
        let pause = PAUSES.lock().unwrap().remove(&channel);
        if let Some((entered, resume)) = pause {
            let _ = entered.send(());
            let _ = resume.await;
        }
    }
}

fn retired_tmux_evidence(
    name: &str,
    output_path: &str,
    host: &HostSnapshot,
) -> Option<(crate::services::tui_prompt_dedupe::TuiRuntimeBinding, SourceFileIdentity)> {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
    use crate::services::session_host::HostKind;
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
    if host.refresh_sync(name) != WatchHost::Legacy
        || read_host_kind_marker(name) != HostKindMarker::Known(HostKind::Tmux)
    {
        return None;
    }
    let Ok(Some(binding)) = crate::services::tui_prompt_dedupe::try_peek_tmux_runtime_binding(name)
    else {
        return None;
    };
    let valid = matches!(
        (parse_provider_and_channel_from_tmux_name(name).map(|(p, _)| p), binding.runtime_kind),
        (Some(ProviderKind::Claude), ClaudeTui) | (Some(ProviderKind::Codex), CodexTui)
    ) && crate::services::tmux_common::resolve_tmux_runtime_kind_marker(name)
        == Some(binding.runtime_kind)
        && binding.output_path == output_path
        && binding.session_id.as_deref().is_some_and(|id| !id.trim().is_empty());
    if !valid {
        return None;
    }
    let file = std::fs::File::open(output_path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let source = SourceFileIdentity::from_open_file(&file);
    if source == SourceFileIdentity::Unavailable {
        return None;
    }
    Some((binding, source))
}

async fn row_probe(name: &str, channel_id: ChannelId) -> bool {
    match channel_row(&watcher_provider(name), channel_id, name) {
        Ok(row) => probe_tmux_session_liveness_with_row(name, row).await,
        Err(error) => {
            tracing::info!(
                name,
                error,
                "watcher kept the session: inflight row unreadable"
            );
            true
        }
    }
}

/// [`host_alive`] on the marker-only probe of a session the watcher holds no row for.
pub(in crate::services::discord::tmux::tmux_watcher) async fn marker_alive(
    shared: &SharedData,
    name: &str,
    channel_id: ChannelId,
    host: &HostSnapshot,
) -> bool {
    let probe = probe_tmux_session_liveness(name);
    host_alive(shared, name, channel_id, host, probe).await
}

/// The completion sniff of the background-agent footer; off tmux the pane reads not pending,
/// as a failed capture does.
pub(in crate::services::discord::tmux::tmux_watcher) async fn background_agent_pending(
    host: &HostSnapshot,
    name: Option<String>,
) -> bool {
    let herdr = name
        .as_deref()
        .is_some_and(|n| host.refresh_sync(n) == WatchHost::Herdr);
    let sniff = crate::services::discord::tmux::sniff_background_agent_pending_for_completion;
    !herdr && sniff(name.as_deref()).await
}

/// `probe` decides only off Herdr, which is never probed as tmux; a death it reports stands
/// unless a re-read of an unverified sessions row names Herdr.
pub(in crate::services::discord::tmux::tmux_watcher) async fn host_alive(
    shared: &SharedData,
    name: &str,
    channel_id: ChannelId,
    host: &HostSnapshot,
    probe: impl std::future::Future<Output = bool>,
) -> bool {
    if host.refresh_sync(name) == WatchHost::Herdr {
        tracing::debug!(name, "watcher kept the session: host is Herdr");
        return true;
    }
    if probe.await {
        return true;
    }
    let provider = watcher_provider(name);
    let stands = host
        .death_stands(shared, &provider, channel_id.get(), name)
        .await;
    if !stands {
        tracing::info!(
            name,
            "watcher kept the session: its sessions row names Herdr"
        );
    }
    !stands
}

/// The provider the watcher keys its own inflight reads by.
fn watcher_provider(name: &str) -> ProviderKind {
    parse_provider_and_channel_from_tmux_name(name).map_or(ProviderKind::Claude, |(p, _)| p)
}

/// The keyed host verdict before the watcher kills or clears `name`; `false` keeps it. A missing
/// sessions row goes on: a session reacquired at restart never ran the best-effort row write.
pub(in crate::services::discord::tmux::tmux_watcher) async fn admits_teardown(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
    action: &str,
) -> bool {
    match shared_teardown(shared, provider, channel_id.get(), name, None, action).await {
        KeyedTeardown::Cleared(_) | KeyedTeardown::RowMissing => true,
        KeyedTeardown::Kept => false,
    }
}

/// [`admits_teardown`] for a kill the watcher starts on its own; a Herdr-configured channel's
/// session is kept whatever its rows say.
pub(in crate::services::discord::tmux::tmux_watcher) async fn admits_automatic_kill(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
    action: &str,
) -> bool {
    let configured = crate::services::discord::admin_host_guard::configured_refusal;
    configured(channel_id.get()).is_none()
        && admits_teardown(shared, provider, channel_id, name, action).await
}

/// A pane tmux confirms dead, or an unanswered probe the wrapper's `.pane_dead` confirms.
/// Herdr is never dead here; the keyed teardown gate before it already read the sessions row.
pub(in crate::services::discord::tmux::tmux_watcher) fn tmux_pane_dead(
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
    host: &HostSnapshot,
) -> bool {
    if host.refresh_sync(name) == WatchHost::Herdr {
        return false;
    }
    let Ok(row) = channel_row(provider, channel_id, name) else {
        return false;
    };
    match host_liveness::observe_liveness(name, row.as_ref()) {
        SessionLiveness::Missing => true,
        SessionLiveness::ProbeFailed if tmux_dead_marker_exists(name) => true,
        SessionLiveness::ProbeFailed => {
            tracing::info!(
                name,
                "watcher kept the pane: the tmux probe went unanswered"
            );
            false
        }
        SessionLiveness::Alive | SessionLiveness::Unknown => false,
    }
}

/// A session tmux confirms present whose panes read dead by [`tmux_pane_dead`].
pub(in crate::services::discord::tmux::tmux_watcher) fn tmux_dead_pane_present(
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
    host: &HostSnapshot,
) -> bool {
    if host.refresh_sync(name) == WatchHost::Herdr {
        return false;
    }
    let Ok(row) = channel_row(provider, channel_id, name) else {
        return false;
    };
    host_liveness::observe_presence(name, row.as_ref()) == Some(HostPresence::Present)
        && tmux_pane_dead(provider, channel_id, name, host)
}
