//! A wait's first look at its source, and what the end of its budget makes of it.
use super::evidence::{self, Stamp};
use super::source::Source;
use super::{clear, ledger, observe};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One re-evaluation window, and how many a single wait may open.
pub(crate) const WINDOW: Duration = Duration::from_secs(2);
const WINDOWS: u8 = 3;

pub(crate) struct Watched {
    session: String,
    path: Option<PathBuf>,
    seen: Option<(Source, Option<Stamp>)>,
    /// Windows this wait opened after its transcript moved.
    windows: u8,
}
pub(crate) fn watched(session: &str, path: Option<&Path>) -> Watched {
    let seen = Source::current(session, path).map(|source| {
        let stamp = Stamp::read(&source.path);
        (source, stamp)
    });
    Watched {
        session: session.into(),
        path: path.map(Path::to_path_buf),
        seen,
        windows: 0,
    }
}

/// What a budget that ended without a proof says of its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// Idle with no turn, control or fence open: only new activity can make it ready.
    Unproven,
    /// The same, but its transcript moved: the same wait runs one more short window.
    Requiet,
    /// A turn, a fence, a clear or another source may still supply the evidence.
    Waits,
}

impl Watched {
    /// `pane` is read only to settle a wait whose transcript moved: its screen must show no turn.
    pub(crate) fn expired(&mut self, pane: impl FnOnce() -> Option<String>) -> Expiry {
        let Some((source, stamp)) = &self.seen else {
            return Expiry::Waits;
        };
        let path = self.path.as_deref();
        if observe(&self.session, path).is_some()
            || Source::current(&self.session, path).as_ref() != Some(source)
            || clear::current(source).is_none()
        {
            return Expiry::Waits;
        }
        let tail = evidence::read(&source.path);
        let now = tail.as_ref().map(|tail| tail.stamp.clone());
        if Stamp::read(&source.path) != now {
            return Expiry::Waits;
        }
        let moved = now != *stamp;
        // A replaced or shortened file is another history, not metadata after this one.
        if moved
            && stamp.as_ref().is_some_and(|seen| {
                now.as_ref()
                    .is_none_or(|now| now.file != seen.file || now.len < seen.len)
            })
        {
            return Expiry::Waits;
        }
        let running = tail.as_ref().is_some_and(|tail| {
            tail.native
                .as_deref()
                .is_some_and(|native| native != source.native)
                || crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
                    &source.path,
                ) != crate::services::tui_turn_state::TuiTurnState::Idle
        });
        if running {
            return Expiry::Waits;
        }
        let state = ledger();
        let fenced = [source.nonce.clone(), None].into_iter().any(|nonce| {
            state
                .unread_continuations
                .contains(&(source.path.clone(), nonce.clone()))
                || now.as_ref().is_some_and(|stamp| {
                    state.continuations.contains_key(&(
                        source.path.clone(),
                        stamp.file,
                        nonce.clone(),
                    ))
                })
        });
        let open = fenced
            || state.entries.get(source).is_none_or(|entry| {
                entry.pending.is_some()
                    || entry.compact_ticket.is_some()
                    || entry.compact_done.is_some()
            });
        drop(state);
        if open {
            return Expiry::Waits;
        }
        if !moved && self.windows == 0 {
            return Expiry::Unproven;
        }
        if moved && self.windows < WINDOWS {
            self.windows += 1;
            if let Some((_, stamp)) = &mut self.seen {
                *stamp = now;
            }
            return Expiry::Requiet;
        }
        // A quiet window, or every window spent: the transcript shows no turn, so the screen decides.
        match pane().is_some_and(|capture| super::idle::shows_no_turn(&capture)) {
            true => Expiry::Unproven,
            false => Expiry::Waits,
        }
    }
}
