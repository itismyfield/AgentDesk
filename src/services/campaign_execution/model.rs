pub use crate::domain::campaign::model::{
    Action, ActionKind, ActionObservation, ActionState, Attempt, AttemptBinding, AttemptExecution,
    AttemptOutcome, CampaignControl, CampaignState, ChangeReason, Command, CommandEnvelope,
    CommandResult, Decision, Event, EventKind, EvidenceWrite, ExecutionReadiness,
    FenceAcknowledgement, FinishGrant, HostGate, NodeDisposition, NodeFence, NodeInterruption,
    NodeRuntime, PhaseFact, ReasonCode, Receipt, Rejection, RejectionCode, Repository, Role, Run,
    SessionIdentity, SessionObservation, SessionRecord, Stage, StopIntent, StopMode,
    WorkAssignment,
};

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
