//! Session-scoped composer ownership; idle Discord headers retain their original meaning.
use super::screen;
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::tui_input::human_frame::{holds_inject, shows_inject};

/// How a protected pane's draft reads in one capture.
pub(crate) fn draft_sighting(capture: &str) -> DraftSighting {
    draft_sighting_for(capture, None)
}

pub(crate) fn draft_sighting_in_session(session: &str, capture: &str) -> DraftSighting {
    draft_sighting_for(capture, Some(session))
}

fn draft_sighting_for(capture: &str, session: Option<&str>) -> DraftSighting {
    let screen = screen::read(capture);
    match (screen.stash, screen.composer) {
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Empty) => {
            DraftSighting::Settled
        }
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Text(rows))
            if !rows.iter().any(|row| row.contains("[📱 "))
                && !holds_inject(&rows)
                && !session.is_some_and(|session| {
                    crate::services::tui_input::human_frame::history::screen_pending(
                        session,
                        &rows.join("\n"),
                    )
                }) =>
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
    match screen::typed(capture) {
        Some(false) => ComposerOwner::Empty,
        Some(true) => {
            let screen = screen::read(capture);
            if screen.stash != screen::Stash::AbsentInRecognizedLayout {
                return ComposerOwner::Unread;
            }
            match screen.composer {
                screen::Composer::Text(rows) if agentdesk_prompt(&rows, session) => {
                    ComposerOwner::AgentDesk
                }
                screen::Composer::Empty => ComposerOwner::Unread,
                _ => ComposerOwner::Person,
            }
        }
        None => ComposerOwner::Unread,
    }
}

/// AgentDesk frames every Discord prompt it types as `[User: …]`; a running turn's input left
/// in the composer reads as a person's draft, as the older `[📱 …]` one did.
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
