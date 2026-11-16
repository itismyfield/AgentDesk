//! A Herdr canary whose old verified tmux pin keeps its writer blocked may launch a new execution,
//! and only while the old pane and its launch files are vacant. Nothing here writes.

use super::super::{SourcePolicyState, disposition, verified_panes};
use super::absent_open;
use crate::services::codex_tui::canary::CANARY_TMUX;
use crate::services::herdr_launch::herdr_canary;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker;

/// The pinned nonce when `channel` is the Herdr canary that only its old verified pin blocks, with
/// no spawn marker, no owned rollout marker and no tmux pane left.
pub(crate) fn local_eligible(channel: u64) -> Result<String, &'static str> {
    if !herdr_canary(channel) {
        return Err("not_herdr_canary");
    }
    if verified_panes(channel) != [CANARY_TMUX] {
        return Err("pane_registered");
    }
    tc::with_tmux_source_authority(CANARY_TMUX, |authority| {
        vacancy(authority.session())?;
        let pin = super::capture_under(authority)
            .map_err(|_| "pin_unreadable")?
            .ok_or("no_verified_pin")?;
        match disposition(authority) {
            SourcePolicyState::VerifiedUnavailable => Ok(pin.nonce),
            _ => Err("source_not_pin_blocked"),
        }
    })
}

/// Read again under the launch fence before the Pending row: a pane, process or marker of
/// `logical` that appeared since the turn was judged refuses the launch.
pub(crate) fn vacant_recheck(logical: &str) -> Result<(), String> {
    tc::with_tmux_source_authority(logical, |authority| vacancy(authority.session()))
        .map_err(|reason| format!("herdr bootstrap: target not vacant ({reason})"))
}

fn vacancy(tmux: &str) -> Result<(), &'static str> {
    if absent_open::read_nonce(tmux) != SpawnNonceMarker::Absent {
        return Err("spawn_nonce_present");
    }
    if !absent_open::rollout_unowned(tmux) {
        return Err("rollout_owned");
    }
    if !absent_open::target_absent(tmux) {
        return Err("target_present");
    }
    Ok(())
}
