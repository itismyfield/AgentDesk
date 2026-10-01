//! Host gate for the watcher's destructive exits: a death, kill or clear acts only on a
//! session the host evidence, the channel's inflight row and sessions row included, leaves tmux.

use super::*;
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
    name: &str,
    channel_id: ChannelId,
) -> bool {
    // The provider the watcher keys its own inflight reads by.
    let provider = parse_provider_and_channel_from_tmux_name(name)
        .map_or(ProviderKind::Claude, |(provider, _)| provider);
    match channel_row(&provider, channel_id, name) {
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

/// A pane tmux confirms dead, or an unanswered probe the wrapper's `.pane_dead` confirms.
pub(in crate::services::discord::tmux::tmux_watcher) fn tmux_pane_dead(
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
) -> bool {
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
) -> bool {
    let Ok(row) = channel_row(provider, channel_id, name) else {
        return false;
    };
    host_liveness::observe_presence(name, row.as_ref()) == Some(HostPresence::Present)
        && tmux_pane_dead(provider, channel_id, name)
}
