//! Host gate for the watcher's destructive exits: a kill, dispatch fail or inflight clear
//! acts on a local tmux session only, and on a pane death only tmux or the wrapper confirms.

use super::*;
use crate::services::discord::host_liveness;
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::HostPresence;

/// Whether `name` is a local tmux session, reading this channel's row only when it names it.
pub(in crate::services::discord::tmux::tmux_watcher) fn watcher_session_is_tmux(
    provider: &ProviderKind,
    channel_id: ChannelId,
    name: &str,
    action: &str,
) -> bool {
    let row = crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
        .filter(|row| row.tmux_session_name.as_deref() == Some(name));
    let tmux = host_liveness::local_tmux(name, row.as_ref());
    if !tmux {
        tracing::info!(
            name,
            action,
            channel_id = channel_id.get(),
            "watcher skipped a tmux teardown: the session host is not tmux"
        );
    }
    tmux
}

/// A pane tmux confirms dead, or an unanswered probe the wrapper's `.pane_dead` confirms.
pub(in crate::services::discord::tmux::tmux_watcher) fn tmux_pane_dead(name: &str) -> bool {
    match host_liveness::observe_liveness(name, None) {
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
pub(in crate::services::discord::tmux::tmux_watcher) fn tmux_dead_pane_present(name: &str) -> bool {
    host_liveness::observe_presence(name, None) == Some(HostPresence::Present)
        && tmux_pane_dead(name)
}
