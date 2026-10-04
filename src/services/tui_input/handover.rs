//! Dispositions for inputs crossing between the Legacy queue and the input ledger.

use super::rows::{AbandonReason, DoneReason, HeldReason, RowState};

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
    Settled,
}

// `accepted`: a transcript user record or an acceptance witness exists for any generation of the row.
pub fn handback_plan(state: RowState, accepted: bool, composer: Composer) -> Handback {
    let reconcile = || match (accepted, composer) {
        (true, _) => Handback::Close(RowState::Done(DoneReason::HandbackRunning)),
        (false, Composer::Empty) => Handback::Enqueue,
        (false, Composer::Draft) => {
            Handback::NoticeThenClose(RowState::Held(HeldReason::Ambiguous))
        }
    };
    match state {
        RowState::Done(_) | RowState::Abandoned(_) => Handback::Settled,
        RowState::Received
        | RowState::Ready
        | RowState::Held(HeldReason::Modal | HeldReason::NotReady) => Handback::Enqueue,
        RowState::Held(HeldReason::Ambiguous)
        | RowState::Unaccepted
        | RowState::Injecting
        | RowState::AwaitTurn => reconcile(),
        RowState::Running if accepted => {
            Handback::Close(RowState::Done(DoneReason::HandbackRunning))
        }
        RowState::Running => Handback::NoticeThenClose(RowState::Held(HeldReason::Ambiguous)),
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
