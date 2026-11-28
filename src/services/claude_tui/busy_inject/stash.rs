//! Optional C-s, bracketed paste, a fixed settle, and one Enter. No pane is read after a key;
//! `enter` lets the receipt check finish an unreceived paste with the same pinned key.
//! An owner is asked twice: ready before any key, and the payload start just before content.
use std::io::Write;

use super::super::owner_input;
use super::{Outcome, Pane, Timing, Veto, WriteFailure};

pub use crate::services::session_host::KeyOutcome as Key;
use crate::services::session_host::{HostKey, PinnedInput, SubmitPayload};

pub(in crate::services::claude_tui) fn submit(
    pane: &Pane,
    nonce: &str,
    text: &str,
    timing: &Timing,
    stash: bool,
) -> Outcome {
    submit_with_start(pane, nonce, text, timing, stash, || {})
}

pub(in crate::services::claude_tui) fn submit_with_start(
    pane: &Pane,
    nonce: &str,
    text: &str,
    timing: &Timing,
    stash: bool,
    on_first_key: impl FnOnce(),
) -> Outcome {
    let session = pane.target().trim_start_matches('=').trim_end_matches(':');
    if crate::services::tui_input::writer_lease::admits("tmux", session).is_err() {
        return Outcome::NotSent(Veto::LockContended);
    }
    let Ok(mut file) = tempfile::NamedTempFile::new() else {
        return load_failed();
    };
    if file
        .write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .is_err()
    {
        return load_failed();
    }
    let buffer = format!("agentdesk-busy-inject-{nonce}");
    let path = file.path().to_string_lossy().into_owned();
    if pane
        .input_ok(SubmitPayload::LoadFile {
            buffer: &buffer,
            path: &path,
        })
        .is_none()
    {
        return load_failed();
    }
    let outcome = (|| {
        // A refused owner leaves no key: a changed target and a refused lease stay apart from a
        // failed load-buffer.
        match owner_input::ready_payload() {
            Ok(()) => {}
            Err(owner_input::ReadyRefusal::Target(_)) => {
                return Outcome::NotSent(Veto::PaneUnavailable);
            }
            Err(owner_input::ReadyRefusal::Lease(_)) => {
                return Outcome::NotSent(Veto::LockContended);
            }
        }
        // A stop that took the writer token's first-key gate leaves the pane without a key.
        if !pane.first_key() {
            return Outcome::NotSent(Veto::LockContended);
        }
        on_first_key();
        if stash {
            owner_input::keyed();
            if pane
                .input_ok(SubmitPayload::Keys(&[HostKey::CtrlS]))
                .is_none()
            {
                return Outcome::Failed(WriteFailure::Stash);
            }
        }
        // After C-s the draft sits in the stash, so a refused start is a stash failure.
        if owner_input::start_payload().is_err() {
            return if stash {
                Outcome::Failed(WriteFailure::Stash)
            } else {
                Outcome::NotSent(Veto::LockContended)
            };
        }
        owner_input::keyed();
        match key(pane, PinnedInput::Paste { buffer: &buffer }) {
            Key::Applied => {}
            Key::Held if stash => return Outcome::Failed(WriteFailure::Stash),
            Key::Held | Key::Failed => return Outcome::Failed(WriteFailure::Paste),
        }
        std::thread::sleep(timing.settle);
        match key(pane, PinnedInput::Enter) {
            Key::Applied => Outcome::Injected,
            // The text may sit in the composer: a refused or failed Enter is never not-sent.
            Key::Held | Key::Failed => Outcome::Failed(WriteFailure::Enter),
        }
    })();
    if outcome != Outcome::Injected {
        let _ = pane.ok(&["delete-buffer", "-b", &buffer]);
    }
    outcome
}

/// One Enter, pinned like the submit's own keys; whether the server applied it.
pub(in crate::services::claude_tui) fn enter(pane: &Pane) -> bool {
    matches!(key(pane, PinnedInput::Enter), Key::Applied)
}

fn load_failed() -> Outcome {
    owner_input::load_failed();
    Outcome::NotSent(Veto::LoadFailed)
}

pub(crate) fn key(pane: &Pane, input: PinnedInput<'_>) -> Key {
    pane.input(SubmitPayload::Pinned {
        input,
        codex: false,
    })
    .into_key()
}

#[cfg(all(test, unix))]
#[path = "stash_owner_tests.rs"]
mod owner_tests;
