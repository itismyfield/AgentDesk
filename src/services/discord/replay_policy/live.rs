//! What one live attempt was seen to do, and the evidence a complete observation can give.

use crate::db::replay_disposition::write::ExactAttempt;
use crate::services::agent_protocol::StreamMessage;
use std::sync::Mutex;
pub(crate) mod seal;
mod tap;
pub(crate) use tap::RawTap;

/// Activity after start that makes an automatic rerun unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activity {
    Output,
    Tool,
    Unclassified,
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
    seal: seal::Seal,
}

impl RawObservation {
    fn new(attempt: &ExactAttempt) -> Self {
        Self {
            receipt_id: attempt.receipt_id(),
            nonce: attempt.nonce().to_string(),
            seen: Vec::new(),
            seal: seal::Seal::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_attempt(attempt: &ExactAttempt) -> Self {
        let mut observation = Self::new(attempt);
        observation.seal = seal::Seal::fixture();
        observation
    }

    fn append(&mut self, seen: Seen) {
        self.seen.push(seen);
    }

    #[cfg(test)]
    pub(crate) fn record(&mut self, seen: Seen) {
        self.append(seen);
    }

    /// The reader has nothing more to add: stdout ended, the child was waited and stderr drained,
    /// or the record broke.
    fn finished(&self) -> bool {
        self.seal.closed || self.seen.contains(&Seen::Broken)
    }

    fn complete(&self) -> bool {
        self.seal.complete()
            && [
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

/// A provider terminal from its complete reader.
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
            observation: Mutex::new(RawObservation::new(&attempt)),
            attempt: Mutex::new(Some(attempt)),
            recorded: tokio::sync::Notify::new(),
            verdict: Mutex::new(None),
            stale: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            after_check: Mutex::new(Vec::new()),
        }
    }

    fn record_fact(&self, seen: Seen) {
        lock(&self.observation).append(seen);
        self.recorded.notify_waiters();
    }

    /// Hands the attempt to its one decision once the reader finished, or what it saw by `wait`.
    pub(crate) async fn take_for_decision(
        &self,
        wait: std::time::Duration,
    ) -> Option<(ExactAttempt, RawObservation)> {
        self.take(wait, false).await
    }

    pub(super) async fn take_for_terminal(
        &self,
        wait: std::time::Duration,
    ) -> Option<(ExactAttempt, RawObservation)> {
        self.take(wait, true).await
    }

    async fn take(
        &self,
        wait: std::time::Duration,
        terminal: bool,
    ) -> Option<(ExactAttempt, RawObservation)> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Registered before the state check, so a fact recorded after the check still wakes it.
            let recorded = self.recorded.notified();
            tokio::pin!(recorded);
            recorded.as_mut().enable();
            let finished = lock(&self.observation).finished();
            #[cfg(test)]
            {
                let injected = std::mem::take(&mut *lock(&self.after_check));
                if !injected.is_empty() {
                    for seen in injected {
                        self.record_fact(seen);
                    }
                    lock(&self.observation).seal.closed = true;
                    self.recorded.notify_waiters();
                }
            }
            if finished || tokio::time::timeout_at(deadline, recorded).await.is_err() {
                break;
            }
        }
        let mut attempt = lock(&self.attempt);
        let observation = lock(&self.observation).clone();
        if self.verdict().is_some()
            || (terminal && self.stale() && observation.no_effect_evidence().is_some())
        {
            return None;
        }
        Some((attempt.take()?, observation))
    }

    #[cfg(test)]
    pub(crate) fn record(&self, seen: Seen) {
        lock(&self.observation).seal = seal::Seal::fixture();
        self.record_fact(seen);
    }

    #[cfg(test)]
    pub(super) fn fixture_pending(&self) {
        lock(&self.observation).seal.closed = false;
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(all(test, unix))]
#[path = "live/observer_tests.rs"]
mod observer_tests;

pub(crate) use seal::can_activate;
