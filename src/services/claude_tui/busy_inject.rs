//! Pastes one external human input into a busy hosted Claude TUI composer; nothing here enqueues.
//! A person's draft is first stashed (`stash`); after the paste only Injected or Unconfirmed follow.

mod screen;
#[cfg(test)]
mod screen_tests;
mod stash;
#[cfg(all(test, unix))]
mod stash_tests;

use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use super::composer_lock::{ComposerAdmission, DraftSighting};
use crate::services::tui_input::actor::gate::{own_draft, own_wrapped_draft};
use crate::services::tui_input::bounded_tmux::{BoundedTmuxError, run_bounded_tmux};
use crate::services::tui_o::shadow::ShadowProvider;

/// Larger inputs are refused before any tmux call.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
const CAPTURE_SCROLLBACK: &str = "-80";
const VETOED: &str = "agentdesk-busy-inject-vetoed";

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
    /// The pane size is unknown or changed, or Claude's rows for the input are not predictable,
    /// so a paste could not be proven ours and would stay in the composer.
    UnpredictableRender,
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

/// Whether the external input reached Claude, read off the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    NotAttempted,
    AttemptedUnconfirmed,
    Observed,
}

/// What became of a person's composer draft; only Unchanged and RestoredObserved leave the pane free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DraftState {
    Unchanged,
    StashedVerified,
    RestoredObserved,
    Unknown,
}

/// The delivery and the draft are judged apart: a delivered input is never resent for its draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub outcome: Outcome,
    pub draft: DraftState,
}

impl Report {
    fn new(outcome: Outcome, draft: DraftState) -> Self {
        Self { outcome, draft }
    }

    fn not_sent(veto: Veto) -> Self {
        Self::new(Outcome::NotSent(veto), DraftState::Unchanged)
    }

    pub(crate) fn delivery(&self) -> Delivery {
        match self.outcome {
            Outcome::NotSent(_) => Delivery::NotAttempted,
            Outcome::Unconfirmed(_) => Delivery::AttemptedUnconfirmed,
            Outcome::Injected => Delivery::Observed,
        }
    }
}

pub(crate) struct Timing {
    pub lock_retries: &'static [Duration],
    pub settle: Duration,
    pub rechecks: usize,
    pub recheck_interval: Duration,
    pub confirm_window: Duration,
    pub confirm_poll: Duration,
    /// How long after the submit Claude may take to put a stashed draft back.
    pub restore_window: Duration,
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
    restore_window: Duration::from_secs(2),
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

    fn state(&self) -> Option<PaneState> {
        let out = self.ok(&[
            "display-message",
            "-p",
            "-t",
            &self.target,
            "#{session_attached},#{session_last_attached},#{pane_width},#{pane_height}",
        ])?;
        let mut fields = out.trim_end_matches('\n').split(',');
        let attached = fields.next()?.parse().ok()?;
        // Never-attached sessions report an empty value, else epoch seconds. A reply without it is
        // read as empty, which any past attach breaks, and its unknown size keeps the stash off.
        let last = fields.next().unwrap_or_default().to_string();
        if !last.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut size = || fields.next().and_then(|field| field.parse::<usize>().ok());
        Some(PaneState {
            generation: Generation { attached, last },
            size: size().zip(size()),
        })
    }

    fn generation(&self) -> Option<Generation> {
        self.state().map(|state| state.generation)
    }

    /// Runs `command` in the server only while no client is attached, none attached since `g0`,
    /// and the pane is still `size`. tmux runs a client's queued commands in one pass.
    fn guarded(&self, g0: &Generation, size: Option<(usize, usize)>, command: &str) -> Guard {
        let vetoed = format!(
            "display-message -p -t '{}' '{VETOED} #{{session_attached}} #{{session_last_attached}}'",
            self.target
        );
        let mut condition = format!(
            "#{{&&:#{{==:#{{session_attached}},0}},#{{==:#{{session_last_attached}},{}}}}}",
            g0.last
        );
        if let Some((width, height)) = size {
            let sized = format!(
                "#{{&&:#{{==:#{{pane_width}},{width}}},#{{==:#{{pane_height}},{height}}}}}"
            );
            condition = format!("#{{&&:{condition},{sized}}}");
        }
        let args = [
            "if-shell",
            "-F",
            "-t",
            &self.target,
            &condition,
            command,
            &vetoed,
        ];
        let Some(out) = self.ok(&args) else {
            return Guard::Failed;
        };
        // The else branch ran, so the command did not; a count means a person attached since g0.
        match out.trim().strip_prefix(VETOED).map(str::trim) {
            None if out.trim().is_empty() => Guard::Applied,
            None => Guard::Failed,
            Some(rest)
                if rest
                    .split_whitespace()
                    .next()
                    .is_some_and(|count| count.parse::<u32>().is_ok()) =>
            {
                Guard::Vetoed
            }
            Some(_) => Guard::Gone,
        }
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

enum Guard {
    Applied,
    Vetoed,
    Gone,
    Failed,
}

/// Attach count and last attach second; a change between two reads means a person could type.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Generation {
    attached: u32,
    last: String,
}

struct PaneState {
    generation: Generation,
    /// Columns and rows, which decide whether Claude folds or wraps a paste.
    size: Option<(usize, usize)>,
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

enum Plan {
    /// An empty composer, and how Claude will show the paste in it.
    Direct(screen::Drawn),
    /// The person's draft rows as Claude shows them, to recognise it when it comes back.
    Stash(Vec<String>),
}

/// Pre-mutation pane verdict: an empty composer under a live turn, a draft Claude can stash and
/// give back, or the veto.
fn judge_before_paste(
    capture: &str,
    transcript: &Path,
    text: &str,
    size: Option<(usize, usize)>,
) -> Result<Plan, Veto> {
    use crate::services::tmux_common::{
        tmux_capture_indicates_claude_tui_busy,
        tmux_capture_indicates_claude_tui_exact_empty_composer,
        tmux_capture_indicates_claude_tui_prompt_draft,
    };
    if modal(capture) {
        return Err(Veto::Modal);
    }
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    let empty = !tmux_capture_indicates_claude_tui_prompt_draft(&plain)
        && tmux_capture_indicates_claude_tui_exact_empty_composer(&plain);
    let stash = if empty {
        None
    } else {
        Some(screen::stashable(capture, text, size).ok_or(Veto::Draft)?)
    };
    // Enter on an idle pane would start a new turn instead of queueing behind this one. Claude
    // hides its busy chrome while the composer holds text, so a draft rests on the transcript.
    let turn = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(transcript);
    if !turn.is_busy() || (empty && !tmux_capture_indicates_claude_tui_busy(&plain)) {
        return Err(Veto::NotBusy);
    }
    match stash {
        Some(draft) => Ok(Plan::Stash(draft)),
        None => screen::drawn(text, size)
            .map(Plan::Direct)
            .ok_or(Veto::UnpredictableRender),
    }
}

/// How a protected pane's draft reads in one capture.
pub(crate) fn draft_sighting(capture: &str) -> DraftSighting {
    let screen = screen::read(capture);
    match (screen.stash, screen.composer) {
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Empty) => {
            DraftSighting::Settled
        }
        (screen::Stash::AbsentInRecognizedLayout, screen::Composer::Text(_)) => {
            DraftSighting::PersonDraft
        }
        _ => DraftSighting::Unsettled,
    }
}

/// Channel ids whose panes may take the stash path, comma-separated and read once; unset is none.
pub(crate) const STASH_CHANNELS_ENV: &str = "ADK_BUSY_INJECT_STASH_CHANNELS";

fn stash_channels_configured() -> &'static [u64] {
    static CHANNELS: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let raw = || std::env::var(STASH_CHANNELS_ENV).ok();
    CHANNELS.get_or_init(|| stash_channels(raw().as_deref()))
}

fn stash_channels(raw: Option<&str>) -> Vec<u64> {
    let ids = raw.unwrap_or_default().split(',');
    ids.filter_map(|id| id.trim().parse().ok()).collect()
}

/// Only the channel the caller resolved for this input counts; without one nothing is stashed.
fn stash_channel_listed(channel: Option<u64>, listed: &[u64]) -> bool {
    channel.is_some_and(|channel| listed.contains(&channel))
}

/// Tries the composer lock a bounded number of times, then injects under it. No channel comes
/// with this entry, so a person's draft is never stashed: it queues.
pub(crate) fn inject(pane: &Pane, request: &Request<'_>, timing: &Timing) -> Outcome {
    inject_report(pane, request, None, timing).outcome
}

/// `inject` with the draft axis kept apart from the delivery, for an input whose channel the
/// caller resolved, if any.
pub(crate) fn inject_report(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    timing: &Timing,
) -> Report {
    inject_listed(pane, request, channel, stash_channels_configured(), timing)
}

/// `inject_report` against an explicit allowlist.
fn inject_listed(
    pane: &Pane,
    request: &Request<'_>,
    channel: Option<u64>,
    listed: &[u64],
    timing: &Timing,
) -> Report {
    inject_gated(pane, request, timing, stash_channel_listed(channel, listed))
}

/// `inject_listed` with the stash path allowed or not.
fn inject_gated(pane: &Pane, request: &Request<'_>, timing: &Timing, stash: bool) -> Report {
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
        || !request.nonce.chars().all(|c| c.is_ascii_alphanumeric())
        || request.nonce.is_empty()
    {
        return Report::not_sent(Veto::InvalidInput);
    }
    let started = Instant::now();
    // The whole transaction, restore watch included, runs under the lock: one stash at a time.
    let locked = at_offsets(
        timing.lock_retries,
        || started.elapsed(),
        std::thread::sleep,
        || {
            super::composer_lock::try_with_composer_mutation_lock(request.session, || {
                inject_locked(pane, request, &text, timing, stash)
            })
        },
    );
    locked.unwrap_or(Report::not_sent(Veto::LockContended))
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

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn inject_locked(
    pane: &Pane,
    request: &Request<'_>,
    text: &str,
    timing: &Timing,
    stash: bool,
) -> Report {
    // The composer lock fences other AgentDesk writers only; a person reaches the pane by attaching.
    // The second is read first, so an attach after the state read lands in a later second.
    let floor = unix_seconds();
    let state = match pane.state() {
        None => return Report::not_sent(Veto::AttachUnknown),
        Some(state) if state.generation.attached > 0 => {
            return Report::not_sent(Veto::HumanAttached);
        }
        // A second attach within this second would leave the generation unchanged.
        Some(state)
            if state
                .generation
                .last
                .parse::<u64>()
                .is_ok_and(|last| last >= floor) =>
        {
            return Report::not_sent(Veto::HumanAttached);
        }
        Some(state) => state,
    };
    let Some(before) = pane.capture() else {
        return Report::not_sent(Veto::PaneUnavailable);
    };
    let capture = || Some(before.clone());
    let admission = super::composer_lock::composer_admission(request.session, capture);
    if admission == ComposerAdmission::Held {
        return Report::not_sent(Veto::Draft);
    }
    let plan = match judge_before_paste(&before, request.transcript, text, state.size) {
        Ok(plan) => plan,
        Err(veto) => return Report::not_sent(veto),
    };
    // A person's draft moves only through a stash on an allowlisted channel.
    let permitted = match plan {
        Plan::Direct(_) => admission == ComposerAdmission::Any,
        Plan::Stash(_) => stash,
    };
    if !permitted {
        return Report::not_sent(Veto::Draft);
    }
    let Ok(offset) = std::fs::metadata(request.transcript).map(|meta| meta.len()) else {
        return Report::not_sent(Veto::TranscriptUnavailable);
    };
    let Ok(mut file) = tempfile::NamedTempFile::new() else {
        return Report::not_sent(Veto::LoadFailed);
    };
    if file
        .write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .is_err()
    {
        return Report::not_sent(Veto::LoadFailed);
    }
    let buffer = format!("agentdesk-busy-inject-{}", request.nonce);
    let path = file.path().to_string_lossy().into_owned();
    if pane.ok(&["load-buffer", "-b", &buffer, &path]).is_none() {
        return Report::not_sent(Veto::LoadFailed);
    }
    let attempt = Attempt {
        pane,
        g0: state.generation,
        request,
        text,
        offset,
        buffer,
        timing,
        size: state.size,
    };
    match plan {
        Plan::Direct(drawn) => {
            Report::new(paste_into_empty(&attempt, &drawn), DraftState::Unchanged)
        }
        Plan::Stash(draft) => stash::run(&attempt, &draft),
    }
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
    /// The pane size the paste's rows were predicted for.
    size: Option<(usize, usize)>,
}

impl Attempt<'_> {
    fn key(&self, key: &str) -> Guard {
        let command = format!("send-keys -t '{}' {key}", self.pane.target);
        self.pane.guarded(&self.g0, None, &command)
    }

    /// With `size`, the paste lands only in a pane still that size.
    fn paste(&self, size: Option<(usize, usize)>) -> Guard {
        let command = format!(
            "paste-buffer -p -r -d -b {} -t '{}'",
            self.buffer, self.pane.target
        );
        self.pane.guarded(&self.g0, size, &command)
    }

    fn drop_buffer(&self) {
        let _ = self.pane.ok(&["delete-buffer", "-b", &self.buffer]);
    }

    /// No client now and none since g0.
    fn unattended(&self) -> bool {
        self.pane.generation().as_ref() == Some(&self.g0)
    }

    /// Rechecks after the paste until `owns` accepts a capture; the last capture is kept.
    fn await_own(
        &self,
        owns: impl Fn(&str) -> bool,
        last: &mut Option<String>,
    ) -> Result<(), Unconfirmed> {
        std::thread::sleep(self.timing.settle);
        for attempt in 0..self.timing.rechecks {
            if attempt > 0 {
                std::thread::sleep(self.timing.recheck_interval);
            }
            if !self.unattended() {
                return Err(Unconfirmed::AttachedAfterPaste);
            }
            let Some(after) = self.pane.capture() else {
                return Err(Unconfirmed::CaptureFailed);
            };
            let shown_modal = modal(&after);
            let owned = !shown_modal && owns(&after);
            *last = Some(after);
            if shown_modal {
                return Err(Unconfirmed::ModalAfterPaste);
            }
            if owned {
                return Ok(());
            }
        }
        Err(Unconfirmed::DraftNotOwned)
    }

    /// One guarded Enter, then the transcript watch; nothing is ever sent twice.
    fn enter(&self) -> Outcome {
        // The server does not report when it applied the key, so the window starts just before
        // the request; a slow reply (an after-send-keys hook) only shortens it.
        let deadline = Instant::now() + self.timing.confirm_window;
        // A person may still type after the last capture; an attach by then withholds the Enter.
        match self.key("Enter") {
            Guard::Applied => {}
            Guard::Vetoed => return Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste),
            Guard::Gone | Guard::Failed => return Outcome::Unconfirmed(Unconfirmed::EnterFailed),
        }
        // Only a scan that ends inside the window confirms; later evidence stays NotObserved.
        let request = self.request;
        loop {
            if Instant::now() >= deadline {
                return Outcome::Unconfirmed(Unconfirmed::NotObserved);
            }
            if transcript_carries(request.transcript, self.offset, request.nonce)
                && Instant::now() < deadline
            {
                return Outcome::Injected;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(self.timing.confirm_poll.min(left));
        }
    }
}

/// The composer was empty: paste, prove the bytes are ours, then one Enter.
fn paste_into_empty(attempt: &Attempt<'_>, drawn: &screen::Drawn) -> Outcome {
    // From the paste on, absence of evidence never proves the input was not taken.
    match attempt.paste(attempt.size) {
        Guard::Applied => {}
        guard @ (Guard::Vetoed | Guard::Gone) => {
            attempt.drop_buffer();
            // With no attach since g0, only a resize can have refused the paste.
            return Outcome::NotSent(match guard {
                Guard::Vetoed if attempt.unattended() => Veto::UnpredictableRender,
                Guard::Vetoed => Veto::HumanAttached,
                _ => Veto::PaneUnavailable,
            });
        }
        Guard::Failed => return Outcome::Unconfirmed(Unconfirmed::PasteFailed),
    }
    // Exact body, the exact rows Claude wraps it into, or a folded placeholder matching only in
    // shape and line count; anything else may hold a person's keys.
    let owns = |after: &str| {
        own_draft(ShadowProvider::Claude, after, attempt.text, true)
            || matches!(drawn, screen::Drawn::Rows(rows) if own_wrapped_draft(after, rows))
    };
    if let Err(detail) = attempt.await_own(owns, &mut None) {
        return Outcome::Unconfirmed(detail);
    }
    attempt.enter()
}
