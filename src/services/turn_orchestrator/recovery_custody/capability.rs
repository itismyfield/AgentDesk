//! A limited right to apply one armed stage of one operation. Only the custody core issues it, after
//! the stage's PG arm committed; nothing converts an input permit, PID or reference into one.

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;

use super::denial_index::Denial;
use super::model::Namespace;
use super::operation::{ActiveOperation, Stage, StageStatus};
#[cfg(test)]
use super::seam::{self, Point};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CapabilityRefusal {
    Revoked,
    WrongTarget,
}

/// The registry attempt an arm was issued to: its process and its never-reused attempt id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptKey {
    pub(crate) process_incarnation: String,
    pub(crate) attempt: u64,
}

/// One arm's lifetime: closed to new applies, applies entered and still running, carriers sent
/// and not yet admitted, and whether its capability is still held outside a carrier.
#[derive(Debug)]
struct Gate {
    closed: bool,
    active: usize,
    queued: usize,
    held: bool,
}

#[derive(Debug)]
struct ArmLife {
    channel_id: String,
    operation_id: String,
    stage: Stage,
    arm_id: String,
    /// The namespace of the row the arm committed on.
    namespace: Namespace,
    attempt: Option<AttemptKey>,
    gate: Mutex<Gate>,
}

impl ArmLife {
    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn admits(
        &self,
        gate: &Gate,
        (channel_id, operation_id, stage): (&str, &str, Stage),
    ) -> Result<(), CapabilityRefusal> {
        if gate.closed {
            return Err(CapabilityRefusal::Revoked);
        }
        if (channel_id, operation_id, stage)
            != (
                self.channel_id.as_str(),
                self.operation_id.as_str(),
                self.stage,
            )
        {
            return Err(CapabilityRefusal::WrongTarget);
        }
        Ok(())
    }

    /// Enters one apply; a carried one leaves the queue in the same critical section.
    fn enter(
        self: &Arc<Self>,
        target: (&str, &str, Stage),
        from_queue: bool,
    ) -> Result<ApplyGuard, CapabilityRefusal> {
        let mut gate = self.gate();
        self.admits(&gate, target)?;
        gate.active += 1;
        if from_queue {
            gate.queued -= 1;
        }
        Ok(ApplyGuard(self.clone()))
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

/// An arm's live count: its capability held by a worker, carriers queued, applies entered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Load {
    pub(crate) held: bool,
    pub(crate) queued: usize,
    pub(crate) active: usize,
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

    pub(crate) fn channel_id(&self) -> &str {
        &self.0.channel_id
    }

    pub(crate) fn namespace(&self) -> &Namespace {
        &self.0.namespace
    }

    /// The registry attempt it was issued to; `None` for an arm made outside any attempt.
    pub(crate) fn attempt(&self) -> Option<&AttemptKey> {
        self.0.attempt.as_ref()
    }

    pub(crate) fn load(&self) -> Load {
        let gate = self.0.gate();
        Load {
            held: gate.held,
            queued: gate.queued,
            active: gate.active,
        }
    }

    /// Closes the arm to new applies; one already entered keeps running and stays counted.
    pub(crate) fn revoke(&self) {
        self.0.gate().closed = true;
    }

    /// The arm's end proof: only once revoked, with no entered apply left and its capability gone
    /// from every worker and queue.
    pub(crate) fn end_proof(&self) -> Option<EndProof> {
        let gate = self.0.gate();
        let idle = gate.active == 0 && gate.queued == 0 && !gate.held;
        (gate.closed && idle).then(|| EndProof {
            operation_id: self.0.operation_id.clone(),
            arm_id: Some(self.0.arm_id.clone()),
        })
    }
}

impl ApplyCapability {
    pub(super) fn issue(
        (channel_id, operation_id, stage, arm_id): (&str, &str, Stage, &str),
        namespace: &Namespace,
        attempt: Option<AttemptKey>,
    ) -> (Self, RevokeHandle) {
        let life = Arc::new(ArmLife {
            channel_id: channel_id.to_string(),
            operation_id: operation_id.to_string(),
            stage,
            arm_id: arm_id.to_string(),
            namespace: namespace.clone(),
            attempt,
            gate: Mutex::new(Gate {
                closed: false,
                active: 0,
                queued: 0,
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

    /// An instant read for its own channel, operation and stage; it holds nothing open, so a writer
    /// enters through the index instead.
    pub(crate) fn check(
        &self,
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
    ) -> Result<(), CapabilityRefusal> {
        self.0
            .admits(&self.0.gate(), (channel_id, operation_id, stage))
    }

    /// Enters one apply in a short critical section; the arm cannot end while the guard lives.
    pub(super) fn enter(
        &self,
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
    ) -> Result<ApplyGuard, CapabilityRefusal> {
        self.0.enter((channel_id, operation_id, stage), false)
    }

    /// Counts the apply as queued before anything is sent; the worker's hold passes to the carrier
    /// in the same critical section, so the arm never looks idle on the way.
    pub(crate) fn carry<P>(self, payload: P) -> Carrier<P> {
        let life = self.0.clone();
        {
            let mut gate = life.gate();
            gate.queued += 1;
            gate.held = false;
        }
        drop(self);
        #[cfg(test)]
        seam::hit(&life.channel_id, Point::Queued);
        Carrier {
            life,
            payload: Some(payload),
            queued: true,
        }
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

/// Why a carrier came back unused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unsent {
    /// The receiving mailbox was gone at the send.
    SendFailed,
    /// The applier received it but is closed to new work.
    Closed,
    /// The index or the capability refused its entry.
    Denied(Denial),
}

/// A capability on its way to its applier with the payload it applies. It stays queued until it
/// is admitted or dropped; a refusal hands back the same carrier, so a retry needs no new grant.
#[derive(Debug)]
pub(crate) struct Carrier<P> {
    life: Arc<ArmLife>,
    payload: Option<P>,
    queued: bool,
}

/// A carrier handed back unused, with the reason.
#[derive(Debug)]
pub(crate) struct Unapplied<P> {
    pub(crate) refusal: Unsent,
    pub(crate) carrier: Carrier<P>,
}

impl<P> Carrier<P> {
    pub(crate) fn arm_id(&self) -> &str {
        &self.life.arm_id
    }

    /// `None` only once an admission took it.
    pub(crate) fn payload(&self) -> Option<&P> {
        self.payload.as_ref()
    }

    /// Gives up the apply: the payload returns to the caller and the arm stops counting it.
    pub(crate) fn into_payload(mut self) -> Option<P> {
        self.payload.take()
    }

    pub(crate) fn refuse(self, refusal: Unsent) -> Unapplied<P> {
        Unapplied {
            refusal,
            carrier: self,
        }
    }

    pub(crate) fn send(self, mailbox: &mpsc::UnboundedSender<Self>) -> Result<(), Unapplied<P>> {
        mailbox
            .send(self)
            .map_err(|unsent| unsent.0.refuse(Unsent::SendFailed))
    }

    /// The published target's entry; the queued count becomes the active one under the gate.
    pub(super) fn enter(
        &mut self,
        channel_id: &str,
        (operation_id, stage, arm_id): (&str, Stage, &str),
    ) -> Option<(ApplyGuard, P)> {
        if arm_id != self.life.arm_id || !self.queued {
            return None;
        }
        let guard = self.life.enter((channel_id, operation_id, stage), true);
        let guard = guard.ok()?;
        self.queued = false;
        let payload = self.payload.take()?;
        Some((guard, payload))
    }
}

impl<P> Drop for Carrier<P> {
    fn drop(&mut self) {
        if self.queued {
            self.life.gate().queued -= 1;
        }
    }
}

/// An apply its holder already entered, lent to a nested writer: it neither extends nor ends it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LentApply<'a>(&'a ArmLife);

impl ApplyGuard {
    /// Lends the apply to a writer of `channel_id` in `namespace`; any other target is refused.
    pub(crate) fn lend(
        &self,
        channel_id: &str,
        namespace: &Namespace,
    ) -> Result<LentApply<'_>, CapabilityRefusal> {
        let life = &*self.0;
        let target = life.channel_id == channel_id && life.namespace == *namespace;
        target
            .then_some(LentApply(life))
            .ok_or(CapabilityRefusal::WrongTarget)
    }
}

impl LentApply<'_> {
    pub(crate) fn operation_id(&self) -> &str {
        &self.0.operation_id
    }

    pub(crate) fn stage(&self) -> Stage {
        self.0.stage
    }

    pub(crate) fn arm_id(&self) -> &str {
        &self.0.arm_id
    }

    /// Runs one effect of the lent apply.
    pub(crate) fn effect<T>(&self, apply: impl FnOnce() -> T) -> T {
        #[cfg(test)]
        seam::hit(&self.0.channel_id, Point::BeforeEffect);
        let applied = apply();
        #[cfg(test)]
        seam::effect(&self.0.channel_id);
        applied
    }
}
