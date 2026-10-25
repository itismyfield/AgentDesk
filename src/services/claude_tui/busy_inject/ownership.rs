//! Session-scoped composer ownership; idle Discord headers retain their original meaning.
use super::screen;
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::tui_input::human_frame::{holds_inject, shows_inject};

fn injected_rows(rows: &[String], session: Option<&str>) -> bool {
    rows.iter().any(|row| row.contains("[📱 "))
        || holds_inject(rows)
        || session.is_some_and(|session| {
            crate::services::tui_input::human_frame::history::screen_pending(
                session,
                &rows.join("\n"),
            )
        })
}

/// How a protected pane's draft reads in one capture.
pub(crate) fn draft_sighting(capture: &str) -> DraftSighting {
    draft_sighting_for(capture, None)
}

pub(crate) fn draft_sighting_in_session(session: &str, capture: &str) -> DraftSighting {
    draft_sighting_for(capture, Some(session))
}

fn draft_sighting_for(capture: &str, session: Option<&str>) -> DraftSighting {
    let screen = screen::read(capture);
    if screen.footer != screen::Footer::Normal {
        return DraftSighting::Unsettled;
    }
    match (screen.stash, screen.composer) {
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Empty) => {
            DraftSighting::Settled
        }
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Text(rows))
            if !injected_rows(&rows, session) =>
        {
            DraftSighting::PersonDraft
        }
        _ => DraftSighting::Unsettled,
    }
}

/// Whose text the composer holds before an automatic key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposerOwner {
    /// Nothing typed: an empty `❯` or a faint placeholder.
    Empty,
    /// A Discord prompt AgentDesk typed (`[User: …]`) that never left the composer.
    AgentDesk,
    /// Typed text in the measured layout whose removal a later capture can see.
    Person,
    Unread,
}

pub(crate) fn composer_owner(capture: &str) -> ComposerOwner {
    composer_owner_for(capture, None)
}

pub(crate) fn composer_owner_in_session(session: &str, capture: &str) -> ComposerOwner {
    composer_owner_for(capture, Some(session))
}

fn composer_owner_for(capture: &str, session: Option<&str>) -> ComposerOwner {
    let screen = screen::read(capture);
    // The transient hint only proves a live paste; keep the older empty-composer fallback.
    if screen.footer == screen::Footer::PasteExpand {
        return ComposerOwner::Unread;
    }
    match screen::typed(capture) {
        Some(false) => ComposerOwner::Empty,
        Some(true) => {
            if screen.stash != screen::Stash::AbsentInRecognizedLayout {
                return ComposerOwner::Unread;
            }
            match screen.composer {
                screen::Composer::Text(rows) if injected_rows(&rows, session) => {
                    ComposerOwner::Unread
                }
                screen::Composer::Text(rows) if agentdesk_prompt(&rows, session) => {
                    ComposerOwner::AgentDesk
                }
                screen::Composer::Text(_) => ComposerOwner::Person,
                screen::Composer::Empty | screen::Composer::Unknown => ComposerOwner::Unread,
            }
        }
        None => ComposerOwner::Unread,
    }
}

/// Ordinary idle Discord headers keep their cleanup meaning only without pending frame evidence.
fn agentdesk_prompt(rows: &[String], session: Option<&str>) -> bool {
    let first = rows.first().map(|row| row.trim_start()).unwrap_or_default();
    first
        .get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("[User:"))
        && !shows_inject(rows)
        && !session.is_some_and(|session| {
            crate::services::tui_input::human_frame::history::screen_pending(
                session,
                &rows.join("\n"),
            )
        })
}
