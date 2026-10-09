//! Pastes one external human input into the composer of a Codex TUI model turn so Enter steers it
//! into that same native turn. Nothing here enqueues; every veto leaves the pane untouched.

#[cfg(all(test, unix))]
mod inject_tests;
mod rollout;
mod screen;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) use crate::services::claude_tui::busy_inject::Pane;
use crate::services::claude_tui::busy_inject::{
    Generation, Guard, MAX_INPUT_BYTES, at_offsets, frame, unix_seconds,
};
pub(crate) use rollout::{TurnVerdict, read_turn};

/// Codex folds a paste longer than this many characters into a `[Pasted Content N chars]` row.
const MAX_UNFOLDED_CHARS: usize = 1000;
/// Rows the transcript, status and composer chrome keep; a taller composer pushes its first row
/// off the screen.
const RESERVED_ROWS: usize = 10;

/// Why nothing reached the pane; the caller may queue the input instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Veto {
    InvalidInput,
    LockContended,
    HumanAttached,
    AttachUnknown,
    PaneUnavailable,
    Modal,
    /// No composer row above a status row: an overlay, a picker or an unknown screen.
    NoComposer,
    Draft,
    /// Codex already shows its own queue, so an Enter would queue behind it.
    QueueShown,
    NotSteerable,
    NotBusy,
    TurnUnknown,
    /// The rollout's open model turn is not the one the caller resolved.
    TurnChanged,
    TranscriptUnavailable,
    LoadFailed,
    /// The pane size is unknown or changed, or the paste may fold or outgrow the screen.
    UnpredictableRender,
    /// The injected-input ledger is full for this session.
    LedgerFull,
}

/// A paste was attempted, so the input may sit in the composer or be submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unconfirmed {
    PasteFailed,
    AttachedAfterPaste,
    CaptureFailed,
    DraftNotOwned,
    EnterFailed,
    NotObserved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    NotSent(Veto),
    /// The rollout recorded the nonce as user input of `observed_turn`; `joined` when that is the
    /// turn the paste was aimed at.
    Injected {
        observed_turn: Option<String>,
        joined: bool,
    },
    Unconfirmed(Unconfirmed),
}

pub(crate) struct Timing {
    pub lock_retries: &'static [Duration],
    pub settle: Duration,
    pub rechecks: usize,
    pub recheck_interval: Duration,
    pub confirm_window: Duration,
    pub confirm_poll: Duration,
}

pub(crate) const TIMING: Timing = Timing {
    lock_retries: &[
        Duration::ZERO,
        Duration::from_millis(80),
        Duration::from_millis(160),
    ],
    settle: Duration::from_millis(200),
    rechecks: 5,
    recheck_interval: Duration::from_millis(200),
    confirm_window: Duration::from_secs(3),
    confirm_poll: Duration::from_millis(250),
};

pub(crate) struct Request<'a> {
    pub session: &'a str,
    pub rollout: &'a Path,
    pub source: &'a str,
    pub author: &'a str,
    pub nonce: &'a str,
    pub text: &'a str,
    /// The model turn the caller read as open; the paste is refused once it is not.
    pub target_turn: &'a str,
}

/// Where the observers learn that a nonce is ours before its record can reach them.
pub(crate) trait Ledger {
    /// Records the nonce before the paste; false refuses the paste.
    fn register(&self) -> bool;
    /// Drops the record of a paste that never ran.
    fn withdraw(&self);
}

/// Tries the composer lock a bounded number of times, then injects under it.
pub(crate) fn inject(
    pane: &Pane,
    request: &Request<'_>,
    ledger: &dyn Ledger,
    timing: &Timing,
) -> Outcome {
    let text = frame(request.source, request.author, request.nonce, request.text);
    // Both names reach tmux command strings, so only plain characters are accepted.
    let plain = |value: &str, extra: &[char]| {
        !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_alphanumeric() || extra.contains(&c))
    };
    if request.text.trim().is_empty()
        || text.len() > MAX_INPUT_BYTES
        || !plain(request.session, &['-', '_'])
        || request.nonce.is_empty()
        || !request.nonce.chars().all(|c| c.is_ascii_alphanumeric())
        || request.target_turn.trim().is_empty()
    {
        return Outcome::NotSent(Veto::InvalidInput);
    }
    let started = Instant::now();
    let locked = at_offsets(
        timing.lock_retries,
        || started.elapsed(),
        std::thread::sleep,
        || {
            super::input::try_with_composer_mutation_lock(request.session, || {
                inject_locked(pane, request, &text, ledger, timing)
            })
        },
    );
    locked.unwrap_or(Outcome::NotSent(Veto::LockContended))
}

/// Upper bound of the composer rows `text` takes in a pane `width` columns wide. A word-wrapped
/// row holds at least half a row of text, so a wrapped line takes at most twice its hard rows.
fn predicted_rows(text: &str, width: usize) -> Option<usize> {
    use unicode_width::UnicodeWidthStr;
    let usable = width.checked_sub(4).filter(|usable| *usable > 0)?;
    let rows = text.split('\n').map(|line| {
        let columns = line.width();
        if columns <= usable {
            1
        } else {
            2 * columns.div_ceil(usable) + 1
        }
    });
    Some(rows.sum())
}

fn inject_locked(
    pane: &Pane,
    request: &Request<'_>,
    text: &str,
    ledger: &dyn Ledger,
    timing: &Timing,
) -> Outcome {
    let not_sent = Outcome::NotSent;
    // Same attach rule as Claude's busy inject: none now and none within this second.
    let floor = unix_seconds();
    let state = match pane.state() {
        None => return not_sent(Veto::AttachUnknown),
        Some(state) if state.generation.attached > 0 => return not_sent(Veto::HumanAttached),
        Some(state)
            if state
                .generation
                .last
                .parse::<u64>()
                .is_ok_and(|last| last >= floor) =>
        {
            return not_sent(Veto::HumanAttached);
        }
        Some(state) => state,
    };
    let Some((width, height)) = state.size else {
        return not_sent(Veto::UnpredictableRender);
    };
    let rows = predicted_rows(text, width);
    let fits = rows
        .is_some_and(|rows| rows + RESERVED_ROWS <= height && rows <= screen::MAX_COMPOSER_ROWS);
    // The screen reader cannot follow a blank row inside the composer, so it could not prove it.
    let readable = text.split('\n').all(|line| !line.trim().is_empty());
    if text.chars().count() > MAX_UNFOLDED_CHARS || !fits || !readable {
        return not_sent(Veto::UnpredictableRender);
    }
    let Some(before) = pane.capture() else {
        return not_sent(Veto::PaneUnavailable);
    };
    let shown = screen::read(&before);
    if shown.modal {
        return not_sent(Veto::Modal);
    }
    if shown.queued {
        return not_sent(Veto::QueueShown);
    }
    match shown.composer {
        // Images above an empty textarea are a draft an Enter would submit with the input.
        screen::Composer::Empty if shown.attachments => return not_sent(Veto::Draft),
        screen::Composer::Empty => {}
        screen::Composer::Text(_) => return not_sent(Veto::Draft),
        screen::Composer::Unread => return not_sent(Veto::NoComposer),
    }
    // Read before the turn verdict, so the confirm scan covers every record after it.
    let Ok(offset) = std::fs::metadata(request.rollout).map(|meta| meta.len()) else {
        return not_sent(Veto::TranscriptUnavailable);
    };
    match read_turn(request.rollout) {
        Some(TurnVerdict::KnownModelTurn(turn)) if turn == request.target_turn => {}
        Some(TurnVerdict::KnownModelTurn(_)) => return not_sent(Veto::TurnChanged),
        Some(TurnVerdict::NonSteerable(_)) => return not_sent(Veto::NotSteerable),
        Some(TurnVerdict::NotBusy) => return not_sent(Veto::NotBusy),
        Some(TurnVerdict::Unknown) => return not_sent(Veto::TurnUnknown),
        None => return not_sent(Veto::TranscriptUnavailable),
    }
    let Ok(mut file) = tempfile::NamedTempFile::new() else {
        return not_sent(Veto::LoadFailed);
    };
    if file
        .write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .is_err()
    {
        return not_sent(Veto::LoadFailed);
    }
    let buffer = format!("agentdesk-busy-inject-{}", request.nonce);
    let path = file.path().to_string_lossy().into_owned();
    if pane.ok(&["load-buffer", "-b", &buffer, &path]).is_none() {
        return not_sent(Veto::LoadFailed);
    }
    let attempt = Attempt {
        pane,
        g0: state.generation,
        request,
        text,
        offset,
        buffer,
        timing,
    };
    if !ledger.register() {
        attempt.drop_buffer();
        return not_sent(Veto::LedgerFull);
    }
    // From the paste on, absence of evidence never proves the input was not taken.
    match attempt.paste(state.size) {
        Guard::Applied => {}
        guard @ (Guard::Vetoed | Guard::Resized | Guard::Gone) => {
            attempt.drop_buffer();
            ledger.withdraw();
            return not_sent(match guard {
                Guard::Resized => Veto::UnpredictableRender,
                Guard::Vetoed => Veto::HumanAttached,
                _ => Veto::PaneUnavailable,
            });
        }
        Guard::Failed => return Outcome::Unconfirmed(Unconfirmed::PasteFailed),
    }
    if let Err(detail) = attempt.await_own() {
        return Outcome::Unconfirmed(detail);
    }
    attempt.enter()
}

/// One locked attempt past the pre-paste verdict, with the frame loaded into a tmux buffer.
struct Attempt<'a> {
    pane: &'a Pane,
    /// The attach generation read just before the pre-paste capture.
    g0: Generation,
    request: &'a Request<'a>,
    text: &'a str,
    offset: u64,
    buffer: String,
    timing: &'a Timing,
}

impl Attempt<'_> {
    fn paste(&self, size: Option<(usize, usize)>) -> Guard {
        let command = format!(
            "paste-buffer -p -r -d -b {} -t '{}'",
            self.buffer,
            self.pane.target()
        );
        self.pane.guarded(&self.g0, size, &command)
    }

    fn drop_buffer(&self) {
        let _ = self.pane.ok(&["delete-buffer", "-b", &self.buffer]);
    }

    /// Rechecks after the paste until the composer holds exactly the frame.
    fn await_own(&self) -> Result<(), Unconfirmed> {
        std::thread::sleep(self.timing.settle);
        for attempt in 0..self.timing.rechecks {
            if attempt > 0 {
                std::thread::sleep(self.timing.recheck_interval);
            }
            if self.pane.generation().as_ref() != Some(&self.g0) {
                return Err(Unconfirmed::AttachedAfterPaste);
            }
            let Some(after) = self.pane.capture() else {
                return Err(Unconfirmed::CaptureFailed);
            };
            if screen::owns(&after, self.text) {
                return Ok(());
            }
        }
        Err(Unconfirmed::DraftNotOwned)
    }

    /// One guarded Enter, then the rollout watch; nothing is ever sent twice.
    fn enter(&self) -> Outcome {
        // The server does not report when it applied the key, so the window starts just before
        // the request.
        let deadline = Instant::now() + self.timing.confirm_window;
        let command = format!("send-keys -t '{}' Enter", self.pane.target());
        match self.pane.guarded(&self.g0, None, &command) {
            Guard::Applied => {}
            Guard::Vetoed | Guard::Resized => {
                return Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste);
            }
            Guard::Gone | Guard::Failed => return Outcome::Unconfirmed(Unconfirmed::EnterFailed),
        }
        // Only the rollout confirms: an emptied composer may mean a queued or swallowed input.
        let request = self.request;
        loop {
            if Instant::now() >= deadline {
                return Outcome::Unconfirmed(Unconfirmed::NotObserved);
            }
            if let Some(observed_turn) =
                rollout::submitted(request.rollout, self.offset, request.nonce)
                && Instant::now() < deadline
            {
                let joined = observed_turn.as_deref() == Some(request.target_turn);
                return Outcome::Injected {
                    observed_turn,
                    joined,
                };
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(self.timing.confirm_poll.min(left));
        }
    }
}
