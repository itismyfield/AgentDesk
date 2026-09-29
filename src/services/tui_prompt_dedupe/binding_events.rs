//! Per-channel binding event log: each record is appended and fsynced before the binding it names is published.
//! Readers use `binding_events_since` and `subscribe_binding_events`; nothing here deletes a record.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::TuiRuntimeBinding;
use super::binding_context::{SpawnNonceMarker, launch_mode, observe_spawn_nonce_marker};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::runtime_store::fsync_parent_dir;
pub(crate) use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;

pub(crate) const BINDING_EVENTS_DIR: &str = "binding_events";
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
}

impl HookSignal {
    pub(crate) fn from_payload(event: &str, payload: &serde_json::Value) -> Self {
        let text = |key: &str| payload.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        Self {
            event: event.to_owned(),
            source: text("source"),
            transcript_path: text("transcript_path"),
            received_at: Utc::now(),
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

#[derive(Default)]
struct PaneState {
    current: Option<SourceId>,
    pending: Option<BindingEvent>,
    rejected: Option<String>,
    nonce: Option<String>,
}

struct Writer {
    last_seq: u64,
    panes: HashMap<String, PaneState>,
    parents_synced: bool,
    poisoned: bool,
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

struct LogRead {
    records: Vec<BindingEvent>,
    lines: u64,
    complete_len: u64,
    total_len: u64,
}

/// Complete lines only: a torn tail is left out, and a corrupt line is skipped with a warning.
fn read_log(path: &Path) -> io::Result<LogRead> {
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

/// Records of `channel_id` with `seq > after_seq`, in log order. Read-only.
pub(crate) fn binding_events_since(
    channel_id: u64,
    after_seq: u64,
) -> io::Result<Vec<BindingEvent>> {
    let Some(path) = log_path(channel_id)? else {
        return Ok(Vec::new());
    };
    // Holding the lock keeps a record that is being rolled back out of every read.
    let _logs = lock_logs();
    let records = read_log(&path)?.records.into_iter();
    Ok(records.filter(|record| record.seq > after_seq).collect())
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
    let last = read_log(&path)?.records.iter().map(|r| r.seq).max();
    let notify = watch::channel(last.unwrap_or(0)).0;
    let log = logs.entry(path).or_insert(ChannelLog {
        notify,
        writer: None,
    });
    Ok(log.notify.subscribe())
}

/// Appends what `proposal` changes for its pane, if anything; `Ok` means the binding may be published.
pub(crate) fn record_source(proposal: &Proposal) -> io::Result<()> {
    commit(proposal, None)
}

/// Audits a candidate the binding judgment refused; the binding itself stays as it is.
pub(crate) fn record_rejected(proposal: &Proposal, reason: &str) -> io::Result<()> {
    commit(proposal, Some(reason))
}

fn commit(proposal: &Proposal, rejected: Option<&str>) -> io::Result<()> {
    commit_with(proposal.channel_id, |writer| {
        writer.plan(proposal, rejected)
    })
    .map(|_| ())
}

fn commit_with(
    channel_id: u64,
    plan: impl FnOnce(&mut Writer) -> Option<BindingEvent>,
) -> io::Result<bool> {
    let Some(path) = log_path(channel_id)? else {
        return Ok(false);
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
        return Ok(false);
    };
    let Some(record) = plan(writer) else {
        return Ok(false);
    };
    if let Err(error) = writer.append(&path, &record) {
        // A line that could not be cut back off is re-read from disk before the next append.
        if writer.poisoned {
            log.writer = None;
        }
        return Err(error);
    }
    writer.apply(&record);
    log.notify.send_replace(record.seq);
    Ok(true)
}

fn source_id(session: Option<&str>, path: &str, meta: &fs::Metadata) -> SourceId {
    let (dev, ino) = file_identity(meta);
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
    meta: Option<&fs::Metadata>,
) -> bool {
    current.path == Path::new(path)
        && session.is_none_or(|id| current.session_id.is_empty() || current.session_id == id)
        && meta.is_none_or(|meta| file_identity(meta) == (current.dev, current.ino))
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
        let read = read_log(path)?;
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
        };
        read.records.iter().for_each(|record| writer.apply(record));
        Ok(writer)
    }

    fn apply(&mut self, record: &BindingEvent) {
        self.last_seq = self.last_seq.max(record.seq);
        let pane = self.panes.entry(record.tmux_session.clone()).or_default();
        if record.execution_nonce.is_some() {
            pane.nonce = record.execution_nonce.clone();
        }
        match &record.new {
            BindingTarget::Source(source) => pane.current = Some(source.clone()),
            BindingTarget::Pending { .. } => pane.pending = Some(record.clone()),
            BindingTarget::Resolved {
                pending_seq,
                source,
            } => {
                pane.current = Some(source.clone());
                if pane.pending.as_ref().is_some_and(|p| p.seq == *pending_seq) {
                    pane.pending = None;
                }
            }
            BindingTarget::Rejected {
                payload_session_id, ..
            } => pane.rejected = Some(payload_session_id.clone()),
        }
    }

    fn plan(&mut self, p: &Proposal, rejected: Option<&str>) -> Option<BindingEvent> {
        let pane = self.panes.entry(p.tmux_session.to_owned()).or_default();
        let session = p.session();
        let replaced = p.replaced.filter(|(path, id)| {
            let id = id.map(str::trim).filter(|id| !id.is_empty());
            *path != p.path || id.zip(session).is_some_and(|(a, b)| a != b)
        });
        let replaced = replaced.and_then(|(path, id)| {
            let meta = fs::metadata(path).ok()?;
            Some(source_id(id, path, &meta))
        });
        let old = pane.current.clone().or(replaced);
        let payload_session_id = session.unwrap_or_default().to_owned();
        let meta = fs::metadata(p.path);
        let (new, inherited) = if let Some(reason) = rejected {
            if pane.rejected.as_deref() == Some(payload_session_id.as_str()) {
                return None;
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
            .is_some_and(|current| same_source(current, session, p.path, meta.as_ref().ok()))
        {
            return None;
        } else {
            let pending = pane.pending.as_ref();
            let pending = pending.filter(|e| pending_matches(e, session, p.path));
            match (meta, pending) {
                (Err(_), Some(_)) => return None,
                (Err(_), None) => {
                    let payload_transcript_path = p.payload_path();
                    let new = BindingTarget::Pending {
                        payload_session_id,
                        payload_transcript_path,
                    };
                    (new, None)
                }
                (Ok(meta), Some(pending)) => {
                    let source = source_id(session, p.path, &meta);
                    let pending_seq = pending.seq;
                    let new = BindingTarget::Resolved {
                        pending_seq,
                        source,
                    };
                    (new, Some(pending.clone()))
                }
                (Ok(meta), None) => (
                    BindingTarget::Source(source_id(session, p.path, &meta)),
                    None,
                ),
            }
        };
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
                let parent = (derived && rejected.is_none())
                    .then(|| old.clone())
                    .flatten();
                (cause, parent)
            }
        };
        let hook_event = p.hook.map(|hook| hook.event.clone());
        let received_at = p.hook.map_or_else(Utc::now, |hook| hook.received_at);
        Some(BindingEvent {
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
        })
    }

    /// Append and fsync one line; on failure the line is cut back off so no reader sees it.
    fn append(&mut self, path: &Path, record: &BindingEvent) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
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
