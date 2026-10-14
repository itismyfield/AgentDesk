//! One channel's recovery slot: its current attempt, that attempt's workers and the lifetime of
//! every arm it issued. A retry or a successor starts only after all of them ended.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::super::recovery_custody::capability::{EndProof, RevokeHandle};
use super::super::recovery_custody::model::{CustodyRecord, Inspection, Phase};
use super::super::recovery_custody::operation::StageStatus;
use super::super::recovery_custody::reconcile::{LiveOwner, PriorExecution};
use super::super::recovery_custody::{Armed, CustodyStore};
use super::witness::{WitnessCell, WitnessReader, WitnessWriter};
use super::{RegistryRefusal, Shared};
use crate::services::cluster::channel_home_port::scoped_restore::{AttemptIdentity, RestoreScope};

struct Attempt {
    identity: AttemptIdentity,
    /// Open to new workers and arms until off, a scope change or its own end closes it.
    open: bool,
    workers: usize,
    /// Its latest arm and every earlier one that has not drained, each under its own arm id.
    arms: HashMap<String, RevokeHandle>,
}

impl Attempt {
    fn quiescent(&self) -> Result<(), RegistryRefusal> {
        let arms = self.arms.values();
        let arms = arms.filter(|arm| arm.end_proof().is_none()).count();
        if self.open || self.workers > 0 || arms > 0 {
            return Err(RegistryRefusal::NotQuiescent {
                open: self.open,
                workers: self.workers,
                arms,
            });
        }
        Ok(())
    }
}

#[derive(Default)]
struct SlotState {
    attempt: Option<Attempt>,
    retired: bool,
}

/// Its identity is never reused: a slot resolved after a reclaim is another slot.
pub(crate) struct Slot {
    id: u64,
    channel: u64,
    shared: Arc<Shared>,
    state: Mutex<SlotState>,
    witness: Arc<WitnessCell>,
}

pub(crate) enum Begun {
    Started(AttemptHandle),
    /// The same lane is already answered for: the same attempt's read handle, no second attempt.
    Joined(WitnessReader),
}

/// A custody row this process may build a new attempt on: nothing armed, indeterminate or held.
fn decided(inspection: &Inspection) -> bool {
    match inspection {
        Inspection::Absent => true,
        Inspection::Row(row) => {
            let unsettled =
                |status| matches!(status, StageStatus::ApplyArmed | StageStatus::Indeterminate);
            let op = row.active_operation.as_ref();
            row.phase != Phase::Held && !op.is_some_and(|op| unsettled(op.status))
        }
        Inspection::Unsupported { .. } => false,
    }
}

impl Slot {
    pub(super) fn new(channel: u64, shared: Arc<Shared>) -> Arc<Self> {
        Arc::new(Self {
            id: shared.next_id(),
            channel,
            shared,
            state: Mutex::default(),
            witness: Arc::default(),
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    #[cfg(test)]
    pub(super) fn unlocked(&self) -> bool {
        self.state.try_lock().is_ok()
    }

    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn start(
        self: &Arc<Self>,
        state: &mut SlotState,
        scope: RestoreScope,
        generation: u64,
    ) -> Result<AttemptHandle, RegistryRefusal> {
        if state.retired {
            return Err(RegistryRefusal::Retired);
        }
        if scope.channel != self.channel || generation == 0 {
            return Err(RegistryRefusal::Foreign);
        }
        let identity = AttemptIdentity {
            scope,
            generation,
            process_incarnation: self.shared.process_incarnation.clone(),
            attempt: self.shared.next_id(),
        };
        state.attempt = Some(Attempt {
            identity: identity.clone(),
            open: true,
            workers: 0,
            arms: HashMap::new(),
        });
        Ok(AttemptHandle {
            slot: self.clone(),
            witness: self.witness.open(identity.clone()),
            identity,
        })
    }

    /// Starts the slot's first attempt, or joins the attempt already answering for the same lane.
    /// Another lane waits for [`Slot::retry`].
    pub(crate) fn begin(
        self: &Arc<Self>,
        scope: RestoreScope,
        generation: u64,
    ) -> Result<Begun, RegistryRefusal> {
        let mut state = self.state();
        if state.retired {
            return Err(RegistryRefusal::Retired);
        }
        let lane = (&scope, generation);
        match state.attempt.as_ref().map(|attempt| &attempt.identity) {
            Some(current) if (&current.scope, current.generation) == lane => {
                Ok(Begun::Joined(self.witness.reader(current.clone())))
            }
            Some(_) => Err(RegistryRefusal::Foreign),
            None => self
                .start(&mut state, scope, generation)
                .map(Begun::Started),
        }
    }

    /// Off or a scope change: no new worker or arm, and every arm is revoked. Applies already
    /// entered finish and stay counted.
    pub(crate) fn close_admission(&self) {
        if let Some(attempt) = self.state().attempt.as_mut() {
            attempt.open = false;
            attempt.arms.values().for_each(RevokeHandle::revoke);
        }
    }

    /// The current attempt, once it is closed with no worker and no arm left to drain.
    pub(super) fn drained(&self) -> Result<Option<u64>, RegistryRefusal> {
        self.still_drained(&self.state(), None)
    }

    fn still_drained(
        &self,
        state: &SlotState,
        seen: Option<Option<u64>>,
    ) -> Result<Option<u64>, RegistryRefusal> {
        if state.retired {
            return Err(RegistryRefusal::Retired);
        }
        let attempt = state.attempt.as_ref();
        let current = attempt.map(|attempt| attempt.identity.attempt);
        if seen.is_some_and(|seen| seen != current) {
            return Err(RegistryRefusal::Foreign);
        }
        attempt.map_or(Ok(()), Attempt::quiescent)?;
        Ok(current)
    }

    /// Replaces an ended attempt with the next one. Nothing is written and nothing new runs while
    /// the earlier attempt has a worker or an undrained arm, or its operation is undecided.
    pub(crate) async fn retry(
        self: &Arc<Self>,
        custody: &CustodyStore,
        scope: RestoreScope,
        generation: u64,
    ) -> Result<AttemptHandle, RegistryRefusal> {
        let ended = self.drained()?;
        let inspection = custody.inspect(&self.channel.to_string()).await;
        if !inspection.is_ok_and(|inspection| decided(&inspection)) {
            return Err(RegistryRefusal::Undecided);
        }
        let mut state = self.state();
        self.still_drained(&state, Some(ended))?;
        self.start(&mut state, scope, generation)
    }

    /// Ends the slot for a reclaim; its witness is revoked with it.
    pub(super) fn retire(&self, ended: Option<u64>) -> Result<(), RegistryRefusal> {
        let mut state = self.state();
        self.still_drained(&state, Some(ended))?;
        state.retired = true;
        self.witness.revoke();
        Ok(())
    }

    /// What this process can vouch for about the earlier execution of `record`'s operation: the
    /// end proof of the row's own arm, from an ended attempt or from the same one moving on.
    pub(crate) fn prior(&self, record: &CustodyRecord) -> PriorExecution {
        let state = self.state();
        let attempt = state.attempt.as_ref().filter(|_| !state.retired);
        let (Some(op), Some(attempt)) = (&record.active_operation, attempt) else {
            return PriorExecution::Unknown;
        };
        let proof = match &op.arm_id {
            Some(arm) => attempt.arms.get(arm).and_then(RevokeHandle::end_proof),
            None => EndProof::of_unarmed(op),
        };
        if attempt.open {
            return PriorExecution::SameOwnerLive(LiveOwner {
                process_incarnation: attempt.identity.process_incarnation.clone(),
                operation_id: op.operation_id.clone(),
                applier_ended: proof,
            });
        }
        match (attempt.quiescent(), proof) {
            (Ok(()), Some(proof)) => PriorExecution::Ended(proof),
            _ => PriorExecution::Unknown,
        }
    }
}

/// The registry-owned side of one attempt; its caller leaving does not end the attempt.
pub(crate) struct AttemptHandle {
    slot: Arc<Slot>,
    identity: AttemptIdentity,
    witness: WitnessWriter,
}

impl AttemptHandle {
    pub(crate) fn identity(&self) -> &AttemptIdentity {
        &self.identity
    }

    pub(crate) fn witness(&self) -> &WitnessWriter {
        &self.witness
    }

    /// Takes one of the process's install worker places; the attempt has not ended while the
    /// permit lives.
    pub(crate) fn worker(&self) -> Result<WorkerPermit, RegistryRefusal> {
        if !self.slot.shared.take_worker() {
            return Err(RegistryRefusal::WorkersBusy);
        }
        let mut permit = WorkerPermit {
            slot: self.slot.clone(),
            attempt: self.identity.attempt,
            counted: false,
        };
        let mut state = self.slot.state();
        let attempt = state.attempt.as_mut();
        let attempt = attempt.filter(|attempt| attempt.identity == self.identity);
        let attempt = attempt.ok_or(RegistryRefusal::Foreign)?;
        if !attempt.open {
            return Err(RegistryRefusal::AdmissionClosed);
        }
        attempt.workers += 1;
        permit.counted = true;
        drop(state);
        Ok(permit)
    }

    /// Keeps a just-issued arm's lifetime with the attempt, under its own arm id. An attempt
    /// closed meanwhile still counts the arm and revokes it at once.
    pub(crate) fn adopt(&self, armed: &Armed) -> Result<(), RegistryRefusal> {
        let mut state = self.slot.state();
        let attempt = state.attempt.as_mut();
        let Some(attempt) = attempt.filter(|attempt| attempt.identity == self.identity) else {
            armed.revoke.revoke();
            return Err(RegistryRefusal::Foreign);
        };
        attempt.arms.retain(|_, arm| arm.end_proof().is_none());
        let arm = armed.revoke.clone();
        attempt.arms.insert(arm.arm_id().to_string(), arm);
        if !attempt.open {
            armed.revoke.revoke();
            return Err(RegistryRefusal::AdmissionClosed);
        }
        Ok(())
    }
}

/// One running install worker of an attempt.
pub(crate) struct WorkerPermit {
    slot: Arc<Slot>,
    attempt: u64,
    counted: bool,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        if self.counted
            && let Some(attempt) = self.slot.state().attempt.as_mut()
            && attempt.identity.attempt == self.attempt
        {
            attempt.workers -= 1;
        }
        self.slot.shared.release_worker();
    }
}
