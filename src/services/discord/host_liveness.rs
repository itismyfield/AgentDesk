//! Host-aware tmux observations for the liveness and restore consumers. A session whose
//! local host evidence is not tmux is never probed as tmux and never reads dead or absent.

use std::path::Path;
use std::sync::Arc;

use super::SharedData;
use super::host_teardown_gate::shared_teardown;
use super::inflight::{InflightTurnState, KeyedTeardown};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::operation_policy::historical_execution_liveness;
use crate::services::session_host::{
    HostLiveness, HostPresence, HostSessionRef, InteractiveSessionHost, TmuxHost,
};
use crate::services::tmux_common::host_marker::read_host_kind_marker;
use crate::services::tmux_turn_liveness::IndependentTmuxReadiness;

/// Tmux only when neither the `.host_kind` marker nor the inflight row records another
/// host; only the hosted binding writes a row locator, so any locator is such a record.
pub(in crate::services::discord) fn local_tmux(
    name: &str,
    row: Option<&InflightTurnState>,
) -> bool {
    let marker = read_host_kind_marker(name);
    let marker_tmux = crate::services::session_host::operation_policy::marker_is_tmux(&marker);
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

/// A Herdr row's recorded pane on this node: `Missing` once its endpoint's snapshot lacks it,
/// `Alive` while listed; a row not Herdr by marker and locator, or an unread one, is `Unknown`.
pub(in crate::services::discord) fn observe_herdr_liveness(
    name: &str,
    row: &InflightTurnState,
) -> SessionLiveness {
    use super::inflight::PersistedHostLocator;
    use crate::services::session_host::{HostKind, herdr_endpoints};
    use crate::services::tmux_common::host_marker::HostKindMarker;
    let Some(PersistedHostLocator::Known(locator)) = &row.host_locator else {
        return SessionLiveness::Unknown;
    };
    let (HostKind::Herdr, Some(node), Some(pane)) =
        (locator.host_kind, &locator.execution_node, &locator.pane)
    else {
        return SessionLiveness::Unknown;
    };
    if read_host_kind_marker(name) != HostKindMarker::Known(HostKind::Herdr) {
        return SessionLiveness::Unknown;
    }
    match herdr_endpoints().pane_presence(node, &locator.host_session_id, pane) {
        HostPresence::Present => SessionLiveness::Alive,
        HostPresence::Missing => SessionLiveness::Missing,
        HostPresence::ProbeFailed => SessionLiveness::Unknown,
    }
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

/// The keyed gate's verdict before a consumer acts on a tmux answer. Each caller decides
/// whether `RowMissing` may go on: only where a row may never have been written.
pub(in crate::services::discord) async fn tmux_verdict_gate(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    name: &str,
    observed: SessionLiveness,
    caller: &str,
) -> KeyedTeardown {
    let observed = match observed {
        SessionLiveness::Alive => HostLiveness::Live,
        SessionLiveness::Missing => HostLiveness::DeadOrAbsent,
        SessionLiveness::ProbeFailed | SessionLiveness::Unknown => HostLiveness::ProbeError,
    };
    shared_teardown(shared, provider, channel_id, name, Some(observed), caller).await
}

/// A known historical host must confirm death; missing evidence never stands for absence.
pub(in crate::services::discord) fn historical_execution_ended(name: &str) -> bool {
    historical_execution_liveness(name) == Some(HostLiveness::DeadOrAbsent)
}

/// Shared #5176 terminal-evidence conjunction.
///
/// Cancel surfaces pass `release_authorized = token.cancelled`.  The #5068
/// finalizer reconciler passes an exact episode-nonce match between the
/// terminal submission and the residual mailbox token.  Both callers must
/// independently prove that no inflight bridge owns the turn and that the TUI
/// is structurally terminal. Keeping the conjunction here prevents the two
/// recovery surfaces from drifting toward different definitions of a zombie.
pub(in crate::services::discord) const fn terminal_evidence_allows_mailbox_release(
    mailbox_holds_active_turn: bool,
    release_authorized: bool,
    inflight_state_present: bool,
    tui_structurally_idle: Option<bool>,
) -> bool {
    mailbox_holds_active_turn
        && release_authorized
        && !inflight_state_present
        && matches!(tui_structurally_idle, Some(true))
}

/// `Missing` is terminal evidence, not an error: a foreground turn whose tmux
/// session no longer exists cannot be running.
pub(in crate::services::discord) fn readiness_is_structurally_idle(
    readiness: IndependentTmuxReadiness,
) -> bool {
    matches!(
        readiness,
        IndependentTmuxReadiness::Missing | IndependentTmuxReadiness::ReadyForInput
    )
}

/// Probe through the shared readiness authority. An absent or blank binding is
/// unmeasured, so it cannot authorize releasing foreground ownership.
pub(in crate::services::discord) fn tui_structurally_idle(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
) -> Option<bool> {
    let Some(tmux_session) = token
        .tmux_session_name()
        .filter(|session| !session.trim().is_empty())
    else {
        return None;
    };
    // Another host's session is never read as idle from a tmux probe.
    if !local_tmux(&tmux_session, None) {
        return None;
    }
    let runtime_kind =
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&tmux_session);
    let output_path =
        crate::services::tmux_common::resolve_session_temp_path(&tmux_session, "jsonl");
    Some(readiness_is_structurally_idle(
        crate::services::tmux_turn_liveness::independent_tmux_readiness(
            &tmux_session,
            provider,
            runtime_kind,
            output_path.as_deref().map(Path::new),
            None,
        ),
    ))
}
