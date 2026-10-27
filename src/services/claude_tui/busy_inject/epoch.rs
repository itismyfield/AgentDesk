//! Capture owner identity before the one composer read; post-paste checks stay absent.
use crate::services::claude_tui::owner_input;
pub(super) fn observe(session: &str) -> Result<(), String> {
    if !owner_input::active() || owner_input::epoch().is_some() {
        return Ok(());
    }
    let pin = crate::services::tui_input::submission_tmux::guarded::PaneIdentity::observe(session)
        .ok_or("pane identity unavailable")?;
    owner_input::observe_pin(session, &pin)
}
