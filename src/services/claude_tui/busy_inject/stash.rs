//! Optional C-s, bracketed paste, a fixed settle, and one Enter. No pane is read after a key.
//! An owner is asked twice: ready before any key, and the payload start just before content.
use std::io::Write;

use super::super::owner_input;
use super::{Outcome, Pane, Timing, Veto, WriteFailure};

const PINNED_APPLIED: &str = "PINNED_KEY_APPLIED";
const PINNED_HELD: &str = "PINNED_KEY_HELD";

pub(in crate::services::claude_tui) fn submit(
    pane: &Pane,
    nonce: &str,
    text: &str,
    timing: &Timing,
    stash: bool,
) -> Outcome {
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
    if pane.ok(&["load-buffer", "-b", &buffer, &path]).is_none() {
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
        if stash {
            owner_input::keyed();
            if pane
                .ok(&["send-keys", "-t", pane.target(), "C-s"])
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
        let paste = |target: &str| {
            [
                "paste-buffer",
                "-p",
                "-r",
                "-d",
                "-b",
                &buffer,
                "-t",
                target,
            ]
            .map(String::from)
        };
        match key(pane, paste) {
            Key::Applied => {}
            Key::Held if stash => return Outcome::Failed(WriteFailure::Stash),
            Key::Held | Key::Failed => return Outcome::Failed(WriteFailure::Paste),
        }
        std::thread::sleep(timing.settle);
        match key(pane, |target| {
            ["send-keys", "-t", target, "Enter"].map(String::from)
        }) {
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

fn load_failed() -> Outcome {
    owner_input::load_failed();
    Outcome::NotSent(Veto::LoadFailed)
}

enum Key {
    Applied,
    /// The tmux server found another pane identity and ran nothing.
    Held,
    Failed,
}

/// Runs one content key; under an owner the server runs it only for the pinned pane identity.
fn key<const N: usize>(pane: &Pane, command: impl Fn(&str) -> [String; N]) -> Key {
    match owner_input::pinned() {
        Ok(None) => {}
        Ok(Some((target, condition))) => {
            let command = command(&target).join(" ");
            let applied = format!("{command} ; display-message -p {PINNED_APPLIED}");
            let held = format!("display-message -p {PINNED_HELD}");
            let args = ["if-shell", "-F", "-t", &target, &condition, &applied, &held];
            return match pane.ok(&args).as_deref().map(str::trim) {
                Some(PINNED_APPLIED) => Key::Applied,
                Some(PINNED_HELD) => Key::Held,
                _ => Key::Failed,
            };
        }
        Err(_) => return Key::Held,
    }
    let command = command(pane.target());
    match pane.ok(&command.each_ref().map(String::as_str)) {
        Some(_) => Key::Applied,
        None => Key::Failed,
    }
}

#[cfg(all(test, unix))]
#[path = "stash_owner_tests.rs"]
mod owner_tests;
