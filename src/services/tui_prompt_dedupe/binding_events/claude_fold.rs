//! The Claude sessions one pane moved through in the execution its records name. Only
//! `Writer::apply` folds it, so a reloaded writer and the live one hold the same history.

use std::io;

use chrono::{DateTime, Utc};

use super::{BindingEvent, BindingTarget, Logged, Writer, lock_logs, log_path, read_log};
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::claude_tui::source_verify::{Left, SourceHistory, Visit};

/// Left sessions kept per execution; past it the history is marked incomplete instead.
const LEFT_LIMIT: usize = 1024;

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
    #[cfg(test)]
    let prompt = prompt || (event.is_some() && super::n2b_mutant("r5-reclaim-any-event"));
    let Some(waiting) = waiting else {
        return false;
    };
    let later = matches!((published_at, waiting.published_at), (Some(t), Some(since)) if t > since);
    #[cfg(test)]
    let later = later || super::n2b_mutant("r5-reclaim-time-off");
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
    #[cfg(test)]
    let moved = moved || super::n2b_mutant("supersede-same");
    let hooked = record.evidence.hook_event.is_some();
    #[cfg(test)]
    let hooked = hooked || super::n2b_mutant("supersede-nonhook");
    match (hooked, moved) {
        (false, _) => Step::Observe,
        (true, true) => Step::Switch,
        // A first pin is written whether or not the reclaim gate held, so only a re-pin reclaims.
        (true, false) => {
            let event = record.evidence.hook_event.as_deref();
            let session = source.session_id.as_str();
            #[cfg(test)]
            let repinned = repinned || super::n2b_mutant("r5-reclaim-first-pin");
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

#[derive(Clone, Debug, Default)]
pub(super) struct ClaudeFold {
    nonce: Option<String>,
    history: SourceHistory,
    /// Seq of the Pending record behind `history.awaiting`.
    awaiting_seq: Option<u64>,
    /// The last superseded Pending and its time, for the mutation that resolves it at that time.
    #[cfg(test)]
    dropped: Option<(u64, DateTime<Utc>)>,
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
                #[cfg(test)]
                let fresh = !super::n2b_mutant("nonce");
                #[cfg(not(test))]
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
        #[cfg(test)]
        let at = if super::n2b_mutant("recv") {
            record.evidence.received_at
        } else {
            at
        };
        let hooked = record.evidence.hook_event.is_some();
        let (source, valid) = match &record.new {
            BindingTarget::Rejected { .. } => return,
            BindingTarget::Pending {
                payload_session_id: session,
                ..
            } => {
                let waiting = self.history.awaiting.take();
                if let Some(waiting) = waiting.filter(|w| hooked && w.session != *session) {
                    self.leave(waiting.session, None, at);
                }
                self.history.left.remove(session);
                let since = hooked.then_some(at);
                let (session, pin) = (session.clone(), None);
                self.history.awaiting = Some(Visit {
                    session,
                    pin,
                    since,
                    seen: at,
                });
                self.awaiting_seq = Some(record.seq);
                return;
            }
            BindingTarget::Source(source) => (source, false),
            BindingTarget::Resolved {
                pending_seq,
                source,
            } => (source, self.awaiting_seq == Some(*pending_seq)),
        };
        let pin = verified.then(|| source.clone());
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
        #[cfg(test)]
        let at = match (&record.new, self.dropped) {
            (BindingTarget::Resolved { pending_seq, .. }, Some((seq, then)))
                if *pending_seq == seq && super::n2b_mutant("resolved-pair") =>
            {
                then
            }
            _ => at,
        };
        match step {
            Step::Switch | Step::Resolve if hooked => self.adopt(session, pin, at, at),
            Step::Confirm => self.confirm(session, pin, at),
            Step::Reclaim => {
                #[cfg(test)]
                let leaves = !super::n2b_mutant("r5-reclaim-fold-off");
                #[cfg(not(test))]
                let leaves = true;
                if let Some(waiting) = leaves.then(|| self.history.awaiting.take()).flatten() {
                    self.leave(waiting.session, None, at);
                    self.awaiting_seq = None;
                }
                self.confirm(session, pin, at);
            }
            _ => self.observe(session, pin, at),
        }
    }

    /// A hook naming the session the pane already has: it pins it and dates the visit if undated.
    fn confirm(&mut self, session: &str, pin: Option<super::SourceId>, at: DateTime<Utc>) {
        if self.current_is(session) {
            let current = self.history.current.as_mut().expect("current checked");
            current.pin = pin;
            current.since.get_or_insert(at);
        } else {
            self.history.current = Some(visit(session, pin, Some(at), at));
        }
    }

    /// The pane moved to `session` at `since`: the current and waiting sessions are left then.
    fn adopt(
        &mut self,
        session: &str,
        pin: Option<super::SourceId>,
        since: DateTime<Utc>,
        seen: DateTime<Utc>,
    ) {
        if let Some(current) = self.history.current.take().filter(|c| c.session != session) {
            self.leave(current.session, current.pin, since);
        }
        let waiting = self.history.awaiting.take();
        #[cfg(test)]
        if let (Some(seq), Some(w)) = (self.awaiting_seq, waiting.as_ref()) {
            self.dropped = Some((seq, w.since.unwrap_or(w.seen)));
        }
        self.awaiting_seq = None;
        if let Some(waiting) = waiting.filter(|w| w.session != session) {
            self.leave(waiting.session, None, since);
        }
        self.history.left.remove(session);
        self.history.current = Some(visit(session, pin, Some(since), seen));
    }

    /// A record no hook proved: it names the current session without making a left one.
    fn observe(&mut self, session: &str, pin: Option<super::SourceId>, seen: DateTime<Utc>) {
        if self.current_is(session) {
            self.history.current.as_mut().expect("current checked").pin = pin;
        } else {
            #[cfg(test)]
            if super::n2b_mutant("observed") {
                if let Some(current) = self.history.current.take() {
                    self.leave(current.session, current.pin, seen);
                }
            }
            self.history.current = Some(visit(session, pin, None, seen));
        }
        self.history.left.remove(session);
    }

    fn leave(&mut self, session: String, pin: Option<super::SourceId>, left_at: DateTime<Utc>) {
        let history = &mut self.history;
        if !history.left.contains_key(&session) && history.left.len() >= LEFT_LIMIT {
            history.complete = false;
            return;
        }
        history.left.insert(session, Left { pin, left_at });
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
        #[cfg(test)]
        let tainted = tainted && !super::n2b_mutant("complete");
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
    pin: Option<super::SourceId>,
    since: Option<DateTime<Utc>>,
    seen: DateTime<Utc>,
) -> Visit {
    let session = session.to_owned();
    Visit {
        session,
        pin,
        since,
        seen,
    }
}
