//! One external human input pasted into a busy hosted Claude TUI composer. Nothing here
//! enqueues: after the first pane mutation the only outcomes are Injected or Unconfirmed.

use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::services::tui_input::actor::gate::own_draft;
use crate::services::tui_input::bounded_tmux::{BoundedTmuxError, run_bounded_tmux};
use crate::services::tui_o::shadow::ShadowProvider;

/// Larger inputs are refused before any tmux call.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
const CAPTURE_SCROLLBACK: &str = "-80";

/// Why nothing reached the pane; the caller may queue the input instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Veto {
    InvalidInput,
    LockContended,
    HumanAttached,
    AttachUnknown,
    PaneUnavailable,
    Modal,
    Draft,
    NotBusy,
    TranscriptUnavailable,
    LoadFailed,
}

/// A paste was attempted, so the input may sit in the composer or be submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unconfirmed {
    PasteFailed,
    AttachedAfterPaste,
    CaptureFailed,
    ModalAfterPaste,
    DraftNotOwned,
    EnterFailed,
    NotObserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Injected,
    NotSent(Veto),
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
    pub transcript: &'a Path,
    pub source: &'a str,
    pub author: &'a str,
    pub nonce: &'a str,
    pub text: &'a str,
}

/// A fresh 8-hex nonce that names this input in the transcript.
pub(crate) fn fresh_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// The header line keeps the first character off `/`, `!` and `#` and names the source.
pub(crate) fn frame(source: &str, author: &str, nonce: &str, text: &str) -> String {
    let clean = |value: &str| -> String {
        value
            .chars()
            .map(|c| if c.is_control() || c == ']' { ' ' } else { c })
            .collect()
    };
    format!(
        "[📱 {} · {} · {nonce}]\n{text}",
        clean(source),
        clean(author)
    )
}

fn marker(nonce: &str) -> String {
    format!(" · {nonce}]")
}

/// Whether a complete transcript record after `offset` carries the nonce as human input.
pub(crate) fn transcript_carries(path: &Path, offset: u64, nonce: &str) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if file.metadata().map(|meta| meta.len()).unwrap_or(0) < offset
        || file.seek(SeekFrom::Start(offset)).is_err()
    {
        return false;
    }
    let marker = marker(nonce);
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        // A line without its newline may still be mid-write.
        if line.ends_with(b"\n")
            && let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line)
            && human_input_text(&record).is_some_and(|text| text.contains(&marker))
        {
            return true;
        }
        line.clear();
    }
    false
}

/// Text of an enqueue, a queued command or a non-meta user prompt; other records never count.
fn human_input_text(record: &serde_json::Value) -> Option<String> {
    let text_of = |value: &serde_json::Value| -> Option<String> {
        match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Array(blocks) => Some(
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
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
    pub(crate) fn new(session: &str) -> Self {
        Self::with_program(session, PathBuf::from("tmux"))
    }

    pub(crate) fn with_program(session: &str, program: PathBuf) -> Self {
        Self {
            program,
            target: format!("={session}:"),
        }
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

    fn ok(&self, args: &[&str]) -> Option<String> {
        self.run(args)
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn attached(&self) -> Option<u32> {
        let out = self.ok(&[
            "display-message",
            "-p",
            "-t",
            &self.target,
            "#{session_attached}",
        ])?;
        out.trim().parse().ok()
    }

    fn capture(&self) -> Option<String> {
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

fn modal(capture: &str) -> bool {
    use crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog;
    use crate::services::tmux_common::{
        tmux_capture_indicates_claude_tui_interactive_modal,
        tmux_capture_indicates_claude_tui_mcp_auth_required,
    };
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    detect_claude_startup_dialog(&plain).is_some()
        || tmux_capture_indicates_claude_tui_interactive_modal(&plain)
        || tmux_capture_indicates_claude_tui_mcp_auth_required(&plain)
}

/// Pre-mutation pane verdict: an empty composer under a live turn, or the veto.
fn judge_before_paste(capture: &str, transcript: &Path) -> Result<(), Veto> {
    use crate::services::tmux_common::{
        tmux_capture_indicates_claude_tui_busy,
        tmux_capture_indicates_claude_tui_exact_empty_composer,
        tmux_capture_indicates_claude_tui_prompt_draft,
    };
    if modal(capture) {
        return Err(Veto::Modal);
    }
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    if tmux_capture_indicates_claude_tui_prompt_draft(&plain)
        || !tmux_capture_indicates_claude_tui_exact_empty_composer(&plain)
    {
        return Err(Veto::Draft);
    }
    // Enter on an idle pane would start a new turn instead of queueing behind this one.
    let turn = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(transcript);
    if !turn.is_busy() || !tmux_capture_indicates_claude_tui_busy(&plain) {
        return Err(Veto::NotBusy);
    }
    Ok(())
}

/// Tries the composer lock a bounded number of times, then injects under it.
pub(crate) fn inject(pane: &Pane, request: &Request<'_>, timing: &Timing) -> Outcome {
    let text = frame(request.source, request.author, request.nonce, request.text);
    if request.text.trim().is_empty() || text.len() > MAX_INPUT_BYTES {
        return Outcome::NotSent(Veto::InvalidInput);
    }
    for wait in timing.lock_retries {
        std::thread::sleep(*wait);
        let locked = super::composer_lock::try_with_composer_mutation_lock(request.session, || {
            inject_locked(pane, request, &text, timing)
        });
        if let Some(outcome) = locked {
            return outcome;
        }
    }
    Outcome::NotSent(Veto::LockContended)
}

fn inject_locked(pane: &Pane, request: &Request<'_>, text: &str, timing: &Timing) -> Outcome {
    // The composer lock fences other AgentDesk writers only; a person reaches the pane by attaching.
    match pane.attached() {
        None => return Outcome::NotSent(Veto::AttachUnknown),
        Some(clients) if clients > 0 => return Outcome::NotSent(Veto::HumanAttached),
        Some(_) => {}
    }
    let Some(before) = pane.capture() else {
        return Outcome::NotSent(Veto::PaneUnavailable);
    };
    if let Err(veto) = judge_before_paste(&before, request.transcript) {
        return Outcome::NotSent(veto);
    }
    let Ok(offset) = std::fs::metadata(request.transcript).map(|meta| meta.len()) else {
        return Outcome::NotSent(Veto::TranscriptUnavailable);
    };
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
    let buffer = format!("agentdesk-busy-inject-{}", request.nonce);
    let path = file.path().to_string_lossy().into_owned();
    if pane.ok(&["load-buffer", "-b", &buffer, &path]).is_none() {
        return Outcome::NotSent(Veto::LoadFailed);
    }
    // From the paste on, absence of evidence never proves the input was not taken.
    let paste = [
        "paste-buffer",
        "-p",
        "-r",
        "-d",
        "-b",
        &buffer,
        "-t",
        &pane.target,
    ];
    if pane.ok(&paste).is_none() {
        return Outcome::Unconfirmed(Unconfirmed::PasteFailed);
    }
    std::thread::sleep(timing.settle);
    let mut owned = false;
    for attempt in 0..timing.rechecks {
        if attempt > 0 {
            std::thread::sleep(timing.recheck_interval);
        }
        if pane.attached() != Some(0) {
            return Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste);
        }
        let Some(after) = pane.capture() else {
            return Outcome::Unconfirmed(Unconfirmed::CaptureFailed);
        };
        if modal(&after) {
            return Outcome::Unconfirmed(Unconfirmed::ModalAfterPaste);
        }
        // Exact body, or a folded placeholder matching only in shape and line count;
        // anything else may hold a person's keys.
        if own_draft(ShadowProvider::Claude, &after, text, true) {
            owned = true;
            break;
        }
    }
    if !owned {
        return Outcome::Unconfirmed(Unconfirmed::DraftNotOwned);
    }
    // A race remains after the last check; observed doubt above already withheld every key.
    if pane
        .ok(&["send-keys", "-t", &pane.target, "Enter"])
        .is_none()
    {
        return Outcome::Unconfirmed(Unconfirmed::EnterFailed);
    }
    let deadline = Instant::now() + timing.confirm_window;
    loop {
        if transcript_carries(request.transcript, offset, request.nonce) {
            return Outcome::Injected;
        }
        if Instant::now() >= deadline {
            return Outcome::Unconfirmed(Unconfirmed::NotObserved);
        }
        std::thread::sleep(timing.confirm_poll);
    }
}
