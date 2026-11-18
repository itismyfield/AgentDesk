use serde::{Deserialize, Serialize};

use super::model::Stage;
use super::workflow_options::ApprovalGate;

// One lifecycle position of a node. The epoch moves with every handover, interruption and
// resume, so a step recomputed after a restart names the same IDs and a later one never does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepKey {
    pub run_id: String,
    pub node_id: String,
    pub series: u32,
    pub repair_cycle: u32,
    pub epoch: u64,
}

impl StepKey {
    pub fn attempt_id(&self, stage: Stage) -> String {
        let role = match stage {
            Stage::Review => "review",
            _ => "work",
        };
        format!(
            "{}:{}:s{}:c{}:e{}:{role}",
            self.run_id, self.node_id, self.series, self.repair_cycle, self.epoch
        )
    }

    pub fn launch_id(&self, stage: Stage) -> String {
        format!("launch:{}", self.attempt_id(stage))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    NotRunning,
    Interrupted,
    ObservationUnknown,
    PredecessorMain,
    BudgetExhausted,
    Complete,
    Unsupported,
    NeedsInspection,
}

// What the server does next for one node, computed from the saved state alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum NextAction {
    AssignWork { key: StepKey },
    BeginReview { key: StepKey, work_attempt: String },
    BeginLanding { key: StepKey },
    CompleteNode { key: StepKey },
    // The boundary's approval slot has no gate for the current target yet.
    OpenGate { gate: ApprovalGate },
    AwaitApproval { gate: ApprovalGate },
    // The open attempt, or the evidence of a later stage, has to answer first.
    AwaitAttempt { attempt_id: String },
    AwaitEvidence { stage: Stage },
    Hold { reason: HoldReason },
}
