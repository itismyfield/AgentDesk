//! The owner pin supplements each legacy attachment and geometry condition.
use crate::services::claude_tui::owner_input;
pub(super) fn observe(session: &str) -> Result<(), String> {
    if !owner_input::active() {
        return Ok(());
    }
    let pin = crate::services::tui_input::submission_tmux::guarded::PaneIdentity::observe(session)
        .ok_or("pane identity unavailable")?;
    owner_input::observe_pin(session, &pin)
}
pub(super) fn condition(session: &str, condition: &str) -> Result<String, String> {
    owner_input::condition(session, condition)
}
