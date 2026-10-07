//! Dispositions for inputs crossing between the Legacy queue and the input ledger.

use super::attempt::WitnessKind;
use super::rows::{AbandonReason, DoneReason, HeldReason, Row, RowState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveSource {
    Queue,
    DispatchOnly,
    TurnRow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composer {
    Empty,
    Draft,
}

// `user_record`: the transcript already holds this input's user record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveEvidence {
    pub user_record: bool,
    pub turn_open: bool,
    // Positive producer evidence of a pre-effect refusal/never-started attempt.
    pub never_started: bool,
    pub composer: Composer,
}

// State a moved input is staged with; a queued input was never pasted, so the composer is irrelevant.
pub fn move_disposition(source: MoveSource, evidence: MoveEvidence) -> RowState {
    match (source, evidence.user_record) {
        (MoveSource::Queue, false) => RowState::Received,
        (MoveSource::Queue, true) => RowState::Done(DoneReason::AcceptedBeforeMove),
        (_, true) if evidence.turn_open => RowState::Running,
        (_, true) => RowState::Done(DoneReason::Completed),
        (_, false) => match evidence.composer {
            Composer::Empty if evidence.never_started => RowState::Received,
            Composer::Empty => RowState::Held(HeldReason::Ambiguous),
            Composer::Draft => RowState::Held(HeldReason::Ambiguous),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handback {
    // Enqueue durably into Legacy, then record what `handback_after_enqueue` returns.
    Enqueue,
    Close(RowState),
    NoticeThenClose(RowState),
    /// Keep the row as it is and hold the handback with a notice.
    Hold,
    Settled,
}

/// What a row's attempts are known to have caused, strongest evidence first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reconciliation {
    /// A model input record of some generation carries the row.
    ModelConfirmed,
    /// A tool or modal consumed the frame; it must not be delivered again.
    Consumed,
    /// A provider queue holds the frame without a model record yet.
    QueueOnly,
    Ambiguous,
    /// Positive evidence that nothing reached the provider.
    NeverSent,
}

impl Reconciliation {
    /// The ledger's own witnesses and per-generation effects.
    pub fn from_row(row: &Row) -> Self {
        let kinds = || row.witnesses.iter().map(|seen| seen.witness.kind);
        if kinds().any(WitnessKind::confirms_input) {
            Self::ModelConfirmed
        } else if kinds().any(|kind| kind == WitnessKind::Tool) {
            Self::Consumed
        } else if kinds().any(WitnessKind::queue_only) {
            Self::QueueOnly
        } else if before_effect(row.state) && row.attempts.iter().all(|a| a.effect.none()) {
            Self::NeverSent
        } else {
            Self::Ambiguous
        }
    }

    /// A host's model-record flag; an empty composer never proves a pasted row did nothing.
    pub fn from_legacy(state: RowState, accepted: bool) -> Self {
        if accepted {
            Self::ModelConfirmed
        } else if before_effect(state) {
            Self::NeverSent
        } else {
            Self::Ambiguous
        }
    }
}

// The actor enters these only before a paste or after a positive pre-effect refusal.
fn before_effect(state: RowState) -> bool {
    matches!(
        state,
        RowState::Received
            | RowState::Ready
            | RowState::Held(HeldReason::Modal | HeldReason::NotReady)
    )
}

/// Only positive no-effect evidence returns a row to Legacy; a queued copy may still be delivered.
pub fn handback_plan(state: RowState, evidence: Reconciliation) -> Handback {
    if state.is_terminal() {
        return Handback::Settled;
    }
    match (evidence, state) {
        (Reconciliation::ModelConfirmed, _) => {
            Handback::Close(RowState::Done(DoneReason::HandbackRunning))
        }
        (Reconciliation::Consumed, _) => {
            Handback::NoticeThenClose(RowState::Done(DoneReason::HandbackRunning))
        }
        (Reconciliation::QueueOnly, _) | (_, RowState::Queued) => Handback::Hold,
        (Reconciliation::NeverSent, state) if state != RowState::Running => Handback::Enqueue,
        _ => Handback::NoticeThenClose(RowState::Held(HeldReason::Ambiguous)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Persisted,
    AlreadyPreserved,
    Rejected,
}

// A rejected enqueue keeps the row open so the next boot retries; only a durable copy closes it.
pub fn handback_after_enqueue(outcome: EnqueueOutcome) -> Option<RowState> {
    match outcome {
        EnqueueOutcome::Persisted | EnqueueOutcome::AlreadyPreserved => {
            Some(RowState::Abandoned(AbandonReason::Handback))
        }
        EnqueueOutcome::Rejected => None,
    }
}
