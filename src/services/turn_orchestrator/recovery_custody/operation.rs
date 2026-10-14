//! The one root operation a custody row carries, and the owned collector that records its step
//! result even after the caller has gone.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::model::{CanonicalSeal, Namespace};

const MAX_SOURCES: usize = 64;
const MAX_HANDOFFS: usize = 8;

/// Whether the root was admitted for the legacy path or a scoped install.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Root {
    Legacy,
    Scoped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    LegacyConsume,
    ScopedInstall,
    ReturnOnly,
    ExactRelease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Reserved,
    MarkerPublish,
    QueueCommit,
    ActorInstall,
    SourceReturn,
    ExactRelease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageStatus {
    ReservedNotArmed,
    ApplyArmed,
    KnownOutcome,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveOperation {
    pub(crate) operation_id: String,
    pub(crate) root: Root,
    pub(crate) kind: OperationKind,
    /// The effects this operation may ever apply.
    pub(crate) allowed: Vec<Stage>,
    pub(crate) target: Namespace,
    pub(crate) sources: Vec<u64>,
    pub(crate) issued_revision: i64,
    pub(crate) stage: Stage,
    pub(crate) status: StageStatus,
    /// Stages whose effect is known to be published; a later refusal never forgets them.
    pub(crate) applied: Vec<Stage>,
    pub(crate) before: CanonicalSeal,
    pub(crate) expected_after: Option<CanonicalSeal>,
    /// Follow-up effects or handoffs already granted (e.g. a queue-exit debt event-id); reused,
    /// never re-issued.
    pub(crate) handoffs: Vec<String>,
    /// The grant a resumed stage must reuse; arming it with any other grant is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reuse: Option<String>,
    /// The current arm's identity: a hold keeps it, only the next arm replaces it, and only its
    /// own report records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arm_id: Option<String>,
}

impl ActiveOperation {
    pub(crate) fn check_bounds(&self) -> Result<(), String> {
        if self.sources.len() > MAX_SOURCES || self.handoffs.len() > MAX_HANDOFFS {
            return Err("operation exceeds its bounded progress".into());
        }
        Ok(())
    }

    /// Records a granted follow-up once; a re-reported id never grows the bounded list.
    pub(crate) fn record_handoff(&mut self, id: &str) {
        if !self.handoffs.iter().any(|known| known == id) {
            self.handoffs.push(id.to_string());
        }
    }

    pub(crate) fn has_applied(&self) -> bool {
        !self.applied.is_empty()
    }
}

/// What the applier reports for the armed stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    Applied {
        after: Box<CanonicalSeal>,
        handoff: Option<String>,
    },
    NotApplied,
    Indeterminate {
        detail: String,
    },
}

/// The applier's half: dropping it without a report records the stage as indeterminate.
pub(crate) struct OutcomeReport(oneshot::Sender<StepOutcome>);

impl OutcomeReport {
    pub(crate) fn pair() -> (Self, oneshot::Receiver<StepOutcome>) {
        let (sender, receiver) = oneshot::channel();
        (Self(sender), receiver)
    }

    pub(crate) fn send(self, outcome: StepOutcome) {
        // A closed receiver means the collector already settled the stage as indeterminate.
        let _ = self.0.send(outcome);
    }
}

pub(crate) async fn await_report(receiver: oneshot::Receiver<StepOutcome>) -> StepOutcome {
    receiver
        .await
        .unwrap_or_else(|_| StepOutcome::Indeterminate {
            detail: "the applier ended without reporting".into(),
        })
}
