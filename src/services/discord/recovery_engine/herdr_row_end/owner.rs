//! Which durable rows a prior process's Herdr turn owns: new idle starts wait on them, and only
//! their dedicated owners end them, never a generic sweep.

use super::*;
use crate::services::session_host::HostKind;
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// Why a durable row keeps new idle starts off its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum RowHold {
    RestartedHerdrRow,
    /// The row or its host marker did not read; an unread row is never taken for none.
    Unread,
}

impl RowHold {
    pub(in crate::services::discord) fn reason(self) -> &'static str {
        match self {
            Self::RestartedHerdrRow => "restarted_herdr_row",
            Self::Unread => "restarted_herdr_row_unread",
        }
    }
}

/// The channel's fresh durable row, when a prior process's Herdr turn holds it.
pub(in crate::services::discord) fn restarted_herdr_hold(
    provider: &ProviderKind,
    channel: u64,
    current: u64,
) -> Option<RowHold> {
    inflight::inflight_runtime_root()?;
    if !hosted_provider(provider) {
        return None;
    }
    match inflight::load_inflight_state_read_only_result(provider, channel) {
        Ok(row) => restarted_owner(provider, &row?, current),
        Err(_) => Some(RowHold::Unread),
    }
}

/// A prior process's row on a Herdr host: its recorded execution, else its session's host marker.
fn restarted_owner(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    current: u64,
) -> Option<RowHold> {
    if row.channel_id == 0 || inflight::row_is_current_generation(row, current) {
        return None;
    }
    if row.hosted_execution_nonce.is_some() {
        return Some(RowHold::RestartedHerdrRow);
    }
    match read_host_kind_marker(&recovery_tmux_session_name(provider, row)?) {
        HostKindMarker::Known(HostKind::Herdr) => Some(RowHold::RestartedHerdrRow),
        HostKindMarker::Known(_) | HostKindMarker::Absent => None,
        HostKindMarker::Unrecognized(_) | HostKindMarker::ReadFailed(_) => Some(RowHold::Unread),
    }
}

/// A prior process's Herdr row whose end an owner other than a generic sweep settles: the end
/// pass, an admitted terminal, a before-start receipt, a hand-off, a rebind or a replay.
pub(in crate::services::discord) fn dedicated_cleanup_owner(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    current: u64,
) -> bool {
    hosted_provider(provider)
        && restarted_owner(provider, row, current) == Some(RowHold::RestartedHerdrRow)
        && (target(provider, row, current)
            || row.tui_terminal_kind.is_some()
            || row.herdr_before_start_stop.is_some()
            || inflight::before_start_receipt_present(provider, row.channel_id)
            || row.restart_mode == Some(InflightRestartMode::HotSwapHandoff)
            || row.rebind_origin
            || row.replay_rerun_blocked())
}
