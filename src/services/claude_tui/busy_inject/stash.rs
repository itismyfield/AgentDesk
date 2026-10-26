//! Optional C-s, bracketed paste, a fixed settle, and one Enter. No pane is read after a key.
use std::io::Write;

use super::{Outcome, Pane, Timing, Veto, WriteFailure};

pub(in crate::services::claude_tui) fn submit(
    pane: &Pane,
    nonce: &str,
    text: &str,
    timing: &Timing,
    stash: bool,
) -> Outcome {
    let Ok(mut file) = tempfile::NamedTempFile::new() else {
        return Outcome::NotSent(Veto::LoadFailed);
    };
    if file
        .write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .is_err()
    {
        return Outcome::NotSent(Veto::LoadFailed);
    }
    let buffer = format!("agentdesk-busy-inject-{nonce}");
    let path = file.path().to_string_lossy().into_owned();
    if pane.ok(&["load-buffer", "-b", &buffer, &path]).is_none() {
        return Outcome::NotSent(Veto::LoadFailed);
    }
    let outcome = (|| {
        if stash
            && pane
                .ok(&["send-keys", "-t", pane.target(), "C-s"])
                .is_none()
        {
            return Outcome::Failed(WriteFailure::Stash);
        }
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
            return Outcome::Failed(WriteFailure::Paste);
        }
        std::thread::sleep(timing.settle);
        if pane
            .ok(&["send-keys", "-t", pane.target(), "Enter"])
            .is_none()
        {
            return Outcome::Failed(WriteFailure::Enter);
        }
        Outcome::Injected
    })();
    if outcome != Outcome::Injected {
        let _ = pane.ok(&["delete-buffer", "-b", &buffer]);
    }
    outcome
}
