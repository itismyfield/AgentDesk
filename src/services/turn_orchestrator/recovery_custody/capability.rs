//! A limited right to apply one armed stage of one operation. Only the custody core issues it, after
//! the stage's PG arm committed; nothing converts an input permit, PID or reference into one.

use std::sync::{Arc, Mutex, MutexGuard};

use super::operation::{ActiveOperation, Stage, StageStatus};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CapabilityRefusal {
    Revoked,
    WrongTarget,
}

/// One arm's lifetime: closed to new applies, applies entered and still running, and whether its
/// capability still exists in a worker or a queue.
#[derive(Debug)]
struct Gate {
    closed: bool,
    active: usize,
    held: bool,
}

#[derive(Debug)]
struct ArmLife {
    channel_id: String,
    operation_id: String,
    stage: Stage,
    arm_id: String,
    gate: Mutex<Gate>,
}

impl ArmLife {
    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Not `Clone`: one applier holds it, and the core's [`RevokeHandle`] can end it.
#[derive(Debug)]
pub(crate) struct ApplyCapability(Arc<ArmLife>);

#[derive(Clone, Debug)]
pub(crate) struct RevokeHandle(Arc<ArmLife>);

/// One entered apply of one arm, counted until dropped; a revoke after entry does not end it.
#[derive(Debug)]
pub(crate) struct ApplyGuard(Arc<ArmLife>);

/// Proof that one arm of one operation can apply nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndProof {
    operation_id: String,
    arm_id: Option<String>,
}

impl EndProof {
    /// For a row whose owning process is gone; the caller answers for that process's end.
    pub(in super::super) fn of_lost_process(op: &ActiveOperation) -> Self {
        Self {
            operation_id: op.operation_id.clone(),
            arm_id: op.arm_id.clone(),
        }
    }

    /// An operation that never armed, or settled before arms had an identity, has no applier left.
    pub(crate) fn of_unarmed(op: &ActiveOperation) -> Option<Self> {
        let unsettled = matches!(
            op.status,
            StageStatus::ApplyArmed | StageStatus::Indeterminate
        );
        (op.arm_id.is_none() && !unsettled).then(|| Self::of_lost_process(op))
    }

    /// Whether it is about `op`'s current arm; an earlier arm's proof says nothing about it.
    pub(crate) fn names(&self, op: &ActiveOperation) -> bool {
        self.operation_id == op.operation_id && self.arm_id == op.arm_id
    }
}

impl RevokeHandle {
    pub(crate) fn arm_id(&self) -> &str {
        &self.0.arm_id
    }

    /// Closes the arm to new applies; one already entered keeps running and stays counted.
    pub(crate) fn revoke(&self) {
        self.0.gate().closed = true;
    }

    /// The arm's end proof: only once revoked, with no entered apply left and its capability gone
    /// from every worker and queue.
    pub(crate) fn end_proof(&self) -> Option<EndProof> {
        let gate = self.0.gate();
        (gate.closed && gate.active == 0 && !gate.held).then(|| EndProof {
            operation_id: self.0.operation_id.clone(),
            arm_id: Some(self.0.arm_id.clone()),
        })
    }
}

impl ApplyCapability {
    pub(super) fn issue(
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
        arm_id: &str,
    ) -> (Self, RevokeHandle) {
        let life = Arc::new(ArmLife {
            channel_id: channel_id.to_string(),
            operation_id: operation_id.to_string(),
            stage,
            arm_id: arm_id.to_string(),
            gate: Mutex::new(Gate {
                closed: false,
                active: 0,
                held: true,
            }),
        });
        (Self(life.clone()), RevokeHandle(life))
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.0.operation_id
    }

    pub(crate) fn stage(&self) -> Stage {
        self.0.stage
    }

    /// The arm that issued it; another arm of the same operation and stage is another target.
    pub(crate) fn arm_id(&self) -> &str {
        &self.0.arm_id
    }

    fn admits(
        &self,
        gate: &Gate,
        (channel_id, operation_id, stage): (&str, &str, Stage),
    ) -> Result<(), CapabilityRefusal> {
        if gate.closed {
            return Err(CapabilityRefusal::Revoked);
        }
        let life = &self.0;
        if (channel_id, operation_id, stage)
            != (
                life.channel_id.as_str(),
                life.operation_id.as_str(),
                life.stage,
            )
        {
            return Err(CapabilityRefusal::WrongTarget);
        }
        Ok(())
    }

    /// An instant read for its own channel, operation and stage; it holds nothing open, so a writer
    /// enters through the index instead.
    pub(crate) fn check(
        &self,
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
    ) -> Result<(), CapabilityRefusal> {
        self.admits(&self.0.gate(), (channel_id, operation_id, stage))
    }

    /// Enters one apply in a short critical section; the arm cannot end while the guard lives.
    pub(super) fn enter(
        &self,
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
    ) -> Result<ApplyGuard, CapabilityRefusal> {
        let mut gate = self.0.gate();
        self.admits(&gate, (channel_id, operation_id, stage))?;
        gate.active += 1;
        Ok(ApplyGuard(self.0.clone()))
    }
}

impl Drop for ApplyCapability {
    fn drop(&mut self) {
        self.0.gate().held = false;
    }
}

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        self.0.gate().active -= 1;
    }
}
