//! The Claude sessions one pane moved through in the execution its records name. Only
//! `Writer::apply` folds it, so a reloaded writer and the live one hold the same history.

use crate::services::tui_o::shadow::{SourceId, capture::same_file};
#[cfg(unix)]
use crate::services::tui_o::shadow::{binding_reader::source_id_for, capture::SourceCapture};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
#[cfg(unix)]
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use chrono::{DateTime, Utc};

use super::super::binding_context::BindingContext;
use super::{BindingEvent, BindingTarget, Logged, Writer, lock_logs, log_path, read_log};
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::claude_tui::source_verify::{Left, SourceHistory, Visit};

const THREAD_WRITER_POLICY: &str = "claude_thread_writer:";

impl BindingContext {
    #[cfg(unix)]
    pub(crate) fn claude_thread_writer_policy(parent: u64) -> String {
        format!("{THREAD_WRITER_POLICY}{parent}")
    }

    pub(crate) fn claude_thread_writer_parent(&self) -> Option<u64> {
        if self.provider != "claude" || self.launch_mode != "fresh" {
            return None;
        }
        self.source_policy
            .as_deref()?
            .strip_prefix(THREAD_WRITER_POLICY)?
            .parse()
            .ok()
    }

    pub(crate) fn retains_delivery_context(&self) -> bool {
        if self.provider == "codex" {
            return self.source_policy.as_deref() == Some("verified");
        }
        let (Some(channel), Some(parent)) = (self.channel_id, self.claude_thread_writer_parent())
        else {
            return false;
        };
        let disposition = crate::config::runtime_root()
            .as_deref()
            .and_then(crate::services::tui_o::store::OStore::existing)
            .and_then(|store| store.read_init(channel).ok().flatten());
        !disposition.is_some_and(|init| {
            init.policy_parent == Some(parent)
                && init.sources.iter().any(|source| {
                    self.expected_native_session_id.as_deref()
                        == Some(source.source_id.session_id.as_str())
                })
        })
    }

    /// Clear preserves the transcript. An unknown or existing file may still owe delivery.
    pub(crate) fn missing_claude_transcript(&self, native: &str, path: &std::path::Path) -> bool {
        self.expected_native_session_id.as_deref() == Some(native)
            && self.provider_root.as_deref().is_some_and(|root| {
                crate::services::claude_tui::source_verify::is_top_level_transcript(
                    root, path, native,
                )
            })
            && std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    }
}

/// Left sessions kept per execution; past it the history is marked incomplete instead.
const LEFT_LIMIT: usize = 1024;

/// A verified pin and the commit time of the record that pinned it.
type Pin = (Option<super::SourceId>, Option<DateTime<Utc>>);

/// How one Claude record moves its pane, judged before it applies; the writer's waiting Pending
/// and the fold both follow this one judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// A hook adopted another session: a waiting Pending it does not resolve is superseded.
    Switch,
    /// A hook naming the session the pane already has; it is no transition.
    Confirm,
    /// The Resolved of the Pending the writer waits on.
    Resolve,
    /// A prompt of the pane's own session published after the waiting Pending, which it supersedes.
    Reclaim,
    /// A record no hook produced: a registration, restore or stat.
    Observe,
    Await,
    Audit,
}

/// The Pending the writer waits on for a pane, and the publish time of the hook behind it.
#[derive(Clone, Copy)]
pub(super) struct Waiting<'a> {
    pub seq: u64,
    pub session: &'a str,
    pub published_at: Option<DateTime<Utc>>,
}

impl<'a> Waiting<'a> {
    pub(super) fn of(pending: &'a BindingEvent, published_at: Option<DateTime<Utc>>) -> Self {
        let session = match &pending.new {
            BindingTarget::Pending {
                payload_session_id, ..
            } => payload_session_id.as_str(),
            _ => "",
        };
        Self {
            seq: pending.seq,
            session,
            published_at,
        }
    }
}

/// Whether a prompt naming `session` outlived the waiting Pending of another session: a prompt is
/// published only while Claude holds its session, so the waiting one was left before it.
pub(super) fn reclaims(
    provider: &str,
    event: Option<&str>,
    session: &str,
    published_at: Option<DateTime<Utc>>,
    waiting: Option<Waiting>,
) -> bool {
    let prompt = event == Some(HookEventKind::UserPromptSubmit.as_str());
    let Some(waiting) = waiting else {
        return false;
    };
    let later = matches!((published_at, waiting.published_at), (Some(t), Some(since)) if t > since);
    provider == "claude" && prompt && waiting.session != session && later
}

/// `published_at` is the record's hook publish time; `waiting` the pane's Pending, if any;
/// `repinned` whether the record re-pins the source the pane already held verified.
pub(super) fn step(
    record: &BindingEvent,
    published_at: Option<DateTime<Utc>>,
    waiting: Option<Waiting>,
    repinned: bool,
) -> Step {
    let source = match &record.new {
        BindingTarget::Pending { .. } => return Step::Await,
        BindingTarget::Rejected { .. } => return Step::Audit,
        BindingTarget::Resolved { pending_seq, .. }
            if waiting.is_some_and(|w| w.seq == *pending_seq) =>
        {
            return Step::Resolve;
        }
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
    };
    // `old` is the pane's current source when the record was planned.
    let moved = (record.old.as_ref()).is_none_or(|old| old.session_id != source.session_id);
    let hooked = record.evidence.hook_event.is_some();
    match (hooked, moved) {
        (false, _) => Step::Observe,
        (true, true) => Step::Switch,
        // A first pin is written whether or not the reclaim gate held, so only a re-pin reclaims.
        (true, false) => {
            let event = record.evidence.hook_event.as_deref();
            let session = source.session_id.as_str();
            match repinned && reclaims(&record.provider, event, session, published_at, waiting) {
                true => Step::Reclaim,
                false => Step::Confirm,
            }
        }
    }
}

/// `binding_events_since` with whether each record superseded its pane's waiting Pending as a
/// prompt reclaim, judged by replaying the log through the writer's own fold.
pub(crate) fn binding_events_judged_since(
    channel_id: u64,
    after_seq: u64,
) -> io::Result<Vec<(BindingEvent, bool)>> {
    let Some(path) = log_path(channel_id)? else {
        return Ok(Vec::new());
    };
    // Holding the lock keeps a record that is being rolled back out of every read.
    let _logs = lock_logs();
    let read = read_log::<Logged>(&path)?;
    if read.lines != read.records.len() as u64 {
        let unreadable = read.lines - read.records.len() as u64;
        return Err(io::Error::other(format!(
            "{unreadable} unreadable binding event line(s) in {}",
            path.display()
        )));
    }
    let mut replay = Writer::default();
    let records = read.records.into_iter().map(|line| {
        let reclaimed = replay.apply(&line.event, line.verified, line.published_at);
        (line.event, reclaimed)
    });
    Ok(records
        .filter(|(record, _)| record.seq > after_seq)
        .collect())
}

/// Reads the committed hook verification evidence in one pass for a thread takeover.
pub(crate) fn verified_claude_sources(channel: u64) -> io::Result<Vec<BindingEvent>> {
    let Some(path) = log_path(channel)? else {
        return Ok(Vec::new());
    };
    let _logs = lock_logs();
    let read = read_log::<Logged>(&path)?;
    if read.lines != read.records.len() as u64 {
        return Err(io::Error::other("incomplete Claude binding history"));
    }
    Ok(read
        .records
        .into_iter()
        .filter_map(|record| {
            let event = record.event;
            (record.verified && event.channel_id == channel && event.provider == "claude")
                .then_some(event)
        })
        .collect())
}

/// Whether a hook-verified record or a published resume frontier of this channel names `native`.
#[cfg(unix)]
pub(crate) fn names_session(channel: u64, native: &str) -> io::Result<bool> {
    let bound = |event: &BindingEvent| {
        matches!(&event.new,
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. }
            if source.session_id == native)
    };
    Ok(read_thread_resumes(channel)?
        .iter()
        .any(|entry| entry.source.session_id == native)
        || verified_claude_sources(channel)?.iter().any(bound))
}

pub(crate) fn names_source(event: &BindingEvent, session: &str, source: &SourceId) -> bool {
    event.tmux_session == session
        && matches!(&event.new,
        BindingTarget::Source(bound) | BindingTarget::Resolved { source: bound, .. }
        if bound.session_id == source.session_id && same_file(bound, source))
}

/// Observes only a fold already published by the writer; never reloads, repairs or syncs a log.
pub(crate) fn published_claude_history(
    channel: u64,
    session: &str,
    nonce: &str,
) -> Option<(u64, SourceHistory)> {
    let path = log_path(channel).ok()??;
    let logs = lock_logs();
    let writer = logs.get(&path)?.writer.as_ref()?;
    let pane = writer.panes.get(session)?;
    Some((
        writer.last_seq,
        pane.claude.history(Some(nonce), writer.tainted),
    ))
}

#[derive(Clone, Debug, Default)]
pub(super) struct ClaudeFold {
    nonce: Option<String>,
    history: SourceHistory,
    /// Seq of the Pending record behind `history.awaiting`.
    awaiting_seq: Option<u64>,
}

/// The current execution's stat identity, only while no verified pin or other waiting session owns it.
pub(crate) fn unverified_current_source(
    channel_id: u64,
    tmux_session: &str,
    nonce: &str,
) -> io::Result<Option<super::SourceId>> {
    let mut found = None;
    super::commit_with(channel_id, |writer| {
        if let Some(pane) = writer.panes.get(tmux_session)
            && !pane.verified
            && pane.nonce.as_deref() == Some(nonce)
            && let Some(source) = &pane.current
        {
            let history = pane.claude.history(Some(nonce), writer.tainted);
            if history.complete
                && history
                    .current
                    .as_ref()
                    .is_some_and(|v| v.session == source.session_id)
                && history
                    .awaiting
                    .as_ref()
                    .is_none_or(|v| v.session == source.session_id)
            {
                found = Some(source.clone());
            }
        }
        super::Planned::Keep(super::Committed::Unchanged)
    })?;
    Ok(found)
}

impl ClaudeFold {
    /// `tainted`: an unreadable line or seq gap came right before this record.
    pub(super) fn apply(
        &mut self,
        record: &BindingEvent,
        verified: bool,
        published_at: Option<DateTime<Utc>>,
        step: Step,
        tainted: bool,
    ) {
        match record.execution_nonce.as_deref() {
            Some(nonce) if self.nonce.as_deref() != Some(nonce) => {
                let fresh = true;
                if fresh {
                    *self = Self::default();
                    self.history.complete = !tainted;
                }
                self.nonce = Some(nonce.to_owned());
            }
            Some(_) => {}
            None => self.history.complete = false,
        }
        let at = published_at.unwrap_or(record.evidence.received_at);
        let hooked = record.evidence.hook_event.is_some();
        let (source, valid) = match &record.new {
            BindingTarget::Rejected { .. } => return,
            BindingTarget::Pending {
                payload_session_id: session,
                ..
            } => {
                let waiting = self.history.awaiting.take();
                if let Some(waiting) = waiting.filter(|w| hooked && w.session != *session) {
                    self.leave(waiting.session, (None, None), at);
                }
                self.history.left.remove(session);
                let since = hooked.then_some(at);
                self.history.awaiting = Some(visit(session, (None, None), since, at));
                self.awaiting_seq = Some(record.seq);
                return;
            }
            BindingTarget::Source(source) => (source, false),
            BindingTarget::Resolved {
                pending_seq,
                source,
            } => (source, self.awaiting_seq == Some(*pending_seq)),
        };
        // A pin carries the commit time of its own record, never a visit's or a move's.
        let pin = (
            verified.then(|| source.clone()),
            verified.then_some(record.committed_at),
        );
        let session = &source.session_id;
        if valid && step == Step::Resolve {
            let since = self.history.awaiting.as_ref().and_then(|w| w.since);
            match since.or(hooked.then_some(at)) {
                Some(since) => self.adopt(session, pin, since, at),
                None => {
                    (self.history.awaiting, self.awaiting_seq) = (None, None);
                    self.observe(session, pin, at);
                }
            }
            return;
        }
        // A Resolved of a Pending already superseded counts from its own time.
        match step {
            Step::Switch | Step::Resolve if hooked => self.adopt(session, pin, at, at),
            Step::Confirm => self.confirm(session, pin, at),
            Step::Reclaim => {
                let leaves = true;
                if let Some(waiting) = leaves.then(|| self.history.awaiting.take()).flatten() {
                    self.leave(waiting.session, (None, None), at);
                    self.awaiting_seq = None;
                }
                self.confirm(session, pin, at);
            }
            _ => self.observe(session, pin, at),
        }
    }

    /// A hook naming the session the pane already has: it pins it and dates the visit if undated.
    fn confirm(&mut self, session: &str, pin: Pin, at: DateTime<Utc>) {
        if self.current_is(session) {
            let current = self.history.current.as_mut().expect("current checked");
            if current
                .pin
                .as_ref()
                .zip(pin.0.as_ref())
                .is_some_and(|(old, new)| old.path != new.path)
            {
                current.since = Some(at);
            }
            (current.pin, current.pinned_at) = pin;
            current.since.get_or_insert(at);
        } else {
            self.history.current = Some(visit(session, pin, Some(at), at));
        }
    }

    /// The pane moved to `session` at `since`: the current and waiting sessions are left then.
    fn adopt(&mut self, session: &str, pin: Pin, since: DateTime<Utc>, seen: DateTime<Utc>) {
        if let Some(current) = self.history.current.take().filter(|c| c.session != session) {
            self.leave(current.session, (current.pin, current.pinned_at), since);
        }
        let waiting = self.history.awaiting.take();
        self.awaiting_seq = None;
        if let Some(waiting) = waiting.filter(|w| w.session != session) {
            self.leave(waiting.session, (None, None), since);
        }
        self.history.left.remove(session);
        self.history.current = Some(visit(session, pin, Some(since), seen));
    }

    /// A record no hook proved: it names the current session without making a left one.
    fn observe(&mut self, session: &str, pin: Pin, seen: DateTime<Utc>) {
        if self.current_is(session) {
            let current = self.history.current.as_mut().expect("current checked");
            (current.pin, current.pinned_at) = pin;
        } else {
            self.history.current = Some(visit(session, pin, None, seen));
        }
        self.history.left.remove(session);
    }

    fn leave(&mut self, session: String, (pin, pinned_at): Pin, left_at: DateTime<Utc>) {
        let history = &mut self.history;
        if !history.left.contains_key(&session) && history.left.len() >= LEFT_LIMIT {
            history.complete = false;
            return;
        }
        let left = Left {
            pin,
            pinned_at,
            left_at,
        };
        history.left.insert(session, left);
    }

    fn current_is(&self, session: &str) -> bool {
        (self.history.current.as_ref()).is_some_and(|current| current.session == session)
    }

    /// A later record found an unreadable line or seq gap before it: nothing here is complete.
    pub(super) fn taint(&mut self) {
        self.history.complete = false;
    }

    /// The history as execution `nonce` sees it; another execution's sessions are not its own.
    pub(super) fn history(&self, nonce: Option<&str>, tainted: bool) -> SourceHistory {
        if self.nonce.as_deref() == nonce {
            return self.history.clone();
        }
        SourceHistory {
            complete: nonce.is_some() && !tainted,
            ..SourceHistory::default()
        }
    }

    /// Whether the writer's waiting Pending is the one this fold awaits, when both are of its
    /// execution.
    pub(super) fn awaits(&self, pending: Option<&BindingEvent>) -> bool {
        let ours =
            pending.filter(|p| p.execution_nonce.is_some() && p.execution_nonce == self.nonce);
        ours.is_none_or(|pending| self.awaiting_seq == Some(pending.seq))
    }
}

fn visit(
    session: &str,
    (pin, pinned_at): Pin,
    since: Option<DateTime<Utc>>,
    seen: DateTime<Utc>,
) -> Visit {
    let session = session.to_owned();
    Visit {
        session,
        pin,
        pinned_at,
        since,
        seen,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ThreadResume {
    pub(crate) parent: u64,
    pub(crate) tmux: String,
    pub(crate) source: SourceId,
    pub(crate) offset: u64,
    pub(crate) sha256: String,
}

fn thread_resume_path(channel: u64) -> io::Result<PathBuf> {
    crate::config::runtime_root()
        .map(|root| {
            root.join("runtime/claude_thread_resumes")
                .join(format!("{channel}.json"))
        })
        .ok_or_else(|| io::Error::other("runtime root unavailable"))
}

pub(crate) fn read_thread_resumes(channel: u64) -> io::Result<Vec<ThreadResume>> {
    match std::fs::read(thread_resume_path(channel)?) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
pub(crate) fn check_thread_resume(
    channel: u64,
    parent: u64,
    tmux: &str,
    native: &str,
    expected: Option<&str>,
    path: &Path,
) -> io::Result<()> {
    let expected = expected.unwrap_or(native);
    let entries = read_thread_resumes(channel)?;
    let mut relevant = entries
        .iter()
        .filter(|entry| {
            entry.parent == parent && entry.tmux == tmux && entry.source.session_id == expected
        })
        .peekable();
    if relevant.peek().is_none() {
        return Ok(());
    }
    if expected != native {
        return Err(io::Error::other(
            "the Claude resume native session changed before its first key",
        ));
    }
    let resume = relevant
        .find(|entry| entry.source.path == path)
        .ok_or_else(|| io::Error::other("the Claude resume source changed before its first key"))?;
    SourceCapture::reopen(resume.source.clone(), resume.offset, &resume.sha256).map(|_| ())
}

#[cfg(unix)]
fn effort_ended(channel: u64, tmux: &str, source: &SourceId, cutoff: u64, hash: &str) -> bool {
    use super::super::binding_context::{SpawnNonceMarker, observe_spawn_nonce_marker};
    use crate::services::tui_o::{shadow::ShadowProvider, store::restart_end};
    let (Some(root), Ok(Ok(events))) = (
        crate::config::runtime_root(),
        super::records_strict(channel),
    ) else {
        return false;
    };
    let Some(bound) = events.iter().rev().find(|event| {
        event.channel_id == channel
            && event.provider == "claude"
            && event.tmux_session == tmux
            && !matches!(&event.new, BindingTarget::Rejected { .. })
    }) else {
        return false;
    };
    let (BindingTarget::Source(pinned) | BindingTarget::Resolved { source: pinned, .. }) =
        &bound.new
    else {
        return false;
    };
    let Some(nonce) = bound
        .execution_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())
    else {
        return false;
    };
    let same_launch = match observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Absent => true,
        SpawnNonceMarker::Known(current) => current == nonce,
        SpawnNonceMarker::Unreadable => false,
    };
    if !same_launch || !same_file(pinned, source) {
        return false;
    }
    restart_end::read_durable(&root, channel).is_ok_and(|ends| {
        ends.iter().any(|end| {
            end.end_id.starts_with("effort-")
                && end.channel == channel
                && end.provider == ShadowProvider::Claude
                && end.tmux_session == tmux
                && end.execution_nonce == nonce
                && end.binding_seq == bound.seq
                && end.source == *pinned
                && end.cutoff == cutoff
                && end.prefix_hash == hash
        })
    })
}

#[cfg(unix)]
pub(crate) fn reserve_thread_resume(
    channel: u64,
    parent: u64,
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    session_id: &str,
    (transcript, cold): (&Path, bool),
    before_new_boundary: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};
    let tmux = authority.session();
    let source = source_id_for(session_id, transcript)?;
    let mut existing = read_thread_resumes(channel)?;
    if let Some(resume) = existing.iter().find(|entry| {
        entry.parent == parent && entry.tmux == tmux && same_file(&entry.source, &source)
    }) {
        return SourceCapture::reopen(source, resume.offset, &resume.sha256).map(|_| ());
    }
    before_new_boundary()?;
    if !verified_claude_sources(channel)?
        .iter()
        .any(|event| names_source(event, tmux, &source))
    {
        return Err(io::Error::other(
            "Claude thread transcript has no verified binding",
        ));
    }
    let mut file = File::open(transcript)?;
    let offset = file.metadata()?.len();
    if offset != 0 {
        let mut last = [0];
        file.seek(SeekFrom::Start(offset - 1))?;
        file.read_exact(&mut last)?;
        if last != [b'\n'] {
            return Err(io::Error::other(
                "Claude transcript is not at a closed turn boundary",
            ));
        }
    }
    let after = source_id_for(session_id, transcript)?;
    if source != after
        || crate::services::tui_o::shadow::capture::file_identity(&file.metadata()?)
            != (source.dev, source.ino)
    {
        return Err(io::Error::other(
            "Claude thread transcript changed during reservation",
        ));
    }
    let capture = SourceCapture::open(source.clone(), offset)?;
    let sha256 = capture.prefix_hash();
    if offset != 0
        && observe_claude_jsonl_turn_state(transcript) != TuiTurnState::Idle
        && !(cold && effort_ended(channel, tmux, &source, offset, &sha256))
    {
        return Err(io::Error::other(
            "Claude transcript is not at a closed turn boundary",
        ));
    }
    capture.verify_eof(offset, &sha256)?;
    existing.push(ThreadResume {
        parent,
        tmux: tmux.into(),
        source,
        offset,
        sha256,
    });
    let target = thread_resume_path(channel)?;
    let directory = target
        .parent()
        .ok_or_else(|| io::Error::other("thread resume directory unavailable"))?;
    super::super::binding_context::durable_directory(directory)?;
    let data = serde_json::to_string(&existing)?;
    crate::domain::atomic_write::prepare(&target, &data)
        .and_then(|write| write.publish())
        .map_err(|failure| failure.error)?;
    crate::domain::file_sync::fsync_parent_dir(&target)?;
    let saved = read_thread_resumes(channel)?;
    if saved != existing {
        return Err(io::Error::other(
            "Claude thread frontier changed after publication",
        ));
    }
    Ok(())
}
