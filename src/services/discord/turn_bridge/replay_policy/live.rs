//! What one live attempt was seen to do, and the evidence a complete observation can give.

use crate::db::replay_disposition::write::ExactAttempt;

/// Activity after start that makes an automatic rerun unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activity {
    Output,
    Tool,
    Unclassified,
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
    /// A malformed record, read or send error, dropped receiver or stderr error.
    Broken,
    Activity(Activity),
    Terminal(TerminalKind),
}

/// Raw record of one exact attempt; an incomplete record never reads as an empty one.
#[derive(Debug)]
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
        self.seen
            .iter()
            .filter_map(|seen| match seen {
                Seen::Terminal(kind) => Some(*kind),
                _ => None,
            })
            .collect()
    }

    fn active(&self) -> bool {
        self.seen
            .iter()
            .any(|seen| matches!(seen, Seen::Activity(_)))
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
