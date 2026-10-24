//! Session-scoped composer ownership; idle Discord headers retain their original meaning.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use super::{DraftState, Outcome, Report, screen};
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::tui_input::human_frame::{holds_inject, shows_inject};

static PENDING_CHIPS: LazyLock<Mutex<HashMap<(String, String), String>>> =
    LazyLock::new(Default::default);

struct ChipWitness {
    frame: String,
    chip: Option<String>,
}

thread_local! {
    static ACTIVE_CHIP: RefCell<Option<ChipWitness>> = const { RefCell::new(None) };
}

struct ChipScope(Option<ChipWitness>);

impl Drop for ChipScope {
    fn drop(&mut self) {
        ACTIVE_CHIP.with(|active| {
            let _ = active.replace(self.0.take());
        });
    }
}

/// Retains only chips the existing guarded ownership check saw; no further pane read runs.
pub(super) fn with_chip_witness(
    session: &str,
    nonce: &str,
    frame: &str,
    operation: impl FnOnce() -> Report,
) -> Report {
    let previous = ACTIVE_CHIP.with(|active| {
        active.replace(Some(ChipWitness {
            frame: frame.to_string(),
            chip: None,
        }))
    });
    let scope = ChipScope(previous);
    let report = operation();
    let chip = ACTIVE_CHIP.with(|active| {
        active
            .borrow_mut()
            .as_mut()
            .and_then(|active| active.chip.take())
    });
    drop(scope);
    if report.outcome == Outcome::Injected {
        forget_pending_chip(session);
    } else if matches!(report.outcome, Outcome::Unconfirmed(_))
        && report.draft != DraftState::RestoredObserved
        && !nonce.is_empty()
        && nonce.chars().all(|c| c.is_ascii_alphanumeric())
        && let Some(chip) = chip
    {
        let mut pending = PENDING_CHIPS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        pending.retain(|(owner, _), _| owner != session);
        pending.insert((session.to_string(), nonce.to_string()), chip);
    }
    report
}

/// Called only after an exact chip matched in the current guarded capture; denial evidence only.
pub(super) fn observe_owned_chip(frame: &str, chip: &str) {
    ACTIVE_CHIP.with(|active| {
        if let Some(active) = active
            .borrow_mut()
            .as_mut()
            .filter(|active| active.frame == frame)
        {
            active.chip = Some(chip.to_string());
        }
    });
}

#[cfg(test)]
pub(super) fn remember_unconfirmed_chip(session: &str, nonce: &str, frame: &str, capture: &str) {
    with_chip_witness(session, nonce, frame, || {
        let _ = screen::owns_direct(capture, frame, &screen::Drawn::Folded);
        Report::new(
            Outcome::Unconfirmed(super::Unconfirmed::CursorChangedBeforeEnter),
            DraftState::Unknown,
        )
    });
}

pub(super) fn forget_pending_chip(session: &str) {
    PENDING_CHIPS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|(owner, _), _| owner != session);
}

fn pending_chip(session: Option<&str>, screen: &screen::Screen) -> bool {
    let Some(session) = session else { return false };
    if screen.footer != screen::Footer::Normal || screen.composer == screen::Composer::Unknown {
        return false;
    }
    let mut pending = PENDING_CHIPS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let seen = pending.iter().any(|((owner, _), chip)| {
        owner == session
            && matches!(&screen.composer, screen::Composer::Text(rows) if rows.len() == 1 && rows.first() == Some(chip))
    });
    if !seen {
        pending.retain(|(owner, _), _| owner != session);
    }
    seen
}

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
    let pending = pending_chip(session, &screen);
    match (screen.stash, screen.composer) {
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Empty) => {
            DraftSighting::Settled
        }
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Text(rows))
            if !pending && !injected_rows(&rows, session) =>
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
    let pending = pending_chip(session, &screen);
    match screen::typed(capture) {
        Some(false) => ComposerOwner::Empty,
        Some(true) => {
            if screen.stash != screen::Stash::AbsentInRecognizedLayout {
                return ComposerOwner::Unread;
            }
            match screen.composer {
                screen::Composer::Text(rows) if pending || injected_rows(&rows, session) => {
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
