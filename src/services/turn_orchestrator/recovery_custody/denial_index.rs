//! The sync writers' refusal index for attached namespaces. It is a mirror, not the authority: the
//! PG operation is. An exact operation becomes visible only after its arm committed.

use std::collections::HashMap;
use std::sync::Mutex;

use super::operation::Stage;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexState {
    /// Restore or judgement not finished: every consume, change or destructive cleanup is refused.
    Unknown,
    /// Undecided or owned elsewhere: general changes are refused.
    Blocked,
    /// Only this durably armed operation's stage may apply.
    ExactOperation { operation_id: String, stage: Stage },
}

/// A commit-confirmed arm; only the core builds one, after the arm transaction committed.
pub(super) struct CommittedArm<'a> {
    pub(super) channel_id: &'a str,
    pub(super) operation_id: &'a str,
    pub(super) stage: Stage,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Denial(pub(crate) IndexState);

#[derive(Default)]
pub(crate) struct DenialIndex {
    states: Mutex<HashMap<String, IndexState>>,
}

impl DenialIndex {
    fn states(&self) -> std::sync::MutexGuard<'_, HashMap<String, IndexState>> {
        self.states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An attached channel with no entry yet reads as `Unknown`.
    pub(crate) fn state(&self, channel_id: &str) -> IndexState {
        self.states()
            .get(channel_id)
            .cloned()
            .unwrap_or(IndexState::Unknown)
    }

    pub(super) fn block(&self, channel_id: &str) {
        self.states()
            .insert(channel_id.to_string(), IndexState::Blocked);
    }

    pub(super) fn publish(&self, arm: CommittedArm<'_>) {
        self.states().insert(
            arm.channel_id.to_string(),
            IndexState::ExactOperation {
                operation_id: arm.operation_id.to_string(),
                stage: arm.stage,
            },
        );
    }

    /// A sync writer's check: it passes only as the published exact operation's stage.
    pub(crate) fn admit_write(
        &self,
        channel_id: &str,
        exact: Option<(&str, Stage)>,
    ) -> Result<(), Denial> {
        match self.state(channel_id) {
            IndexState::ExactOperation {
                operation_id,
                stage,
            } if exact == Some((operation_id.as_str(), stage)) => Ok(()),
            state => Err(Denial(state)),
        }
    }
}
