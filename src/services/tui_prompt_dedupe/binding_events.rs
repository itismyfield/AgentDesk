//! Per-channel binding event log: each record is appended and fsynced before the binding it names is published.
//! Readers use `binding_events_since` and `subscribe_binding_events`; nothing here deletes a record.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::TuiRuntimeBinding;
use super::binding_context::{SpawnNonceMarker, launch_mode, observe_spawn_nonce_marker};
use crate::services::agent_protocol::RuntimeHandoffKind;
#[cfg(test)]
use crate::services::claude_tui::source_verify::n2b_mutant;
use crate::services::claude_tui::source_verify::{
    Observation, PinJudgment, SourceHistory, judge_pin,
};
use crate::services::discord::runtime_store::fsync_parent_dir;
pub(crate) use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;

pub(crate) const BINDING_EVENTS_DIR: &str = "binding_events";
mod claude_fold;
use claude_fold::Waiting;
pub(crate) use claude_fold::binding_events_judged_since;
pub(crate) mod codex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BindingCause {
    Startup,
    Resume,
    Clear,
    Compact,
    Continuation,
    Fork,
    Unknown,
}

/// `Resolved` completes the `Pending` record `pending_seq`; `Rejected` is audit only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BindingTarget {
    Source(SourceId),
    Pending {
        payload_session_id: String,
        payload_transcript_path: Option<String>,
    },
    Resolved {
        pending_seq: u64,
        source: SourceId,
    },
    Rejected {
        payload_session_id: String,
        payload_transcript_path: Option<String>,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BindingEvidence {
    pub hook_event: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// `seq` rises by exactly one per record of a channel, so a gap means a skipped line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BindingEvent {
    pub seq: u64,
    pub channel_id: u64,
    pub provider: String,
    pub tmux_session: String,
    pub execution_nonce: Option<String>,
    pub old: Option<SourceId>,
    pub new: BindingTarget,
    pub cause: BindingCause,
    pub parent_hint: Option<SourceId>,
    pub evidence: BindingEvidence,
    pub committed_at: DateTime<Utc>,
}

/// A log line: the event plus whether its source passed the Claude source check and when its hook
/// was published. Both sit beside the event so readers of `BindingEvent` see the same record.
#[derive(Deserialize)]
struct Logged {
    #[serde(flatten)]
    event: BindingEvent,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    codex_ownership: Option<codex::Ownership>,
}

#[derive(Serialize)]
struct LoggedRef<'a> {
    #[serde(flatten)]
    event: &'a BindingEvent,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_ownership: Option<&'a codex::Ownership>,
}

/// A binding change whose event could not be persisted; the binding was not published.
#[derive(Debug)]
pub(crate) struct BindingPersistError {
    pub tmux_session: String,
    pub error: io::Error,
}

/// What one hook said about a session switch; `source` is the SessionStart reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HookSignal {
    pub event: String,
    pub source: Option<String>,
    pub transcript_path: Option<String>,
    pub received_at: DateTime<Utc>,
    /// The relay's publish time; publish times of one host are what order a pane's transitions.
    pub published_at: Option<DateTime<Utc>>,
}

impl HookSignal {
    pub(crate) fn from_payload(event: &str, payload: &serde_json::Value) -> Self {
        let text = |key: &str| payload.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        Self {
            event: event.to_owned(),
            source: text("source"),
            transcript_path: text("transcript_path"),
            received_at: Utc::now(),
            published_at: None,
        }
    }

    /// Only SessionStart names why the session changed.
    pub(crate) fn cause(&self) -> BindingCause {
        if self.event != "session_start" {
            return BindingCause::Unknown;
        }
        match self.source.as_deref() {
            Some("startup") => BindingCause::Startup,
            Some("resume") => BindingCause::Resume,
            Some("clear") => BindingCause::Clear,
            Some("compact") => BindingCause::Compact,
            Some("fork") => BindingCause::Fork,
            _ => BindingCause::Unknown,
        }
    }
}

/// Launch reads the execution's context, but only for the first record of that execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CauseSource {
    Hook(BindingCause),
    Launch,
    Observed,
}

/// A source proposed for one pane of a channel; `replaced` is the binding it would overwrite.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Proposal<'a> {
    pub channel_id: u64,
    pub provider: &'a str,
    pub tmux_session: &'a str,
    pub session_id: Option<&'a str>,
    pub path: &'a str,
    pub replaced: Option<(&'a str, Option<&'a str>)>,
    pub cause: CauseSource,
    pub hook: Option<&'a HookSignal>,
}

impl<'a> Proposal<'a> {
    /// `None` for runtimes O does not read and for panes without a known channel.
    pub(crate) fn for_binding(
        channel_id: Option<u64>,
        tmux_session: &'a str,
        binding: &'a TuiRuntimeBinding,
        replaced: Option<&'a TuiRuntimeBinding>,
        cause: CauseSource,
    ) -> Option<Self> {
        let provider = match binding.runtime_kind {
            RuntimeHandoffKind::ClaudeTui => "claude",
            RuntimeHandoffKind::CodexTui => "codex",
            _ => return None,
        };
        Some(Self {
            channel_id: channel_id.filter(|id| *id != 0)?,
            provider,
            tmux_session,
            session_id: binding.session_id.as_deref(),
            path: &binding.output_path,
            replaced: replaced.map(|old| (old.output_path.as_str(), old.session_id.as_deref())),
            cause,
            hook: None,
        })
    }

    fn session(&self) -> Option<&'a str> {
        self.session_id.map(str::trim).filter(|id| !id.is_empty())
    }

    fn payload_path(&self) -> Option<String> {
        let hook = self.hook.and_then(|hook| hook.transcript_path.clone());
        hook.or_else(|| Some(self.path.to_owned()))
    }
}

/// `verified` pins `current`'s (dev, ino): a hook naming it on another file is an anomaly.
#[derive(Default)]
struct PaneState {
    current: Option<SourceId>,
    verified: bool,
    pending: Option<BindingEvent>,
    /// The waiting Pending's publish time; only a prompt published after it reclaims the pane.
    pending_published: Option<DateTime<Utc>>,
    /// `(session, reason)` of the latest refusal, so only a repeat of the same judgment is dropped.
    rejected: Option<(String, String)>,
    nonce: Option<String>,
    claude: claude_fold::ClaudeFold,
}

/// How a proposal is written. Only `Stat` reads the file; the others record the caller's judgment.
#[derive(Clone, Copy)]
enum Plan<'a> {
    Stat,
    ForcePending,
    Verified(&'a SourceId),
    Rejected(&'a str),
    /// The bound source again, logged only when it reclaims the pane from a waiting Pending.
    Reclaim(&'a SourceId),
}

#[derive(Default)]
struct Writer {
    last_seq: u64,
    panes: HashMap<String, PaneState>,
    parents_synced: bool,
    poisoned: bool,
    /// An unreadable line or seq gap came after the last record applied.
    tainted: bool,
}

struct ChannelLog {
    notify: watch::Sender<u64>,
    writer: Option<Writer>,
}

static LOGS: LazyLock<Mutex<HashMap<PathBuf, ChannelLog>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_logs() -> MutexGuard<'static, HashMap<PathBuf, ChannelLog>> {
    LOGS.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(not(test))]
fn events_dir() -> io::Result<Option<PathBuf>> {
    let root = crate::config::runtime_root()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "runtime root unavailable"))?;
    Ok(Some(root.join(BINDING_EVENTS_DIR)))
}

#[cfg(not(test))]
fn fault(_step: &str) -> io::Result<()> {
    Ok(())
}

fn log_path(channel_id: u64) -> io::Result<Option<PathBuf>> {
    Ok(events_dir()?.map(|dir| dir.join(format!("{channel_id}.log"))))
}

struct LogRead<T = BindingEvent> {
    records: Vec<T>,
    lines: u64,
    complete_len: u64,
    total_len: u64,
}

/// Complete lines only: a torn tail is left out, and a corrupt line is skipped with a warning.
fn read_log<T: DeserializeOwned>(path: &Path) -> io::Result<LogRead<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1);
    let (mut records, mut lines) = (Vec::new(), 0);
    for line in bytes[..complete]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
    {
        lines += 1;
        match serde_json::from_slice(line) {
            Ok(record) => records.push(record),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unreadable binding event");
            }
        }
    }
    let (complete_len, total_len) = (complete as u64, bytes.len() as u64);
    Ok(LogRead {
        records,
        lines,
        complete_len,
        total_len,
    })
}

/// Records of `channel_id` with `seq > after_seq`, in log order. Read-only; a corrupt line fails
/// the read, since skipping it would hide a binding from the reader.
pub(crate) fn binding_events_since(
    channel_id: u64,
    after_seq: u64,
) -> io::Result<Vec<BindingEvent>> {
    let events = binding_events_judged_since(channel_id, after_seq)?;
    Ok(events.into_iter().map(|(event, _)| event).collect())
}

/// Why a strict read refused a log; `line` counts complete non-empty lines from 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Corrupt {
    pub line: u64,
    pub kind: CorruptKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CorruptKind {
    Unparseable,
    SeqGap {
        expected: u64,
        found: u64,
    },
    /// A Pending names a transcript other than the one its session would have next to the launch.
    PathMismatch,
}

/// Every record of `channel_id`, or the first line that is unreadable or out of `seq` order.
/// A torn tail is not corruption: it was never published and the writer cuts it off on load.
pub(crate) fn records_strict(channel_id: u64) -> io::Result<Result<Vec<BindingEvent>, Corrupt>> {
    let Some(path) = log_path(channel_id)? else {
        return Ok(Ok(Vec::new()));
    };
    let _logs = lock_logs();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1);
    let lines = bytes[..complete].split(|b| *b == b'\n');
    let mut records = Vec::new();
    for (line, text) in (1..).zip(lines.filter(|l| !l.is_empty())) {
        let Ok(record) = serde_json::from_slice::<BindingEvent>(text) else {
            let kind = CorruptKind::Unparseable;
            return Ok(Err(Corrupt { line, kind }));
        };
        if record.seq != line {
            let (expected, found) = (line, record.seq);
            let kind = CorruptKind::SeqGap { expected, found };
            return Ok(Err(Corrupt { line, kind }));
        }
        records.push(record);
    }
    Ok(Ok(records))
}

/// The latest committed `seq` of `channel_id`, updated after every append. Read-only.
pub(crate) fn subscribe_binding_events(channel_id: u64) -> io::Result<watch::Receiver<u64>> {
    let Some(path) = log_path(channel_id)? else {
        return Ok(watch::channel(0).1);
    };
    let mut logs = lock_logs();
    if let Some(log) = logs.get(&path) {
        return Ok(log.notify.subscribe());
    }
    let last = read_log::<BindingEvent>(&path)?
        .records
        .iter()
        .map(|r| r.seq)
        .max();
    let notify = watch::channel(last.unwrap_or(0)).0;
    let log = logs.entry(path).or_insert(ChannelLog {
        notify,
        writer: None,
    });
    Ok(log.notify.subscribe())
}

/// What a commit left in the log for its proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Committed {
    Appended,
    /// The log already says what the proposal says.
    Unchanged,
    /// The path no longer names the file its check read, and no pin holds it: nothing was logged.
    Stale,
    /// The pinned file was replaced or is gone: nothing was logged and nothing may be bound.
    Anomaly,
    /// The pinned file could not be looked at: nothing was logged, and it is judged again.
    Recheck,
}

/// What a plan does to the log: append a record, whether its source was verified and its hook's
/// publish time, or keep it.
enum Planned {
    Append(BindingEvent, bool, Option<DateTime<Utc>>),
    CodexAppend(BindingEvent, Box<codex::Ownership>),
    Keep(Committed),
}

/// Appends what `proposal`'s stat changes for its pane, if anything, as the pane's pin admits it.
pub(crate) fn record_source(proposal: &Proposal) -> io::Result<Committed> {
    commit(proposal, Plan::Stat)
}

/// How a Pending judgment stands in the log once `record_pending` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingRecord {
    Recorded,
    /// A Pending for the same candidate was already logged.
    AlreadyPending,
}

/// A candidate that has no verified transcript yet waits as a Pending, whether or not a file exists.
/// Only a matching Pending stands in for the record; a current source naming the candidate does not.
pub(crate) fn record_pending(proposal: &Proposal) -> io::Result<PendingRecord> {
    match commit(proposal, Plan::ForcePending)? {
        Committed::Appended => Ok(PendingRecord::Recorded),
        _ => Ok(PendingRecord::AlreadyPending),
    }
}

/// Records exactly `source`, the identity the check read, unless the pane's pin holds another file;
/// the path is not looked at again.
pub(crate) fn record_verified(proposal: &Proposal, source: &SourceId) -> io::Result<Committed> {
    commit(proposal, Plan::Verified(source))
}

/// Judges a path that no longer names the file its check read: under the pane's pin that is an
/// anomaly, or a recheck while unreadable; otherwise the check is stale and waits.
pub(crate) fn judge_moved(proposal: &Proposal) -> io::Result<Committed> {
    let committed = commit_with(proposal.channel_id, |writer| {
        let stat = fs::metadata(proposal.path).map(|meta| file_identity(&meta));
        Planned::Keep(
            match writer.pin_judgment(proposal, Observation::Stat(stat)) {
                PinJudgment::Anomaly => Committed::Anomaly,
                PinJudgment::Recheck => Committed::Recheck,
                _ => Committed::Stale,
            },
        )
    })?;
    Ok(match committed {
        Committed::Unchanged => Committed::Stale,
        committed => committed,
    })
}

/// Logs the bound `source` again when the prompt behind `proposal` outlived the pane's waiting
/// Pending, which the record then supersedes; `true` when it was appended.
pub(crate) fn record_reclaim(proposal: &Proposal, source: &SourceId) -> io::Result<bool> {
    commit(proposal, Plan::Reclaim(source)).map(|committed| committed == Committed::Appended)
}

/// Audits a candidate the binding judgment refused; `true` when this judgment was newly logged.
pub(crate) fn record_rejected(proposal: &Proposal, reason: &str) -> io::Result<bool> {
    commit(proposal, Plan::Rejected(reason)).map(|c| c == Committed::Appended)
}

/// Reads the existing writer's Pending identity without loading, appending, or publishing a record.
pub(crate) fn pending_seq(channel_id: u64, tmux_session: &str, session: &str) -> Option<u64> {
    let path = log_path(channel_id).ok().flatten()?;
    let logs = lock_logs();
    let writer = logs.get(&path)?.writer.as_ref()?;
    let pending = writer.panes.get(tmux_session)?.pending.as_ref()?;
    let same = matches!(&pending.new, BindingTarget::Pending { payload_session_id: id, .. } if id == session);
    same.then_some(pending.seq)
}

/// The pane's current source, when a verified record pinned it.
pub(crate) fn pinned_source(channel_id: u64, tmux_session: &str) -> io::Result<Option<SourceId>> {
    let mut pinned = None;
    commit_with(channel_id, |writer| {
        let pane = writer.panes.get(tmux_session).filter(|pane| pane.verified);
        pinned = pane.and_then(|pane| pane.current.clone());
        Planned::Keep(Committed::Unchanged)
    })?;
    Ok(pinned)
}

/// The pane's pinned source and its Claude history in execution `nonce`, read from the writer that
/// records them.
pub(crate) fn claude_history(
    channel_id: u64,
    tmux_session: &str,
    nonce: Option<&str>,
) -> io::Result<(Option<SourceId>, SourceHistory)> {
    let mut found = (None, SourceHistory::default());
    commit_with(channel_id, |writer| {
        let pane = writer.panes.get(tmux_session);
        let pin = pane
            .filter(|pane| pane.verified)
            .and_then(|pane| pane.current.clone());
        let history = match pane {
            Some(pane) => pane.claude.history(nonce, writer.tainted),
            None => claude_fold::ClaudeFold::default().history(nonce, writer.tainted),
        };
        found = (pin, history);
        Planned::Keep(Committed::Unchanged)
    })?;
    Ok(found)
}

fn commit(proposal: &Proposal, mode: Plan) -> io::Result<Committed> {
    commit_with(proposal.channel_id, |writer| writer.plan(proposal, mode))
}

/// Runs `plan` on the channel's writer and appends the record it returns.
fn commit_with(
    channel_id: u64,
    plan: impl FnOnce(&mut Writer) -> Planned,
) -> io::Result<Committed> {
    let Some(path) = log_path(channel_id)? else {
        return Ok(Committed::Unchanged);
    };
    let mut logs = lock_logs();
    let log = logs.entry(path.clone()).or_insert_with(|| ChannelLog {
        notify: watch::channel(0).0,
        writer: None,
    });
    if log.writer.is_none() {
        let writer = Writer::load(&path)?;
        let last = writer.last_seq;
        log.notify.send_if_modified(|seen| {
            let moved = *seen < last;
            *seen = (*seen).max(last);
            moved
        });
        log.writer = Some(writer);
    }
    let Some(writer) = log.writer.as_mut() else {
        return Ok(Committed::Unchanged);
    };
    let (record, verified, published_at, codex_ownership) = match plan(writer) {
        Planned::Append(record, verified, published_at) => (record, verified, published_at, None),
        Planned::CodexAppend(record, ownership) => (record, false, None, Some(ownership)),
        Planned::Keep(committed) => return Ok(committed),
    };
    #[cfg(test)]
    let logged = published_at.filter(|_| !n2b_mutant("live"));
    #[cfg(not(test))]
    let logged = published_at;
    if let Err(error) = writer.append(&path, &record, verified, logged, codex_ownership.as_deref())
    {
        // A line that could not be cut back off is re-read from disk before the next append.
        if writer.poisoned {
            log.writer = None;
        }
        return Err(error);
    }
    writer.apply(&record, verified, published_at);
    log.notify.send_replace(record.seq);
    Ok(Committed::Appended)
}

fn source_id(session: Option<&str>, path: &str, (dev, ino): (u64, u64)) -> SourceId {
    SourceId {
        session_id: session.unwrap_or_default().to_owned(),
        path: PathBuf::from(path),
        dev,
        ino,
    }
}

/// A session filled in later is still the same source; a replaced file on the same path is not.
fn same_source(
    current: &SourceId,
    session: Option<&str>,
    path: &str,
    file: Option<(u64, u64)>,
) -> bool {
    current.path == Path::new(path)
        && session.is_none_or(|id| current.session_id.is_empty() || current.session_id == id)
        && file.is_none_or(|file| file == (current.dev, current.ino))
}

fn pending_matches(pending: &BindingEvent, session: Option<&str>, path: &str) -> bool {
    let BindingTarget::Pending {
        payload_session_id,
        payload_transcript_path,
    } = &pending.new
    else {
        return false;
    };
    session.is_some_and(|id| id == payload_session_id)
        || payload_transcript_path.as_deref() == Some(path)
}

impl Writer {
    fn load(path: &Path) -> io::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("binding event log has no directory"))?;
        match fs::create_dir(dir) {
            Err(error) if !(error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir()) => {
                return Err(error);
            }
            _ => {}
        }
        let read = read_log::<Logged>(path)?;
        if read.total_len > 0 {
            let file = OpenOptions::new().write(true).open(path)?;
            if read.complete_len < read.total_len {
                // A crash mid-append left a line that was never published; drop it before appending.
                file.set_len(read.complete_len)?;
            }
            // Lines read back after a restart may predate their fsync, so make them durable before use.
            fault("reload")?;
            file.sync_all()?;
            fsync_parent_dir(path)?;
            fsync_parent_dir(dir)?;
        }
        let mut writer = Self {
            last_seq: read.lines,
            panes: HashMap::new(),
            parents_synced: read.total_len > 0,
            poisoned: false,
            tainted: false,
        };
        let mut next = 1;
        for logged in &read.records {
            // A skipped line holds its seq, so a gap is where an unreadable record was.
            writer.tainted |= logged.event.seq != next;
            next = logged.event.seq + 1;
            writer.apply(&logged.event, logged.verified, logged.published_at);
        }
        if read.lines >= next {
            writer
                .panes
                .values_mut()
                .for_each(|pane| pane.claude.taint());
            writer.tainted = true;
        }
        Ok(writer)
    }

    fn apply(
        &mut self,
        record: &BindingEvent,
        verified: bool,
        published_at: Option<DateTime<Utc>>,
    ) -> bool {
        self.last_seq = self.last_seq.max(record.seq);
        let tainted = std::mem::take(&mut self.tainted);
        if tainted {
            self.panes.values_mut().for_each(|pane| pane.claude.taint());
        }
        let pane = self.panes.entry(record.tmux_session.clone()).or_default();
        if record.execution_nonce.is_some() {
            pane.nonce = record.execution_nonce.clone();
        }
        let mut reclaimed = false;
        if record.provider == "claude" {
            let waiting = (pane.pending.as_ref()).map(|p| Waiting::of(p, pane.pending_published));
            // Only a verified record of the source the pane already holds verified is a re-pin.
            let same =
                matches!(&record.new, BindingTarget::Source(s) if pane.current.as_ref() == Some(s));
            let repinned = verified && pane.verified && same;
            let step = claude_fold::step(record, published_at, waiting, repinned);
            reclaimed = step == claude_fold::Step::Reclaim;
            #[cfg(test)]
            let kept = n2b_mutant("supersede-off");
            #[cfg(not(test))]
            let kept = false;
            // A hook that moved the pane, or a prompt of its own session it outlived, supersedes it.
            if (step == claude_fold::Step::Switch && !kept) || step == claude_fold::Step::Reclaim {
                (pane.pending, pane.pending_published) = (None, None);
            }
            pane.claude
                .apply(record, verified, published_at, step, tainted);
        }
        match &record.new {
            BindingTarget::Source(source) => {
                pane.current = Some(source.clone());
                pane.verified = verified;
            }
            BindingTarget::Pending {
                payload_session_id, ..
            } => {
                (pane.pending, pane.pending_published) = (Some(record.clone()), published_at);
                // The refused session is a candidate again, so its next refusal must reach the log.
                if pane
                    .rejected
                    .as_ref()
                    .is_some_and(|(session, _)| session == payload_session_id)
                {
                    pane.rejected = None;
                }
            }
            BindingTarget::Resolved {
                pending_seq,
                source,
            } => {
                pane.current = Some(source.clone());
                pane.verified = verified;
                if pane.pending.as_ref().is_some_and(|p| p.seq == *pending_seq) {
                    (pane.pending, pane.pending_published) = (None, None);
                }
            }
            BindingTarget::Rejected {
                payload_session_id,
                reason,
                ..
            } => pane.rejected = Some((payload_session_id.clone(), reason.clone())),
        }
        debug_assert!(
            record.provider != "claude" || pane.claude.awaits(pane.pending.as_ref()),
            "[I-P] the writer's Pending and the fold's awaiting session diverged at seq {}",
            record.seq
        );
        reclaimed
    }

    /// What the pane's pin makes of `seen`, an observation of the proposal's path.
    fn pin_judgment(&self, p: &Proposal, seen: Observation) -> PinJudgment {
        let pane = self.panes.get(p.tmux_session).filter(|pane| pane.verified);
        let pin = pane.and_then(|pane| pane.current.as_ref());
        let session = p.session().or(pin.map(|pin| pin.session_id.as_str()));
        judge_pin(pin, session.unwrap_or_default(), Path::new(p.path), seen)
    }

    /// Whether `p`'s prompt outlived the Pending its pane waits on.
    fn reclaims(&self, p: &Proposal) -> bool {
        let pane = self.panes.get(p.tmux_session);
        let waiting =
            pane.and_then(|pane| Some(Waiting::of(pane.pending.as_ref()?, pane.pending_published)));
        let (event, published_at) = (
            p.hook.map(|h| h.event.as_str()),
            p.hook.and_then(|h| h.published_at),
        );
        let session = p.session().unwrap_or_default();
        claude_fold::reclaims(p.provider, event, session, published_at, waiting)
    }

    fn plan(&mut self, p: &Proposal, mode: Plan) -> Planned {
        let reclaim = matches!(mode, Plan::Reclaim(_));
        if reclaim && !self.reclaims(p) {
            return Planned::Keep(Committed::Unchanged);
        }
        let mode = match mode {
            Plan::Reclaim(source) => Plan::Verified(source),
            mode => mode,
        };
        let stat =
            matches!(mode, Plan::Stat).then(|| fs::metadata(p.path).map(|m| file_identity(&m)));
        // Only a stat reads the file; a Pending judgment has none and a verified one brings its own.
        let file = match mode {
            Plan::Stat => stat.as_ref().and_then(|stat| stat.as_ref().ok().copied()),
            Plan::Verified(source) | Plan::Reclaim(source) => Some((source.dev, source.ino)),
            Plan::ForcePending | Plan::Rejected(_) => None,
        };
        let seen = match (mode, stat) {
            (Plan::Verified(source), _) => Some(Observation::Checked(source)),
            (_, Some(stat)) => Some(Observation::Stat(stat)),
            _ => None,
        };
        // Every source a registration or a check logs is held to the pane's pin first.
        match seen.map(|seen| self.pin_judgment(p, seen)) {
            Some(PinJudgment::Anomaly) => return Planned::Keep(Committed::Anomaly),
            Some(PinJudgment::Recheck) => return Planned::Keep(Committed::Recheck),
            _ => {}
        }
        let pane = self.panes.entry(p.tmux_session.to_owned()).or_default();
        let session = p.session();
        let replaced = p.replaced.filter(|(path, id)| {
            let id = id.map(str::trim).filter(|id| !id.is_empty());
            *path != p.path || id.zip(session).is_some_and(|(a, b)| a != b)
        });
        let replaced = replaced.and_then(|(path, id)| {
            let meta = fs::metadata(path).ok()?;
            Some(source_id(id, path, file_identity(&meta)))
        });
        let old = pane.current.clone().or(replaced);
        let payload_session_id = session.unwrap_or_default().to_owned();
        let pending = pane.pending.as_ref();
        let pending = pending.filter(|e| pending_matches(e, session, p.path));
        // A verified source is no change only once its record is pinned and no Pending waits on it.
        let unsettled = matches!(mode, Plan::Verified(_)) && (!pane.verified || pending.is_some());
        let (new, inherited) = if let Plan::ForcePending = mode {
            if pending.is_some() {
                return Planned::Keep(Committed::Unchanged);
            }
            let payload_transcript_path = p.payload_path();
            let new = BindingTarget::Pending {
                payload_session_id,
                payload_transcript_path,
            };
            (new, None)
        } else if let Plan::Rejected(reason) = mode {
            let judged = (payload_session_id.clone(), reason.to_owned());
            if pane.rejected.as_ref() == Some(&judged) {
                return Planned::Keep(Committed::Unchanged);
            }
            let payload_transcript_path = p.payload_path();
            let reason = reason.to_owned();
            let new = BindingTarget::Rejected {
                payload_session_id,
                payload_transcript_path,
                reason,
            };
            (new, None)
        } else if pane
            .current
            .as_ref()
            .is_some_and(|current| same_source(current, session, p.path, file))
            && !unsettled
            && !reclaim
        {
            return Planned::Keep(Committed::Unchanged);
        } else {
            match (file, pending) {
                (None, Some(_)) => return Planned::Keep(Committed::Unchanged),
                (None, None) => {
                    let payload_transcript_path = p.payload_path();
                    let new = BindingTarget::Pending {
                        payload_session_id,
                        payload_transcript_path,
                    };
                    (new, None)
                }
                (Some(file), Some(pending)) => {
                    let source = source_id(session, p.path, file);
                    let pending_seq = pending.seq;
                    let new = BindingTarget::Resolved {
                        pending_seq,
                        source,
                    };
                    (new, Some(pending.clone()))
                }
                (Some(file), None) => (
                    BindingTarget::Source(source_id(session, p.path, file)),
                    None,
                ),
            }
        };
        let verified = matches!(mode, Plan::Verified(_));
        // Pinning the source it already names is not a new source, so it has no parent.
        let repinned = matches!(&new, BindingTarget::Source(s) if pane.current.as_ref() == Some(s));
        // As the fold judges it: only a re-pin of the source the pane holds verified reclaims.
        #[cfg(test)]
        let pinned = pane.verified || n2b_mutant("r5-reclaim-first-pin");
        #[cfg(not(test))]
        let pinned = pane.verified;
        if reclaim && !(repinned && pinned) {
            return Planned::Keep(Committed::Unchanged);
        }
        let rejected = matches!(mode, Plan::Rejected(_)).then_some(());
        let nonce = match observe_spawn_nonce_marker(p.tmux_session) {
            SpawnNonceMarker::Known(nonce) => Some(nonce),
            _ => None,
        };
        let (cause, parent_hint) = match inherited {
            Some(pending) => (pending.cause, pending.parent_hint),
            None => {
                let cause = match p.cause {
                    CauseSource::Hook(cause) => cause,
                    CauseSource::Observed => BindingCause::Unknown,
                    // A later record of an execution already in the log is not its launch.
                    CauseSource::Launch if nonce.is_none() || pane.nonce == nonce => {
                        BindingCause::Unknown
                    }
                    CauseSource::Launch => {
                        match nonce.as_deref().and_then(|n| launch_mode(p.provider, n)) {
                            Some(mode) if mode == "fresh" => BindingCause::Startup,
                            Some(mode) if mode == "resume" => BindingCause::Resume,
                            _ => BindingCause::Unknown,
                        }
                    }
                };
                let derived = matches!(
                    cause,
                    BindingCause::Fork | BindingCause::Compact | BindingCause::Continuation
                );
                let parent = (derived && rejected.is_none() && !repinned)
                    .then(|| old.clone())
                    .flatten();
                (cause, parent)
            }
        };
        let hook_event = p.hook.map(|hook| hook.event.clone());
        let received_at = p.hook.map_or_else(Utc::now, |hook| hook.received_at);
        let published_at = p.hook.and_then(|hook| hook.published_at);
        let event = BindingEvent {
            seq: self.last_seq + 1,
            channel_id: p.channel_id,
            provider: p.provider.to_owned(),
            tmux_session: p.tmux_session.to_owned(),
            execution_nonce: nonce,
            old,
            new,
            cause,
            parent_hint,
            evidence: BindingEvidence {
                hook_event,
                received_at,
            },
            committed_at: Utc::now(),
        };
        Planned::Append(event, verified, published_at)
    }

    /// Append and fsync one line; on failure the line is cut back off so no reader sees it.
    fn append(
        &mut self,
        path: &Path,
        record: &BindingEvent,
        verified: bool,
        published_at: Option<DateTime<Utc>>,
        codex_ownership: Option<&codex::Ownership>,
    ) -> io::Result<()> {
        let logged = LoggedRef {
            event: record,
            verified,
            published_at,
            codex_ownership,
        };
        let mut line = serde_json::to_vec(&logged).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = OpenOptions::new().write(true).create(true).open(path)?;
        let start = file.seek(SeekFrom::End(0))?;
        let parents_synced = self.parents_synced;
        let mut write = || -> io::Result<()> {
            fault("write")?;
            file.write_all(&line)?;
            fault("sync")?;
            file.sync_all()?;
            if !parents_synced {
                // The first append of this process also makes the file and directory entries durable.
                fsync_parent_dir(path)?;
                path.parent().map_or(Ok(()), fsync_parent_dir)?;
            }
            Ok(())
        };
        if let Err(error) = write() {
            if file.set_len(start).and_then(|()| file.sync_all()).is_err() {
                self.poisoned = true;
            }
            return Err(error);
        }
        self.parents_synced = true;
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    pub(crate) static APPEND_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Test builds log only under a root the test sets, never under the real runtime root.
#[cfg(test)]
fn events_dir() -> io::Result<Option<PathBuf>> {
    let root = TEST_ROOT.with(|root| root.borrow().clone());
    Ok(root.map(|root| root.join(BINDING_EVENTS_DIR)))
}

#[cfg(test)]
fn fault(step: &str) -> io::Result<()> {
    match APPEND_FAULT.with(|fault| fault.get()) {
        Some(armed) if armed == step => Err(io::Error::other(format!("injected {step}"))),
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(crate) fn test_root() -> Option<PathBuf> {
    TEST_ROOT.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
pub(crate) fn set_test_root(root: Option<&Path>) {
    TEST_ROOT.with(|slot| *slot.borrow_mut() = root.map(Path::to_path_buf));
}

/// Drops what this process remembers about `channel_id`, as a restart would.
#[cfg(test)]
pub(crate) fn forget_channel_for_tests(channel_id: u64) {
    if let Ok(Some(path)) = log_path(channel_id) {
        lock_logs().remove(&path);
    }
}

#[cfg(test)]
mod lane_tests;

#[cfg(test)]
mod codex_claim_tests;
