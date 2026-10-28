//! Pastes one external human input into a running Codex composer with one Enter.
//! Native steer or queue admission succeeds locally; later receipts identify the receiving turn.

#[cfg(all(test, unix))]
mod inject_tests;
mod rollout;
mod screen;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) use crate::services::claude_tui::busy_inject::Pane;
use crate::services::claude_tui::busy_inject::{MAX_INPUT_BYTES, at_offsets, frame};
pub(crate) use rollout::{TurnVerdict, read_turn, read_turn_detail, submitted};

/// Why nothing reached the pane; the caller may queue the input instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Veto {
    InvalidInput,
    LockContended,
    PaneUnavailable,
    Modal,
    ComposerUnread,
    DraftNavigationUnproven,
    DraftSelectionUnproven,
    DraftCommandUnproven,
    DraftPasteTransformUnproven,
    DraftPlaceholderUnproven,
    DraftNotFullyVisible,
    DraftImagePositionUnproven,
    NotSteerable,
    NotBusy,
    TurnUnknown,
    /// The rollout's open model turn is not the one the caller resolved.
    TurnChanged,
    TranscriptUnavailable,
    LoadFailed,
    /// The injected-input ledger is full for this session.
    LedgerFull,
    AmbiguousIdenticalPending,
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
    fn message_id(&self) -> Option<u64> {
        None
    }
    fn admitted(&self) {}
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
                let held = Instant::now();
                let outcome = inject_locked(pane, request, &text, ledger, timing);
                tracing::debug!(
                    lock_held_micros = held.elapsed().as_micros() as u64,
                    "Codex composer lock held"
                );
                outcome
            })
        },
    );
    tracing::debug!(
        elapsed_micros = started.elapsed().as_micros() as u64,
        "Codex composer mutation elapsed"
    );
    locked.unwrap_or(Outcome::NotSent(Veto::LockContended))
}

#[derive(PartialEq, Eq)]
struct ComposerProof {
    identity: (String, u32),
    geometry: (usize, usize),
}

fn composer_proof(pane: &Pane) -> Result<ComposerProof, Veto> {
    let query = [
        "display-message",
        "-p",
        "-t",
        pane.target(),
        "#{pane_id}|#{pane_pid}|#{pane_width}|#{pane_height}|#{cursor_x}|#{cursor_y}|#{pane_in_mode}",
    ];
    let before = pane.ok(&query).ok_or(Veto::PaneUnavailable)?;
    let capture = pane
        .ok(&["capture-pane", "-p", "-e", "-t", pane.target()])
        .ok_or(Veto::ComposerUnread)?;
    let after = pane.ok(&query).ok_or(Veto::PaneUnavailable)?;
    let shown = screen::read(&capture);
    if shown.modal {
        return Err(Veto::Modal);
    }
    if let Some(veto) = screen::append_veto(&capture, &shown) {
        return Err(veto);
    }
    tracing::debug!(native_queue = shown.queued, "Codex composer admission");
    if before != after {
        return Err(Veto::ComposerUnread);
    }
    let fields: Vec<_> = after.trim().split('|').collect();
    let [id, pid, width, height, "2", y, "0"] = fields.as_slice() else {
        return Err(Veto::ComposerUnread);
    };
    let pid = pid
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or(Veto::ComposerUnread)?;
    let width = width
        .parse::<usize>()
        .ok()
        .filter(|width| *width > 0)
        .ok_or(Veto::ComposerUnread)?;
    let height = height
        .parse::<usize>()
        .ok()
        .filter(|height| *height > 0)
        .ok_or(Veto::ComposerUnread)?;
    let y = y.parse::<usize>().ok().ok_or(Veto::ComposerUnread)?;
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(&capture);
    let rows: Vec<_> = plain.lines().collect();
    if id.is_empty()
        || rows.len() != height
        || y >= height
        || rows.iter().rposition(|row| row.starts_with('›')) != Some(y)
    {
        return Err(Veto::ComposerUnread);
    }
    Ok(ComposerProof {
        identity: (id.to_string(), pid),
        geometry: (width, height),
    })
}

fn inject_locked(
    pane: &Pane,
    request: &Request<'_>,
    text: &str,
    ledger: &dyn Ledger,
    timing: &Timing,
) -> Outcome {
    let not_sent = Outcome::NotSent;
    let before = match composer_proof(pane) {
        Ok(proof) => proof,
        Err(veto) => return not_sent(veto),
    };
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
    if let Err(error) = crate::services::tui_input::human_frame::history::append::register(
        (request.session, request.nonce, request.author),
        request.rollout,
        offset,
        text,
        request.target_turn,
        ledger.message_id(),
        false,
    ) {
        tracing::warn!(%error, "busy frame ledger registration failed");
        ledger.withdraw();
        drop_buffer();
        return not_sent(if error == "identical frame still unconfirmed" {
            Veto::AmbiguousIdenticalPending
        } else {
            Veto::LedgerFull
        });
    }
    let final_veto = match read_turn(request.rollout) {
        Some(TurnVerdict::KnownModelTurn(turn)) if turn == request.target_turn => None,
        Some(TurnVerdict::KnownModelTurn(_)) => Some(Veto::TurnChanged),
        Some(TurnVerdict::NotBusy) => Some(Veto::NotBusy),
        Some(TurnVerdict::NonSteerable(_)) => Some(Veto::NotSteerable),
        Some(TurnVerdict::Unknown) => Some(Veto::TurnUnknown),
        None => Some(Veto::TranscriptUnavailable),
    }
    .or_else(|| match composer_proof(pane) {
        Ok(current) if current.identity != before.identity => Some(Veto::PaneUnavailable),
        Ok(current) if current != before => Some(Veto::ComposerUnread),
        Ok(_) => None,
        Err(veto) => Some(veto),
    });
    if let Some(veto) = final_veto {
        drop_buffer();
        ledger.withdraw();
        crate::services::tui_input::human_frame::history::withdraw(request.nonce, request.rollout);
        return not_sent(veto);
    }
    use crate::services::tui_input::human_frame::history::append::{self, Admission};
    if append::mark(request.nonce, request.rollout, Admission::EffectPossible).is_err() {
        ledger.withdraw();
        crate::services::tui_input::human_frame::history::withdraw(request.nonce, request.rollout);
        drop_buffer();
        return not_sent(Veto::LedgerFull);
    }
    let unconfirmed = |reason| {
        let _ = append::mark(request.nonce, request.rollout, Admission::Uncertain);
        Outcome::Unconfirmed(reason)
    };
    if crate::services::claude_tui::owner_input::before_payload().is_err() {
        return unconfirmed(Unconfirmed::PasteFailed);
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
        return unconfirmed(Unconfirmed::PasteFailed);
    }
    std::thread::sleep(timing.settle);
    if pane
        .ok(&["send-keys", "-t", pane.target(), "Enter"])
        .is_none()
    {
        return unconfirmed(Unconfirmed::EnterFailed);
    }
    if append::mark(request.nonce, request.rollout, Admission::Admitted).is_ok() {
        ledger.admitted();
    } else {
        tracing::warn!(
            nonce = request.nonce,
            "Codex admission retained without durable confirmation"
        );
    }
    rollout::observe(request, offset, text, timing);
    Outcome::Injected
}

/// Native completion is receipt evidence; later response aliases cannot mint ownership.
pub(crate) fn observe_native_record(
    path: &Path,
    start: u64,
    json: &serde_json::Value,
) -> Option<crate::services::tui_input::human_frame::history::append::Receipt> {
    let receipt = crate::services::tui_input::human_frame::history::append::bind_record_at(
        path, start, json,
    )?;
    observe_receipt(&receipt);
    Some(receipt)
}

pub(crate) fn observe_native_file(
    file: &std::fs::File,
    start: u64,
    json: &serde_json::Value,
) -> Option<crate::services::tui_input::human_frame::history::append::Receipt> {
    let receipt = crate::services::tui_input::human_frame::history::append::bind_open_record(
        file, start, json,
    )?;
    observe_receipt(&receipt);
    Some(receipt)
}

fn observe_receipt(receipt: &crate::services::tui_input::human_frame::history::append::Receipt) {
    crate::services::tui_prompt_dedupe::observe_codex_prompt_in_turn_at(
        &receipt.session,
        &receipt.prompt,
        Some(&receipt.row),
        receipt.native_turn.as_deref(),
        chrono::Utc::now(),
    );
}
