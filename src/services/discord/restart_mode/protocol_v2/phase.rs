use super::{disposition::DecodeDisposition, values::*};

mod codec;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhaseIdentityV2 {
    request_id: RequestIdV2,
    attempt_id: AttemptIdV2,
    provider: ProviderIdentityV2,
    channel: ChannelIdentityV2,
    nonce: NonceV2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EventDigestV2([u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhaseEventV2 {
    identity: PhaseIdentityV2,
    sequence: u64,
    previous_hash: Option<EventDigestV2>,
    event_hash: EventDigestV2,
    kind: PhaseKindV2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PhaseKindV2 {
    Bound,
    Started,
    Terminal {
        outcome: TerminalOutcomeV2,
        terminal_proof: SafeRelativeRefV2,
    },
    Receipt {
        durable_receipt: SafeRelativeRefV2,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalOutcomeV2 {
    Completed,
    RolledBack,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhaseHeadV2 {
    sequence: u64,
    event_hash: EventDigestV2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReducedPhaseStateV2 {
    BoundPending {
        identity: PhaseIdentityV2,
        head: PhaseHeadV2,
    },
    RunningPending {
        identity: PhaseIdentityV2,
        head: PhaseHeadV2,
    },
    TerminalWithoutReceipt {
        identity: PhaseIdentityV2,
        head: PhaseHeadV2,
        outcome: TerminalOutcomeV2,
        terminal_proof: SafeRelativeRefV2,
    },
    TerminalWithReceipt {
        identity: PhaseIdentityV2,
        head: PhaseHeadV2,
        outcome: TerminalOutcomeV2,
        terminal_proof: SafeRelativeRefV2,
        durable_receipt: SafeRelativeRefV2,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhasePoisonV2 {
    MalformedJson,
    InvalidCurrentRecord,
    InvalidEventHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhaseConflictV2 {
    EmptyHistory,
    SequenceGap { expected: u64, actual: u64 },
    DuplicateSequence { sequence: u64 },
    ReorderedSequence { previous: u64, actual: u64 },
    PreviousHashMismatch,
    IdentityMismatch,
    IllegalFirstPhase,
    RepeatedBound,
    RepeatedStarted,
    RepeatedTerminal,
    TerminalBeforeStart,
    ReceiptBeforeTerminal,
    EventAfterReceipt,
}

type PhaseEventDispositionV2 = DecodeDisposition<PhaseEventV2, PhaseConflictV2, PhasePoisonV2>;
type PhaseReductionDispositionV2 =
    DecodeDisposition<ReducedPhaseStateV2, PhaseConflictV2, PhasePoisonV2>;
