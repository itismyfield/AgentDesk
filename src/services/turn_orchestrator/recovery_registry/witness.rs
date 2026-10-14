//! A recovery slot's witness, bound to one exact attempt: scope, generation, process incarnation
//! and attempt. An earlier attempt's handles, alive or dropped late, neither read nor change it.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::services::cluster::channel_home_port::scoped_restore::{
    AttemptIdentity, ExactWitness, InflightObservation, RestoreStatus, RestoreWitness,
};

/// The slot's current attempt and its witness; empty before the first attempt and once revoked.
#[derive(Default)]
pub(super) struct WitnessCell(Mutex<Option<(AttemptIdentity, RestoreWitness)>>);

impl WitnessCell {
    fn current(&self) -> MutexGuard<'_, Option<(AttemptIdentity, RestoreWitness)>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Makes `identity` the current attempt, restoring from the start, and returns its one writer.
    pub(super) fn open(self: &Arc<Self>, identity: AttemptIdentity) -> WitnessWriter {
        let witness = RestoreWitness {
            scope: Some(identity.scope.clone()),
            generation: identity.generation,
            status: RestoreStatus::Restoring,
            inflight: None,
        };
        *self.current() = Some((identity.clone(), witness));
        WitnessWriter(self.reader(identity))
    }

    pub(super) fn reader(self: &Arc<Self>, identity: AttemptIdentity) -> WitnessReader {
        WitnessReader {
            identity,
            cell: self.clone(),
        }
    }

    /// Ends the current attempt's witness; every handle on it reads nothing from then on.
    pub(super) fn revoke(&self) {
        *self.current() = None;
    }
}

/// The attempt was replaced or revoked; nothing was read or written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Superseded;

/// The read half. A handle of an earlier attempt stays alive but is not the current authority.
#[derive(Clone)]
pub(crate) struct WitnessReader {
    identity: AttemptIdentity,
    cell: Arc<WitnessCell>,
}

impl ExactWitness for WitnessReader {
    fn identity(&self) -> &AttemptIdentity {
        &self.identity
    }

    fn read(&self) -> Option<RestoreWitness> {
        let current = self.cell.current();
        let (identity, witness) = current.as_ref()?;
        (*identity == self.identity).then(|| witness.clone())
    }
}

/// The attempt's private write half; only the registry makes one, with the attempt.
pub(crate) struct WitnessWriter(WitnessReader);

impl WitnessWriter {
    pub(crate) fn reader(&self) -> WitnessReader {
        self.0.clone()
    }

    fn update(&self, change: impl FnOnce(&mut RestoreWitness)) -> Result<(), Superseded> {
        match self.0.cell.current().as_mut() {
            Some((identity, witness)) if *identity == self.0.identity => {
                change(witness);
                Ok(())
            }
            _ => Err(Superseded),
        }
    }

    /// Records a stage result of this exact attempt.
    pub(crate) fn record(&self, status: RestoreStatus) -> Result<(), Superseded> {
        self.update(|witness| witness.status = status)
    }

    pub(crate) fn observe(&self, inflight: InflightObservation) -> Result<(), Superseded> {
        self.update(|witness| witness.inflight = Some(inflight))
    }
}

impl Drop for WitnessWriter {
    /// A writer leaving its own attempt mid-restore blocks that attempt; it settles nothing else.
    fn drop(&mut self) {
        let _ = self.update(|witness| {
            if witness.status == RestoreStatus::Restoring {
                witness.status =
                    RestoreStatus::Blocked("restore task left installation incomplete".into());
            }
        });
    }
}
