use super::codex_tui_existing_inflight_cursor_is_raw_rollout;
use crate::services::discord::inflight::InflightTurnState;

pub(super) fn matching_raw_cursor(
    tmux_session_name: &str,
    existing: Option<&InflightTurnState>,
    rollout_path: Option<&str>,
) -> Option<String> {
    let existing = existing?;
    if !codex_tui_existing_inflight_cursor_is_raw_rollout(tmux_session_name, existing) {
        return None;
    }
    let saved = existing.output_path.as_deref()?;
    let rollout = std::fs::canonicalize(rollout_path?).ok()?;
    if !rollout.is_file() || std::fs::canonicalize(saved).ok()? != rollout {
        return None;
    }
    // Keep the incumbent's path spelling so canonical aliases reuse its claim.
    Some(saved.to_owned())
}
