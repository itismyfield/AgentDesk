//! Pastes one external human input into the composer of a Codex TUI model turn so Enter steers it
//! into that same native turn. One pre-paste capture precedes one paste and one Enter.

#[cfg(all(test, unix))]
mod inject_tests;
mod rollout;
mod screen;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) use crate::services::claude_tui::busy_inject::Pane;
use crate::services::claude_tui::busy_inject::{MAX_INPUT_BYTES, at_offsets, frame};
pub(crate) use rollout::{TurnVerdict, read_turn, submitted};

/// Why nothing reached the pane; the caller may queue the input instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Veto {
    InvalidInput,
    LockContended,
    PaneUnavailable,
    Modal,
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
    /// The injected-input ledger is full for this session.
    LedgerFull,
}

/// A paste was attempted, so the input may sit in the composer or be submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unconfirmed {
    PasteFailed,
    EnterFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    NotSent(Veto),
    /// The paste and one Enter succeeded locally; the rollout is diagnostic only.
    Injected,
    Unconfirmed(Unconfirmed),
}

pub(crate) struct Timing {
    pub lock_retries: &'static [Duration],
    pub settle: Duration,
    /// Receipt collection runs separately and never delays the next input.
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
    confirm_window: Duration::from_secs(3),
    confirm_poll: Duration::from_millis(250),
};

pub(crate) struct Request<'a> {
    pub session: &'a str,
    pub rollout: &'a Path,
    pub source: &'a str,
    pub author: &'a str,
    /// The author's name as Discord shows it.
    pub name: &'a str,
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
    let text = frame(
        request.source,
        request.author,
        request.name,
        request.nonce,
        request.text,
    );
    // Keep the session target and buffer name restricted to plain characters.
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

fn inject_locked(
    pane: &Pane,
    request: &Request<'_>,
    text: &str,
    ledger: &dyn Ledger,
    timing: &Timing,
) -> Outcome {
    let not_sent = Outcome::NotSent;
    if pane
        .ok(&["display-message", "-p", "-t", pane.target(), "#{pane_id}"])
        .is_none()
    {
        return not_sent(Veto::PaneUnavailable);
    }
    let before = pane.capture().unwrap_or_default();
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
        screen::Composer::Unread => {}
    }
    // Register every subsequent native record for the diagnostic observer.
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
    let drop_buffer = || {
        let _ = pane.ok(&["delete-buffer", "-b", &buffer]);
    };
    if !ledger.register() {
        drop_buffer();
        return not_sent(Veto::LedgerFull);
    }
    if let Err(error) = crate::services::tui_input::human_frame::history::register(
        ("codex", request.session, request.nonce, request.author),
        request.rollout,
        offset,
        text,
    ) {
        tracing::warn!(%error, "busy frame ledger registration failed");
        ledger.withdraw();
        drop_buffer();
        return not_sent(Veto::LedgerFull);
    }
    if pane
        .ok(&["display-message", "-p", "-t", pane.target(), "#{pane_id}"])
        .is_none()
    {
        drop_buffer();
        ledger.withdraw();
        crate::services::tui_input::human_frame::history::withdraw(request.nonce, request.rollout);
        return not_sent(Veto::PaneUnavailable);
    }
    if crate::services::claude_tui::owner_input::before_payload().is_err() {
        return Outcome::Unconfirmed(Unconfirmed::PasteFailed);
    }
    // After a write is attempted, its failure cannot prove that the pane took no input.
    if pane
        .ok(&[
            "paste-buffer",
            "-p",
            "-r",
            "-d",
            "-b",
            &buffer,
            "-t",
            pane.target(),
        ])
        .is_none()
    {
        return Outcome::Unconfirmed(Unconfirmed::PasteFailed);
    }
    std::thread::sleep(timing.settle);
    if pane
        .ok(&["send-keys", "-t", pane.target(), "Enter"])
        .is_none()
    {
        return Outcome::Unconfirmed(Unconfirmed::EnterFailed);
    }
    rollout::observe(request, offset, text, timing);
    Outcome::Injected
}
