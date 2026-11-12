//! Idle submission keeps focus and stash protection without a screen readiness verdict.
use super::super::busy_inject::screen;

pub(crate) fn focus_modal(capture: &str) -> bool {
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    let normalized =
        super::super::prompt_readiness::normalize_prompt_readiness_panel_in_capture(&plain);
    if super::super::startup_dialog::detect_claude_startup_dialog(&normalized).is_some() {
        return true;
    }
    let rows: Vec<_> = plain.lines().collect();
    let chrome = if screen::input_box(capture) != screen::Composer::Unknown {
        rows.iter()
            .rposition(|row| row.starts_with('❯'))
            .and_then(|prompt| {
                let bottom = (prompt + 1..rows.len()).find(|&index| {
                    let row = rows[index].trim_end();
                    row.chars().count() >= 10 && row.chars().all(|c| c == '─')
                })?;
                Some(
                    rows[..prompt]
                        .iter()
                        .chain(rows[bottom..].iter())
                        .copied()
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            })
    } else {
        None
    };
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(
        chrome.as_deref().unwrap_or(&plain),
    )
}

#[cfg(unix)]
pub(crate) fn before_paste(
    pane: &super::super::busy_inject::Pane,
    stash_allowed: bool,
) -> Result<bool, super::super::busy_inject::Veto> {
    use super::super::{busy_inject::Veto, owner_input};
    use crate::services::tui_input::submission_tmux::guarded::PaneIdentity;
    if owner_input::active() && owner_input::epoch().is_none() {
        let pin =
            PaneIdentity::observe(pane.target().trim_start_matches('=').trim_end_matches(':'))
                .ok_or(Veto::PaneUnavailable)?;
        owner_input::observe_pin(
            pane.target().trim_start_matches('=').trim_end_matches(':'),
            &pin,
        )
        .map_err(|_| Veto::PaneUnavailable)?;
    }
    if pane
        .ok(&["display-message", "-p", "-t", pane.target(), "#{pane_dead}"])
        .is_none_or(|output| !crate::services::platform::tmux::pane_dead_is_live(&output))
    {
        return Err(Veto::PaneUnavailable);
    }
    let proven = stash_allowed || super::super::submission_fence::idle_stash_scope().is_some();
    let Some(capture) = pane.capture() else {
        return if proven {
            Err(Veto::ScreenUnread)
        } else {
            Ok(false)
        };
    };
    if focus_modal(&capture) {
        return Err(Veto::Modal);
    }
    let park = stash_allowed && screen::parkable(&capture);
    if proven && !park && !super::empty_draft(&capture) {
        return Err(Veto::ScreenUnread);
    }
    Ok(park)
}
