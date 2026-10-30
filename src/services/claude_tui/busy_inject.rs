//! Pastes one external human input into a hosted Claude TUI composer; nothing here enqueues.
//! One pre-paste capture chooses an optional stash; a short settle precedes one Enter, and
//! `confirm` settles the result from the transcript.

#[cfg(unix)]
mod epoch;
mod ownership;
pub(crate) use ownership::{
    ComposerOwner, composer_owner, composer_owner_in_session, draft_sighting,
    draft_sighting_in_session,
};
mod receipt;
mod submit;
#[cfg(test)]
pub(crate) use submit::inject;
#[cfg(all(test, unix))]
use submit::{inject_gated, inject_listed};
pub(crate) use submit::{inject_report, inject_report_any_turn};

#[derive(Clone, Copy)]
enum TurnGate {
    BusyOnly,
    AnyTurn,
}
pub(crate) use receipt::exact as exact_receipt;
#[cfg(test)]
mod receipt_interleave_tests;
#[cfg(all(test, unix))]
mod receipt_scope_tests;
pub(in crate::services::claude_tui) mod screen;
#[cfg(test)]
mod screen_identity_tests;
#[cfg(test)]
mod screen_tests;
pub(crate) mod stash;
#[cfg(all(test, unix))]
mod stash_tests;
#[cfg(all(test, unix))]
pub(crate) use stash_tests::{FAKE_TUI as FAKE_STASH_TUI, StashChannelGuard};

use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::services::platform::tmux::pane_dead_is_live;
use crate::services::tui_input::bounded_tmux::{BoundedTmuxError, run_bounded_tmux};

/// Larger inputs are refused before any tmux call.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
const CAPTURE_SCROLLBACK: &str = "-80";

/// Why nothing reached the pane; the caller may queue the input instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Veto {
    InvalidInput,
    LockContended,
    PaneUnavailable,
    Modal,
    NotBusy,
    TranscriptUnavailable,
    LoadFailed,
}

/// An actual tmux write failed; no observation failure can produce this result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    Stash,
    Paste,
    Enter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The paste and Enter were sent; only `confirm` reads whether Claude received them.
    Injected,
    NotSent(Veto),
    Failed(WriteFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub outcome: Outcome,
    /// What `confirm` checks once the paste and Enter were sent.
    sent: Option<Sent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Sent {
    offset: u64,
    text: String,
    /// C-s parked a person's draft that Claude hands back on submit.
    parked: bool,
}

impl Report {
    fn not_sent(veto: Veto) -> Self {
        Self {
            outcome: Outcome::NotSent(veto),
            sent: None,
        }
    }
}

/// How the transcript answered a sent paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Receipt {
    Observed,
    /// The frame was submitted together with other composer text.
    Contaminated,
    /// Nothing within both windows; `resent` tells whether the one finishing Enter went.
    Missing {
        resent: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptError {
    ContaminatedSubmit,
    NotObserved,
}

pub(crate) struct Timing {
    pub lock_retries: &'static [Duration],
    pub settle: Duration,
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
    pub transcript: &'a Path,
    pub source: &'a str,
    pub author: &'a str,
    /// The author's name as Discord shows it.
    pub name: &'a str,
    pub nonce: &'a str,
    pub text: &'a str,
}

/// A fresh 8-hex nonce that names this input in the transcript.
pub(crate) fn fresh_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// The header line keeps the first character off `/`, `!` and `#` and names the person.
pub(crate) use crate::services::tui_input::human_frame::plain_inject as frame;

fn marker(nonce: &str) -> String {
    format!(" · {nonce}]")
}

/// Whether a complete transcript record after `offset` carries the nonce as human input.
#[cfg(all(test, unix))]
pub(crate) fn transcript_carries(path: &Path, offset: u64, nonce: &str) -> bool {
    transcript_submission(path, offset, nonce, None).is_some()
}

pub(super) fn transcript_submission(
    path: &Path,
    offset: u64,
    nonce: &str,
    expected: Option<&str>,
) -> Option<bool> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return None;
    };
    if file.metadata().map(|meta| meta.len()).unwrap_or(0) < offset
        || file.seek(SeekFrom::Start(offset)).is_err()
    {
        return None;
    }
    let marker = marker(nonce);
    let legacy = expected.is_none_or(|frame| {
        !frame.starts_with("[User: ")
            || frame
                .split(']')
                .next()
                .is_some_and(|header| header.contains(" · 📱 v1 mode="))
    });
    let registered = expected.is_some_and(|frame| {
        crate::services::tui_input::human_frame::history::registered(nonce, path, frame)
    });
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut position = offset;
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        let start = position;
        position += line.len() as u64;
        if line.ends_with(b"\n")
            && let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line)
            && let Some((text, has_nontext)) = human_input_text(&record, expected.is_some())
        {
            if legacy && text.contains(&marker) {
                return Some(
                    expected.is_none_or(|expected| !has_nontext && receipt::exact(&text, expected)),
                );
            }
            if registered && let Some(expected) = expected {
                if !has_nontext && receipt::exact(&text, expected) {
                    let kind = match record.get("type").and_then(|v| v.as_str()) {
                        Some("queue-operation") => "enqueue",
                        Some("attachment") => "command",
                        _ => "user",
                    };
                    let row = record.get("uuid").and_then(|v| v.as_str()).or_else(|| {
                        record
                            .pointer("/attachment/source_uuid")
                            .and_then(|v| v.as_str())
                    });
                    if let Some(confirmed) =
                        crate::services::tui_input::human_frame::history::receipt(
                            nonce, path, start, kind, row, expected,
                        )
                    {
                        return Some(confirmed);
                    }
                } else if text.contains(expected)
                    // Another attempt's whole frame, or a mixed row it already took, stays its own.
                    && (receipt::exact(text.trim(), expected.trim())
                        || !crate::services::tui_input::human_frame::history::known(&text))
                    && receipt::take_mixed(path, nonce, &text)
                {
                    return Some(false);
                }
            }
        }
        line.clear();
    }
    None
}

/// Text and non-text presence in an enqueue, queued command or non-meta user prompt.
/// Extra non-text blocks prevent an exact frame receipt even when its text matches.
fn human_input_text(record: &serde_json::Value, prompt_only: bool) -> Option<(String, bool)> {
    let text_of = |value: &serde_json::Value| -> Option<(String, bool)> {
        match value {
            serde_json::Value::String(text) => Some((text.clone(), false)),
            serde_json::Value::Array(blocks) => Some((
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                blocks
                    .iter()
                    .any(|block| block.get("type").and_then(|t| t.as_str()) != Some("text")),
            )),
            _ => None,
        }
    };
    match record.get("type")?.as_str()? {
        "queue-operation" if record.get("operation")?.as_str()? == "enqueue" => {
            text_of(record.get("content")?)
        }
        "attachment" => {
            let attachment = record.get("attachment")?;
            if attachment.get("type")?.as_str()? != "queued_command" {
                return None;
            }
            if prompt_only
                && attachment
                    .get("commandMode")
                    .and_then(|mode| mode.as_str())
                    .is_some_and(|mode| mode != "prompt")
            {
                return None;
            }
            text_of(attachment.get("prompt")?)
        }
        "user" if record.get("isMeta").and_then(|v| v.as_bool()) != Some(true) => {
            text_of(record.get("message")?.get("content")?)
        }
        _ => None,
    }
}

/// Bounded tmux calls against one session; the program is injectable for tests.
pub(crate) struct Pane {
    program: PathBuf,
    target: String,
}

impl Pane {
    pub(crate) fn with_program(session: &str, program: PathBuf) -> Self {
        Self {
            program,
            target: format!("={session}:"),
        }
    }

    /// The `={session}:` target every command names.
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    fn run(&self, args: &[&str]) -> Result<Output, BoundedTmuxError> {
        let mut command = Command::new(&self.program);
        command.arg("-u").args(args);
        crate::services::platform::binary_resolver::apply_runtime_path(&mut command);
        // A fresh thread owns its runtime, so this blocks safely from any caller.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(BoundedTmuxError::Spawn)?
                        .block_on(run_bounded_tmux(&mut command))
                })
                .join()
                .unwrap_or_else(|_| {
                    Err(BoundedTmuxError::Spawn(std::io::Error::other(
                        "tmux worker panicked",
                    )))
                })
        })
    }

    pub(crate) fn ok(&self, args: &[&str]) -> Option<String> {
        self.run(args)
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub(crate) fn capture(&self) -> Option<String> {
        self.ok(&[
            "capture-pane",
            "-p",
            "-e",
            "-t",
            &self.target,
            "-S",
            CAPTURE_SCROLLBACK,
        ])
    }
}

pub(crate) fn modal(capture: &str) -> bool {
    screen::modal(capture)
}

#[cfg(all(test, unix))]
pub(crate) fn parkable(capture: &str) -> bool {
    screen::parkable(capture)
}

/// Read once before any key: whether a visible draft is parked with C-s. An unread composer
/// deliberately proceeds without C-s.
pub(in crate::services::claude_tui) fn before_paste(
    pane: &Pane,
    stash_allowed: bool,
) -> Result<bool, Veto> {
    #[cfg(unix)]
    if epoch::observe(pane.target().trim_start_matches('=').trim_end_matches(':')).is_err() {
        return Err(Veto::PaneUnavailable);
    }
    if pane
        .ok(&["display-message", "-p", "-t", pane.target(), "#{pane_dead}"])
        .is_none_or(|output| !pane_dead_is_live(&output))
    {
        return Err(Veto::PaneUnavailable);
    }
    let Some(capture) = pane.capture() else {
        return Ok(false);
    };
    if modal(&capture) {
        return Err(Veto::Modal);
    }
    Ok(park_draft(stash_allowed, screen::parkable(&capture)))
}

/// The one rule for a visible draft: a listed channel parks it with C-s, any other channel
/// pastes behind it.
fn park_draft(stash_allowed: bool, parkable: bool) -> bool {
    stash_allowed && parkable
}

/// Channel ids whose panes may take the stash path, comma-separated and read once; unset is none.
pub(crate) const STASH_CHANNELS_ENV: &str = "ADK_BUSY_INJECT_STASH_CHANNELS";

fn stash_channels_configured() -> &'static [u64] {
    static CHANNELS: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let raw = || std::env::var(STASH_CHANNELS_ENV).ok();
    CHANNELS.get_or_init(|| stash_channels(raw().as_deref()))
}

#[cfg(unix)]
pub(crate) fn stash_enabled(channel: Option<u64>) -> bool {
    #[cfg(test)]
    if !stash_tests::USE_PRODUCTION_CHANNELS.get() {
        return stash_channel_listed(channel, &stash_tests::configured_channels());
    }
    stash_channel_listed(channel, stash_channels_configured())
}

fn stash_channels(raw: Option<&str>) -> Vec<u64> {
    let ids = raw.unwrap_or_default().split(',');
    ids.filter_map(|id| id.trim().parse().ok()).collect()
}

/// Only the channel the caller resolved for this input counts; without one nothing is stashed.
fn stash_channel_listed(channel: Option<u64>, listed: &[u64]) -> bool {
    channel.is_some_and(|channel| listed.contains(&channel))
}

/// Runs `attempt` at each offset from the first try until one returns a value.
pub(crate) fn at_offsets<T>(
    offsets: &[Duration],
    elapsed: impl Fn() -> Duration,
    mut sleep: impl FnMut(Duration),
    mut attempt: impl FnMut() -> Option<T>,
) -> Option<T> {
    for offset in offsets {
        sleep(offset.saturating_sub(elapsed()));
        if let Some(value) = attempt() {
            return Some(value);
        }
    }
    None
}

fn inject_locked(
    pane: &Pane,
    request: &Request<'_>,
    text: &str,
    timing: &Timing,
    stash: bool,
    turn_gate: TurnGate,
) -> Report {
    let park = match before_paste(pane, stash) {
        Ok(park) => park,
        Err(veto) => return Report::not_sent(veto),
    };
    if matches!(turn_gate, TurnGate::BusyOnly)
        && !crate::services::tui_turn_state::observe_claude_jsonl_turn_state(request.transcript)
            .is_busy()
    {
        return Report::not_sent(Veto::NotBusy);
    }
    let Ok(offset) = std::fs::metadata(request.transcript).map(|meta| meta.len()) else {
        return Report::not_sent(Veto::TranscriptUnavailable);
    };
    if let Err(error) = crate::services::tui_input::human_frame::history::register(
        ("claude", request.session, request.nonce, request.author),
        request.transcript,
        offset,
        text,
    ) {
        tracing::warn!(%error, "busy frame ledger registration failed");
        return Report::not_sent(Veto::LoadFailed);
    }
    let outcome = stash::submit(pane, request.nonce, text, timing, park);
    let report = Report {
        outcome,
        sent: (outcome == Outcome::Injected).then(|| Sent {
            offset,
            text: text.to_string(),
            parked: park,
        }),
    };
    if matches!(report.outcome, Outcome::NotSent(_)) {
        crate::services::tui_input::human_frame::history::withdraw(
            request.nonce,
            request.transcript,
        );
    }
    report
}

/// Records a sent paste's receipt in the frame ledger off the caller's thread; the result stays
/// Enter-terminal, so nothing is retried and a missing receipt confirms nothing.
pub(crate) fn collect_receipt(request: &Request<'_>, report: &Report, timing: &Timing) {
    let Some(sent) = report.sent.clone() else {
        return;
    };
    let (path, nonce) = (request.transcript.to_path_buf(), request.nonce.to_string());
    let (window, poll) = (timing.confirm_window, timing.confirm_poll);
    // The thread records into the same attempt's history, never another's.
    let scope = crate::services::tui_input::human_frame::history::ReceiptScope::capture();
    let _ = std::thread::Builder::new()
        .name("claude-receipt".into())
        .spawn(move || {
            let receipt = (
                path.as_path(),
                sent.offset,
                nonce.as_str(),
                sent.text.as_str(),
            );
            let result = scope.enter(|| await_receipt(receipt, Instant::now() + window, poll));
            tracing::debug!(%nonce, ?result, "Claude injection receipt diagnostic");
        });
}

/// The transcript's answer to a sent paste: one window, at most one finishing Enter under the
/// composer lock, one more window. The waits hold no lock; `None` when nothing was sent.
pub(crate) fn confirm(
    pane: &Pane,
    request: &Request<'_>,
    report: &Report,
    timing: &Timing,
) -> Option<Receipt> {
    let sent = report.sent.as_ref()?;
    let receipt = (
        request.transcript,
        sent.offset,
        request.nonce,
        sent.text.as_str(),
    );
    let wait = || match await_receipt(
        receipt,
        Instant::now() + timing.confirm_window,
        timing.confirm_poll,
    ) {
        Ok(()) => Some(Receipt::Observed),
        Err(ReceiptError::ContaminatedSubmit) => Some(Receipt::Contaminated),
        Err(ReceiptError::NotObserved) => None,
    };
    if let Some(receipt) = wait() {
        return Some(receipt);
    }
    let started = Instant::now();
    let resent = at_offsets(
        timing.lock_retries,
        || started.elapsed(),
        std::thread::sleep,
        || {
            super::composer_lock::try_with_composer_mutation_lock(request.session, || {
                finish_submit(pane, request, sent)
            })
        },
    )
    .unwrap_or(false);
    if let Some(receipt) = wait() {
        return Some(receipt);
    }
    tracing::warn!(
        nonce = %request.nonce,
        tmux_session = %request.session,
        window_ms = timing.confirm_window.as_millis() as u64,
        resent,
        "Claude injection not received in the transcript"
    );
    Some(Receipt::Missing { resent })
}

/// An Enter on our own paste still shown is the same submission finished, not a resend: no
/// content is pasted again. A parked draft is never risked, since a late first Enter restores it.
fn finish_submit(pane: &Pane, request: &Request<'_>, sent: &Sent) -> bool {
    if sent.parked {
        return false;
    }
    let Some(capture) = pane.capture() else {
        return false;
    };
    // Read after the capture, so a receipt that landed meanwhile keeps the Enter off.
    let received = transcript_submission(
        request.transcript,
        sent.offset,
        request.nonce,
        Some(&sent.text),
    );
    let own = crate::services::tui_input::actor::gate::own_draft(
        crate::services::tui_o::shadow::ShadowProvider::Claude,
        &capture,
        &sent.text,
        true,
    );
    if received.is_some() || !own {
        return false;
    }
    let applied = stash::enter(pane);
    tracing::info!(
        nonce = %request.nonce,
        tmux_session = %request.session,
        applied,
        "Claude injection unreceived; Enter sent once more on its own paste"
    );
    applied
}

/// Polls the transcript after `offset` until `deadline` for the submission carrying `nonce`:
/// only the exact `text`, bare or in one paste block, is a receipt; another body is contaminated.
pub(crate) fn await_receipt(
    (transcript, offset, nonce, text): (&Path, u64, &str, &str),
    deadline: Instant,
    poll: Duration,
) -> Result<(), ReceiptError> {
    // Only a scan that ends inside the window confirms; later evidence stays NotObserved.
    loop {
        if Instant::now() >= deadline {
            return Err(ReceiptError::NotObserved);
        }
        if let Some(exact) = transcript_submission(transcript, offset, nonce, Some(text))
            && Instant::now() < deadline
        {
            return if exact {
                Ok(())
            } else {
                Err(ReceiptError::ContaminatedSubmit)
            };
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(poll.min(left));
    }
}
