//! Host-aware tmux observations for the liveness and restore consumers. A session whose
//! local host evidence is not tmux is never probed as tmux and never reads dead or absent.

use super::SharedData;
use super::host_teardown_gate::shared_teardown;
use super::inflight::{InflightTurnState, KeyedTeardown};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::{
    HostKind, HostLiveness, HostPresence, HostSessionRef, InteractiveSessionHost, TmuxHost,
};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// Tmux only when neither the `.host_kind` marker nor the inflight row records another
/// host; only the hosted binding writes a row locator, so any locator is such a record.
pub(in crate::services::discord) fn local_tmux(
    name: &str,
    row: Option<&InflightTurnState>,
) -> bool {
    let marker = read_host_kind_marker(name);
    let marker_tmux = matches!(
        marker,
        HostKindMarker::Absent | HostKindMarker::Known(HostKind::Tmux)
    );
    let row_tmux =
        row.is_none_or(|row| row.host_locator.is_none() && !row.runtime_kind_unknown_on_disk);
    if !(marker_tmux && row_tmux) {
        tracing::info!(
            name,
            ?marker,
            row_tmux,
            "tmux probe deferred: host is not tmux"
        );
    }
    marker_tmux && row_tmux
}

/// Pane liveness of a local tmux session; anything else is `Unknown` with no probe.
pub(in crate::services::discord) fn observe_liveness(
    name: &str,
    row: Option<&InflightTurnState>,
) -> SessionLiveness {
    if !local_tmux(name, row) {
        return SessionLiveness::Unknown;
    }
    TmuxHost.liveness(HostSessionRef::tmux(name)).into()
}

/// Session presence of a local tmux session; anything else is `None` with no probe.
pub(in crate::services::discord) fn observe_presence(
    name: &str,
    row: Option<&InflightTurnState>,
) -> Option<HostPresence> {
    local_tmux(name, row).then(|| TmuxHost.presence(HostSessionRef::tmux(name)))
}

/// The three-state pane answer existing deciders take; another host reads as a failed probe.
pub(in crate::services::discord) fn as_pane_liveness(liveness: SessionLiveness) -> PaneLiveness {
    match liveness {
        SessionLiveness::Alive => PaneLiveness::Live,
        SessionLiveness::Missing => PaneLiveness::DeadOrAbsent,
        SessionLiveness::ProbeFailed | SessionLiveness::Unknown => PaneLiveness::ProbeError,
    }
}

/// Only a confirmed tmux death reads dead; a failed probe or another host never does.
pub(in crate::services::discord) fn not_dead(liveness: SessionLiveness) -> bool {
    liveness != SessionLiveness::Missing
}

/// The keyed gate before a consumer acts on a tmux verdict. A found legacy row goes on, and
/// so does no row with no other-host trace, as in main: dead legacy sessions lose their rows.
pub(in crate::services::discord) async fn admits_tmux_verdict(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    name: &str,
    observed: SessionLiveness,
    caller: &str,
) -> bool {
    let observed = match observed {
        SessionLiveness::Alive => HostLiveness::Live,
        SessionLiveness::Missing => HostLiveness::DeadOrAbsent,
        SessionLiveness::ProbeFailed | SessionLiveness::Unknown => HostLiveness::ProbeError,
    };
    let gate = shared_teardown(shared, provider, channel_id, name, Some(observed), caller);
    !matches!(gate.await, KeyedTeardown::Kept)
}
