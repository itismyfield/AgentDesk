//! The sync writers' refusal index for attached namespaces. It is a mirror, not the authority: the
//! PG operation is. An exact operation becomes visible only after its arm committed.

use std::collections::HashMap;
use std::sync::Mutex;

use super::capability::{ApplyCapability, ApplyGuard, Carrier, RevokeHandle, Unapplied, Unsent};
use super::operation::Stage;
#[cfg(test)]
use super::seam::{self, Point};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexState {
    /// Restore or judgement not finished: every consume, change or destructive cleanup is refused.
    Unknown,
    /// Undecided or owned elsewhere: general changes are refused.
    Blocked,
    /// Only this durably armed operation's stage, under this arm, may apply.
    ExactOperation {
        operation_id: String,
        stage: Stage,
        arm_id: String,
    },
}

/// A commit-confirmed arm; only the core builds one, after the arm transaction committed.
pub(super) struct CommittedArm<'a> {
    pub(super) channel_id: &'a str,
    pub(super) revision: i64,
    pub(super) operation_id: &'a str,
    pub(super) stage: Stage,
    pub(super) arm_id: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Denial(pub(crate) IndexState);

/// Each entry carries the row revision whose commit set it. `arms` keeps every arm this process
/// issued per channel until it ends, so a closed row is reserved again only after them.
#[derive(Default)]
pub(crate) struct DenialIndex {
    states: Mutex<HashMap<String, (i64, IndexState)>>,
    arms: Mutex<HashMap<String, Vec<RevokeHandle>>>,
}

impl DenialIndex {
    fn states(&self) -> std::sync::MutexGuard<'_, HashMap<String, (i64, IndexState)>> {
        self.states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn unlocked(&self) -> bool {
        self.states.try_lock().is_ok()
    }

    /// An attached channel with no entry yet reads as `Unknown`.
    pub(crate) fn state(&self, channel_id: &str) -> IndexState {
        let states = self.states();
        states
            .get(channel_id)
            .map_or(IndexState::Unknown, |(_, state)| state.clone())
    }

    pub(super) fn track(&self, channel_id: &str, arm: RevokeHandle) {
        let mut arms = self
            .arms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        arms.entry(channel_id.to_string()).or_default().push(arm);
    }

    /// Whether every arm issued on the channel ended; ended ones are forgotten.
    pub(super) fn arms_ended(&self, channel_id: &str) -> bool {
        let mut arms = self
            .arms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(live) = arms.get_mut(channel_id) else {
            return true;
        };
        live.retain(|arm| arm.end_proof().is_none());
        let ended = live.is_empty();
        if ended {
            arms.remove(channel_id);
        }
        ended
    }

    /// The channel's entry without creating one; `None` means nothing here has attached it.
    pub(crate) fn entry(&self, channel_id: &str) -> Option<IndexState> {
        let states = self.states();
        states.get(channel_id).map(|(_, state)| state.clone())
    }

    /// Mirrors the row a commit left at `revision`; a mirror of an earlier commit that arrives
    /// late never replaces a later one.
    fn set(&self, channel_id: &str, revision: i64, state: IndexState) {
        let mut states = self.states();
        if states
            .get(channel_id)
            .is_none_or(|(known, _)| *known <= revision)
        {
            states.insert(channel_id.to_string(), (revision, state));
        }
    }

    /// Registers a row found in PG as undecided, unless a commit of this process mirrored it.
    pub(super) fn attach(&self, channel_id: &str, revision: i64) {
        let mut states = self.states();
        let entry = states.entry(channel_id.to_string());
        entry.or_insert((revision, IndexState::Blocked));
    }

    pub(super) fn block(&self, channel_id: &str, revision: i64) {
        self.set(channel_id, revision, IndexState::Blocked);
    }

    /// Ends one arm's entry; an entry another arm published since is left as it is.
    pub(super) fn block_arm(&self, channel_id: &str, arm_id: &str) {
        if let Some((_, state)) = self.states().get_mut(channel_id)
            && matches!(state, IndexState::ExactOperation { arm_id: current, .. } if current == arm_id)
        {
            *state = IndexState::Blocked;
        }
    }

    pub(super) fn publish(&self, arm: CommittedArm<'_>) {
        let state = IndexState::ExactOperation {
            operation_id: arm.operation_id.to_string(),
            stage: arm.stage,
            arm_id: arm.arm_id.to_string(),
        };
        self.set(arm.channel_id, arm.revision, state);
    }

    /// A sync writer's entry: only with an unrevoked capability of the published arm (copied
    /// identifiers are not one). The apply is registered before the index lock is released.
    pub(crate) fn admit_write(
        &self,
        channel_id: &str,
        capability: Option<&ApplyCapability>,
    ) -> Result<ApplyGuard, Denial> {
        let states = self.states();
        let state = states.get(channel_id).map(|(_, state)| state);
        if let Some(IndexState::ExactOperation {
            operation_id,
            stage,
            arm_id,
        }) = state
            && let Some(capability) = capability.filter(|cap| cap.arm_id() == arm_id)
            && let Ok(entered) = capability.enter(channel_id, operation_id, *stage)
        {
            return Ok(entered);
        }
        Err(Denial(state.cloned().unwrap_or(IndexState::Unknown)))
    }

    /// An applier's entry with a carried capability: the carrier leaves the queue as its apply is
    /// counted, and a refusal hands it back still queued with its payload.
    pub(crate) fn admit_carried<P>(
        &self,
        channel_id: &str,
        mut carrier: Carrier<P>,
    ) -> Result<(ApplyGuard, P), Unapplied<P>> {
        let entered = {
            let states = self.states();
            let state = states.get(channel_id).map(|(_, state)| state);
            let entered = match state {
                Some(IndexState::ExactOperation {
                    operation_id,
                    stage,
                    arm_id,
                }) => carrier.enter(channel_id, (operation_id, *stage, arm_id)),
                _ => None,
            };
            entered.ok_or_else(|| Denial(state.cloned().unwrap_or(IndexState::Unknown)))
        };
        match entered {
            Ok(entered) => {
                #[cfg(test)]
                seam::hit(channel_id, Point::Active);
                Ok(entered)
            }
            Err(denial) => Err(carrier.refuse(Unsent::Denied(denial))),
        }
    }
}
