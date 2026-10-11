//! What one live attempt was seen to do, and the evidence a complete observation can give.

use crate::db::replay_disposition::write::ExactAttempt;
use crate::services::agent_protocol::StreamMessage;
use std::sync::Mutex;

/// Activity after start that makes an automatic rerun unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activity {
    Output,
    Tool,
    Unclassified,
    /// Recorded by the TUI injection path, which is not connected yet.
    #[cfg(test)]
    InjectedInput,
}

/// A terminal the provider itself reported, not one a wrapper synthesised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Done,
    Error,
}

/// One raw fact from the attempt's reader, recorded before any parsing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    StdoutEof,
    /// The child's wait result; a failed wait leaves the observation broken.
    Exited {
        waited: bool,
    },
    StderrDrained,
    /// A malformed record, read or send error, dropped receiver, stderr error or cancellation.
    Broken,
    Activity(Activity),
    Terminal(TerminalKind),
}

impl Seen {
    fn terminal(&self) -> Option<TerminalKind> {
        match self {
            Self::Terminal(kind) => Some(*kind),
            _ => None,
        }
    }

    fn activity(&self) -> bool {
        matches!(self, Self::Activity(_))
    }

    /// What one parsed stdout record shows; only the `result` record is the provider's terminal.
    fn of_record(record_type: &str, message: Option<&StreamMessage>) -> Option<Self> {
        use StreamMessage as M;
        Some(match message {
            Some(M::Done { .. }) if record_type == "result" => Self::Terminal(TerminalKind::Done),
            Some(M::Error { .. }) if record_type == "result" => Self::Terminal(TerminalKind::Error),
            Some(M::Text { .. } | M::Thinking { .. }) => Self::Activity(Activity::Output),
            Some(M::ToolUse { .. } | M::ToolResult { .. }) => Self::Activity(Activity::Tool),
            Some(M::TaskNotification { .. }) | None => Self::Activity(Activity::Unclassified),
            Some(_) => return None,
        })
    }
}

/// Raw record of one exact attempt; an incomplete record never reads as an empty one.
#[derive(Debug, Clone)]
pub(crate) struct RawObservation {
    receipt_id: i64,
    nonce: String,
    seen: Vec<Seen>,
}

impl RawObservation {
    pub(crate) fn for_attempt(attempt: &ExactAttempt) -> Self {
        Self {
            receipt_id: attempt.receipt_id(),
            nonce: attempt.nonce().to_string(),
            seen: Vec::new(),
        }
    }

    pub(crate) fn record(&mut self, seen: Seen) {
        self.seen.push(seen);
    }

    /// The reader has nothing more to add: stdout ended, the child was waited and stderr drained,
    /// or the record broke.
    fn finished(&self) -> bool {
        self.seen.contains(&Seen::Broken)
            || [Seen::StdoutEof, Seen::StderrDrained]
                .iter()
                .all(|needed| self.seen.contains(needed))
                && self
                    .seen
                    .iter()
                    .any(|seen| matches!(seen, Seen::Exited { .. }))
    }

    fn complete(&self) -> bool {
        [
            Seen::StdoutEof,
            Seen::Exited { waited: true },
            Seen::StderrDrained,
        ]
        .iter()
        .all(|needed| self.seen.contains(needed))
            && !self
                .seen
                .iter()
                .any(|seen| matches!(seen, Seen::Broken | Seen::Exited { waited: false }))
    }

    fn terminals(&self) -> Vec<TerminalKind> {
        self.seen.iter().filter_map(Seen::terminal).collect()
    }

    fn active(&self) -> bool {
        self.seen.iter().any(Seen::activity)
    }

    /// Normal-classification evidence: a complete observation with exactly one provider terminal.
    pub(crate) fn terminal_evidence(&self) -> Option<TerminalEvidence> {
        match self.terminals().as_slice() {
            [kind] if self.complete() => Some(TerminalEvidence {
                receipt_id: self.receipt_id,
                nonce: self.nonce.clone(),
                kind: *kind,
            }),
            _ => None,
        }
    }

    /// No-effect evidence: complete, no activity at all, and no successful provider turn.
    pub(crate) fn no_effect_evidence(&self) -> Option<NoEffectEvidence> {
        let finished_turn = self.terminals().contains(&TerminalKind::Done);
        (self.complete() && !self.active() && !finished_turn).then(|| NoEffectEvidence {
            receipt_id: self.receipt_id,
            nonce: self.nonce.clone(),
        })
    }

    pub(crate) fn hold_reason(&self) -> &'static str {
        if self.active() {
            "activity after start"
        } else {
            "incomplete or unclassified observation"
        }
    }
}

/// The provider's own terminal for one exact attempt, drawn from a complete observation.
#[derive(Debug)]
pub(crate) struct TerminalEvidence {
    receipt_id: i64,
    nonce: String,
    kind: TerminalKind,
}

impl TerminalEvidence {
    pub(crate) fn names(&self, attempt: &ExactAttempt) -> bool {
        self.receipt_id == attempt.receipt_id() && self.nonce == attempt.nonce()
    }

    pub(crate) fn kind(&self) -> TerminalKind {
        self.kind
    }
}

/// Proof that one exact attempt produced no output, tool, input or provider turn.
#[derive(Debug)]
pub(crate) struct NoEffectEvidence {
    receipt_id: i64,
    nonce: String,
}

impl NoEffectEvidence {
    pub(crate) fn names(&self, attempt: &ExactAttempt) -> bool {
        self.receipt_id == attempt.receipt_id() && self.nonce == attempt.nonce()
    }
}

/// How a protected attempt's terminal was settled; only `Ordinary` admits ordinary effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Ordinary,
    Retried,
    Held,
}

/// The in-process record of one started protected attempt. Only the token of the executor that
/// spent its start permit carries one, so a restored or foreign turn never decides with it.
#[derive(Debug)]
pub(crate) struct LiveAttempt {
    attempt: Mutex<Option<ExactAttempt>>,
    observation: Mutex<RawObservation>,
    recorded: tokio::sync::Notify,
    verdict: Mutex<Option<Verdict>>,
    /// The bridge saw a stale resume; only the stale-resume decision may judge this attempt.
    stale: std::sync::atomic::AtomicBool,
    /// Facts a test records between a decision's state check and its wait.
    #[cfg(test)]
    pub(crate) after_check: Mutex<Vec<Seen>>,
}

impl LiveAttempt {
    pub(crate) fn new(attempt: ExactAttempt) -> Self {
        Self {
            observation: Mutex::new(RawObservation::for_attempt(&attempt)),
            attempt: Mutex::new(Some(attempt)),
            recorded: tokio::sync::Notify::new(),
            verdict: Mutex::new(None),
            stale: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            after_check: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn record(&self, seen: Seen) {
        lock(&self.observation).record(seen);
        self.recorded.notify_waiters();
    }

    /// Hands the attempt to its one decision once the reader finished, or what it saw by `wait`.
    pub(crate) async fn take_for_decision(
        &self,
        wait: std::time::Duration,
    ) -> Option<(ExactAttempt, RawObservation)> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Registered before the state check, so a fact recorded after the check still wakes it.
            let recorded = self.recorded.notified();
            tokio::pin!(recorded);
            recorded.as_mut().enable();
            let finished = lock(&self.observation).finished();
            #[cfg(test)]
            for seen in std::mem::take(&mut *lock(&self.after_check)) {
                self.record(seen);
            }
            if finished || tokio::time::timeout_at(deadline, recorded).await.is_err() {
                break;
            }
        }
        let attempt = lock(&self.attempt).take()?;
        Some((attempt, lock(&self.observation).clone()))
    }

    pub(crate) fn settle(&self, verdict: Verdict) {
        *lock(&self.verdict) = Some(verdict);
    }

    pub(crate) fn verdict(&self) -> Option<Verdict> {
        *lock(&self.verdict)
    }

    pub(crate) fn mark_stale(&self) {
        self.stale.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn stale(&self) -> bool {
        self.stale.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// The direct adapter's tap on its reader. It records nothing unless the token carries a live
/// attempt, which only a protected start attaches.
pub(crate) struct RawTap(Option<std::sync::Arc<LiveAttempt>>);

impl RawTap {
    pub(crate) fn for_token(token: Option<&crate::services::provider::CancelToken>) -> Self {
        Self(token.and_then(|token| token.replay_attempt()).cloned())
    }

    pub(crate) fn active(&self) -> bool {
        self.0.is_some()
    }

    pub(crate) fn see(&self, seen: Seen) {
        if let Some(live) = &self.0 {
            live.record(seen);
        }
    }

    pub(crate) fn record_line(&self, record_type: &str, message: Option<&StreamMessage>) {
        if let Some(seen) = Seen::of_record(record_type, message) {
            self.see(seen);
        }
    }

    /// Reads stderr to its end and returns the fact to record once the last message is sent. A
    /// protected attempt waits at most `STDERR_SETTLE`; an unfinished or failed read is broken.
    pub(crate) fn read_stderr(&self, stderr: Option<std::process::ChildStderr>) -> (String, Seen) {
        let read = match (stderr, self.active()) {
            (None, _) => None,
            (Some(stderr), false) => std::io::read_to_string(stderr).ok(),
            (Some(stderr), true) => {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || tx.send(std::io::read_to_string(stderr)));
                rx.recv_timeout(STDERR_SETTLE).ok().and_then(Result::ok)
            }
        };
        match read {
            Some(text) => (text, Seen::StderrDrained),
            None => (String::new(), Seen::Broken),
        }
    }

    /// Records the stderr fact last, after any failed send, so a decision never judges a record
    /// that is complete but still missing that failure.
    pub(crate) fn finish(&self, delivered: bool, drained: Seen) {
        if !delivered {
            self.see(Seen::Broken);
        }
        self.see(drained);
    }
}

/// How long a protected attempt's reader waits for stderr to end after the child exited.
const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
