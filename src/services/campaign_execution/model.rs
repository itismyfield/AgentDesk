use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::{DefinitionHashes, DefinitionInput, ValidatedDefinition};

macro_rules! domain_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
domain_enum!(Repository { Adk, CookingHeart });
domain_enum!(CampaignControl {
    Prepared,
    Running,
    Stopping,
    Stopped,
    Succeeded
});
domain_enum!(StopMode {
    Immediate,
    FinishStage
});
domain_enum!(Stage {
    Requirements,
    Design,
    DesignReview,
    Implementation,
    Review,
    Landing,
    MainVerification,
    FinalVerification,
    Complete
});
domain_enum!(AttemptOutcome {
    Passed,
    QualityFailed,
    UserInterrupted,
    EnvironmentWait,
    Superseded
});
domain_enum!(NodeDisposition {
    Pending,
    Assigned,
    Running,
    Verifying,
    Failed,
    Blocked,
    Interrupted,
    Complete
});
domain_enum!(Role {
    Implementation,
    Review
});
domain_enum!(ActionKind {
    Launch,
    DeliverStage,
    CreatePr,
    Merge,
    Deploy
});
domain_enum!(ActionState {
    Planned,
    Claimed,
    StartPending,
    Started,
    Reconciled,
    Cancelled,
    Unknown
});
domain_enum!(ExecutionReadiness { Unavailable });
domain_enum!(RejectionCode {
    InvalidDefinition,
    StaleDefinition,
    StaleRun,
    StaleBinding,
    InvalidState,
    ExecutionUnavailable,
    SessionsNotGone,
    UnreconciledActions,
    FenceIncomplete,
    StaleFence,
    IdempotencyConflict,
    MissingChangeReason,
    CompletedNodeChanged,
    AlreadyExists
});
domain_enum!(ReasonCode {
    ExecutionUnavailable,
    ReviewResources,
    HostResources,
    OwnedElsewhere,
    PredecessorMain,
    PredecessorFailed,
    ObservationUnknown,
    StateNeedsInspection
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub code: RejectionCode,
    pub target: Option<String>,
}
impl Rejection {
    pub fn new(code: RejectionCode) -> Self {
        Self { code, target: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HostGate {
    pub host: String,
    pub gate: String,
    pub incarnation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub gate: HostGate,
    pub session: String,
    pub incarnation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionObservation {
    Alive {
        observed_at: DateTime<Utc>,
    },
    Gone {
        observed_at: DateTime<Utc>,
        evidence: String,
    },
    Unknown {
        reason: String,
        last_observed_at: Option<DateTime<Utc>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub identity: SessionIdentity,
    pub attempt_id: String,
    pub observation: SessionObservation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptBinding {
    pub campaign_id: String,
    pub run_id: String,
    pub node_id: String,
    pub series: u32,
    pub stage: Stage,
    pub attempt_id: String,
    pub role: Role,
    pub session: SessionIdentity,
    pub repository: Repository,
    pub hashes: DefinitionHashes,
    pub workflow_version: String,
    pub base_sha: String,
    pub candidate_sha: String,
    pub control_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub binding: AttemptBinding,
    pub started: bool,
    pub outcome: Option<AttemptOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub binding: AttemptBinding,
    pub kind: ActionKind,
    pub target: HostGate,
    pub state: ActionState,
    pub initiation_receipt: Option<String>,
    pub reconciliation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceAcknowledgement {
    pub target: HostGate,
    pub control_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopIntent {
    pub mode: StopMode,
    pub requested_at: DateTime<Utc>,
    pub targets: BTreeSet<HostGate>,
    pub acknowledgements: Vec<FenceAcknowledgement>,
    pub finish_attempts: BTreeSet<String>,
    pub confirmed_event: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub control_version: u64,
    pub stop: Option<StopIntent>,
    pub attempts: BTreeMap<String, Attempt>,
    pub actions: BTreeMap<String, Action>,
    pub sessions: Vec<SessionRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRuntime {
    pub stage: Stage,
    pub disposition: NodeDisposition,
    pub series: u32,
    pub quality_failures: u32,
    pub repair_cycles: u32,
    pub review_rounds: u32,
    pub current_attempt: Option<String>,
    pub main_integrated: bool,
    pub acceptance_complete: bool,
    pub evidence_current: bool,
    pub wait_reason: Option<ReasonCode>,
}
impl Default for NodeRuntime {
    fn default() -> Self {
        Self {
            stage: Stage::Requirements,
            disposition: NodeDisposition::Pending,
            series: 1,
            quality_failures: 0,
            repair_cycles: 0,
            review_rounds: 0,
            current_attempt: None,
            main_integrated: false,
            acceptance_complete: false,
            evidence_current: false,
            wait_reason: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignState {
    pub id: String,
    pub definition_revision: u64,
    pub definition: ValidatedDefinition,
    pub control: CampaignControl,
    pub run: Option<Run>,
    pub nodes: BTreeMap<String, NodeRuntime>,
    pub event_sequence: u64,
    pub last_event_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    EditDefinition {
        expected_revision: u64,
        definition: DefinitionInput,
        change: Option<ChangeReason>,
    },
    Start {
        expected_revision: u64,
    },
    Resume {
        expected_revision: u64,
        expected_run: String,
    },
    PrepareRetry {
        expected_revision: u64,
        expected_run: String,
        node_id: String,
    },
    RequestStop {
        expected_revision: u64,
        expected_run: String,
        mode: StopMode,
    },
    PlanAction {
        action: Box<Action>,
    },
    AdvanceAction {
        action_id: String,
        binding: Box<AttemptBinding>,
        state: ActionState,
    },
    ObserveAction {
        action_id: String,
        binding: Box<AttemptBinding>,
        observation: ActionObservation,
    },
    ObserveSession {
        identity: SessionIdentity,
        observation: SessionObservation,
    },
    AcknowledgeFence {
        expected_run: String,
        target: HostGate,
        control_version: u64,
    },
    ConfirmStopFence,
    ConfirmStopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeReason {
    pub reference: String,
    pub explanation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionObservation {
    Started { receipt: String },
    NotStarted { evidence: String },
    Reconciled { evidence: String },
    Unknown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub idempotency_key: String,
    pub observed_at: DateTime<Utc>,
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    DefinitionCreated,
    DefinitionEdited {
        superseded_nodes: Vec<String>,
        change: Option<ChangeReason>,
    },
    StopRequested {
        mode: StopMode,
    },
    StopEscalated,
    StopConfirmed,
    RunStopped,
    ActionChanged {
        action_id: String,
    },
    SessionObserved {
        identity: SessionIdentity,
    },
    FenceAcknowledged {
        target: HostGate,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub run_id: Option<String>,
    pub command_id: String,
    pub observed_at: DateTime<Utc>,
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    pub definition_revision: u64,
    pub run_id: Option<String>,
    pub control_version: Option<u64>,
    pub event_sequence: u64,
    pub control: CampaignControl,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub state: CampaignState,
    pub events: Vec<Event>,
    pub result: CommandResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub fingerprint: String,
    pub result: Result<CommandResult, Rejection>,
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
