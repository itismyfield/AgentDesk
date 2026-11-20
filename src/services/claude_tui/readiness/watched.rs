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
    /// Whether an older execution's missing source settles; only the Herdr first input opts in.
    settle_lost: bool,
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
        settle_lost: false,
    }
}

/// How a wait that no later look can clear ends its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unproven {
    /// The source sat idle and unmoved all along: the input is retained with its resend ask.
    Retained,
    /// Its transcript moved and the windows ended without evidence: the input is refused for good.
    Refused,
}

/// What a budget that ended without a proof says of its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// Idle with no turn, control or fence open: only new activity can make it ready.
    Unproven(Unproven),
    /// The same, but its transcript moved: the same wait runs one more short window.
    Requiet,
    /// A turn, a fence, a clear or another source may still supply the evidence; the cause says
    /// which, so a hold that never clears names its branch.
    Waits(WaitCause),
}

/// The open cause of a `Waits`, in `expired`'s branch order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitCause {
    /// The wait's first look found no source and nothing settles it: a launch may still
    /// publish one.
    NoSource,
    /// The source proves readiness; the caller's screen or reservation refused it.
    Observed,
    /// Another source, clear evidence or file replaced the one this wait watched.
    SourceMoved,
    /// The transcript changed under this look.
    StampRace,
    /// The transcript shows a running turn or another native session.
    Running,
    /// A Stop continuation, unread or not, still fences the file.
    Fenced,
    /// Our reservation or a submitted prompt awaits its turn.
    Pending,
    /// Our compact control is open or awaits its completion.
    Compact,
    /// The moved transcript's windows ended on a screen that may show a turn.
    Screen,
    /// A dialog, such as a permission prompt, covers the composer until it is dismissed.
    Modal,
}

impl Watched {
    /// An older execution's missing source ends retained instead of held.
    pub(crate) fn settling_lost_source(mut self) -> Self {
        self.settle_lost = true;
        self
    }

    /// `pane` is read only to settle a wait whose transcript moved: its screen must show no turn.
    pub(crate) fn expired(&mut self, pane: impl FnOnce() -> Option<String>) -> Expiry {
        let Some((source, stamp)) = &self.seen else {
            // For an opted-in caller, an execution older than this run has nothing in flight to
            // publish a source: retained with its resend ask instead of held for good.
            return match super::source::nonce(&self.session) {
                Some(Some(nonce)) if self.settle_lost && super::launched_before_run(&nonce) => {
                    Expiry::Unproven(Unproven::Retained)
                }
                _ => Expiry::Waits(WaitCause::NoSource),
            };
        };
        let path = self.path.as_deref();
        if observe(&self.session, path).is_some() {
            return Expiry::Waits(WaitCause::Observed);
        }
        if Source::current(&self.session, path).as_ref() != Some(source)
            || clear::current(source).is_none()
        {
            return Expiry::Waits(WaitCause::SourceMoved);
        }
        let tail = evidence::read(&source.path);
        let now = tail.as_ref().map(|tail| tail.stamp.clone());
        if Stamp::read(&source.path) != now {
            return Expiry::Waits(WaitCause::StampRace);
        }
        let moved = now != *stamp;
        // A replaced or shortened file is another history, not metadata after this one.
        if moved
            && stamp.as_ref().is_some_and(|seen| {
                now.as_ref()
                    .is_none_or(|now| now.file != seen.file || now.len < seen.len)
            })
        {
            return Expiry::Waits(WaitCause::SourceMoved);
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
            return Expiry::Waits(WaitCause::Running);
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
        let open = open_cause(state.entries.get(source));
        drop(state);
        if let Some(cause) = fenced.then_some(WaitCause::Fenced).or(open) {
            return Expiry::Waits(cause);
        }
        if !moved && self.windows == 0 {
            return Expiry::Unproven(Unproven::Retained);
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
            true => Expiry::Unproven(Unproven::Refused),
            false => Expiry::Waits(WaitCause::Screen),
        }
    }
}

/// What of ours is still open on a source. No entry after a look's own observation is nothing.
pub(super) fn open_cause(entry: Option<&super::Entry>) -> Option<WaitCause> {
    let entry = entry?;
    if entry.pending.is_some() {
        Some(WaitCause::Pending)
    } else if entry.compact_ticket.is_some() || entry.compact_done.is_some() {
        Some(WaitCause::Compact)
    } else {
        None
    }
}
