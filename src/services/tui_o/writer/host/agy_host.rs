//! Shadow custody for the process backend's committed spools.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::STORE_DIR_NAME;
use crate::services::tui_o::store::agy_spool::replay::{
    AgyFrontier, AgyOutcome, AgyReplay, AgySlot, AgyTarget, AgyTurnKey, SlotName, checked_dir,
    child_retired, classify_slot, mark_child_retired, replay, version_dir,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AgyReadPermit {
    pub(crate) target: AgyTarget,
    pub(crate) generation: NonZeroU64,
    incarnation: uuid::Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgyBodyOwner {
    Legacy,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Booting,
    Adopted,
    Retired,
}

/// Why a destination is held; the slot names the exact turn directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgyHoldReason {
    /// The slot exists without a committed prefix: the producer stopped right after `mkdir`.
    NoCheckpoint,
    /// The producer committed `open` but neither Done nor a failure.
    Pending(AgyTurnKey),
    Corrupt(String),
    Duplicate,
    Malformed,
    /// A slot pinned by an earlier read disappeared.
    Frontier,
    Evidence(AgyEvidenceHold),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgyEvidenceHold {
    /// Only the PG row: it proves the original and destination, never the execution's nonce.
    PgOnly,
    Conflict,
    Unverifiable,
    Missing,
}

/// The execution an iMessage slot must name, or why it cannot be adopted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgyEvidence {
    Bound(AgyTurnKey),
    Held(AgyEvidenceHold),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgyHeldSlot {
    pub(crate) name: String,
    pub(crate) reason: AgyHoldReason,
}

/// Every slot of one destination: one held slot never hides the others.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AgyCustody {
    pub(crate) turns: Vec<AgyReplay>,
    pub(crate) held: Vec<AgyHeldSlot>,
}

impl AgyCustody {
    /// The frontier each replayed slot reached, for the next read to prove again.
    pub(crate) fn frontiers(&self) -> BTreeMap<AgySlot, AgyFrontier> {
        self.turns
            .iter()
            .map(|turn| (turn.slot.clone(), turn.frontier.clone()))
            .collect()
    }

    fn hold(&mut self, slot: &AgySlot, reason: AgyHoldReason) {
        self.held.push(AgyHeldSlot {
            name: slot.dir_name(),
            reason,
        });
    }
}

#[derive(Debug)]
pub(crate) struct AgyAdoption {
    pub(crate) permit: Option<AgyReadPermit>,
    pub(crate) custody: AgyCustody,
}

pub(crate) struct AgyShadowHost {
    root: PathBuf,
    gate: Arc<OwnershipGate>,
    permit: AgyReadPermit,
    phase: Mutex<Phase>,
}

impl AgyShadowHost {
    pub(crate) fn start(
        root: PathBuf,
        target: AgyTarget,
        gate: Arc<OwnershipGate>,
    ) -> io::Result<Option<Self>> {
        let Some(generation) = gate.current().epoch().and_then(NonZeroU64::new) else {
            return Ok(None);
        };
        let root = checked_dir(root)?;
        let phase = if child_retired(&root, target)? {
            Phase::Retired
        } else {
            Phase::Booting
        };
        Ok(Some(Self {
            root,
            gate,
            permit: AgyReadPermit {
                target,
                generation,
                incarnation: uuid::Uuid::new_v4(),
            },
            phase: Mutex::new(phase),
        }))
    }

    /// Restore all known turns. A held slot other than a pending turn refuses the destination.
    pub(crate) fn restore(
        &self,
        previous: &BTreeMap<AgySlot, AgyFrontier>,
    ) -> io::Result<Vec<AgyReplay>> {
        let custody = self.custody(previous)?;
        if let Some(held) = custody
            .held
            .iter()
            .find(|held| !matches!(held.reason, AgyHoldReason::Pending(_)))
        {
            return Err(io::Error::other(format!(
                "agy spool slot {} is held: {:?}",
                held.name, held.reason
            )));
        }
        Ok(custody.turns)
    }

    /// Every slot with its exact hold reason, including those with a saved frontier that vanished.
    pub(crate) fn custody(
        &self,
        previous: &BTreeMap<AgySlot, AgyFrontier>,
    ) -> io::Result<AgyCustody> {
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase == Phase::Retired || self.current().is_none() {
            return Err(io::Error::other("agy shadow reader lost custody"));
        }
        self.replay_all(previous)
    }

    /// Replay-only custody of an existing spool, iMessage evidence applied. Never a permit.
    pub(crate) fn recover(
        &self,
        previous: &BTreeMap<AgySlot, AgyFrontier>,
        evidence: &BTreeMap<AgySlot, AgyEvidence>,
    ) -> io::Result<AgyCustody> {
        let mut custody = self.custody(previous)?;
        hold_unproven(&mut custody, evidence);
        Ok(custody)
    }

    fn replay_all(&self, previous: &BTreeMap<AgySlot, AgyFrontier>) -> io::Result<AgyCustody> {
        checked_dir(self.root.clone())?;
        if child_retired(&self.root, self.permit.target)? {
            return Err(io::Error::other("agy shadow reader lost custody"));
        }
        let AgyTarget { destination, .. } = self.permit.target;
        let mut custody = AgyCustody::default();
        let slots = match version_dir(&self.root, destination) {
            Ok(dir) => slot_children(&dir, &mut custody)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        for slot in previous.keys() {
            if !slots.contains(slot) {
                custody.hold(slot, AgyHoldReason::Frontier);
            }
        }
        let mut named = BTreeMap::new();
        for slot in slots {
            match replay(&self.root, self.permit.target, &slot, previous.get(&slot)) {
                Ok(turn) => {
                    if let Some(other) = named.insert(turn.key.id, slot.clone()) {
                        custody.hold(&other, AgyHoldReason::Duplicate);
                        custody.hold(&slot, AgyHoldReason::Duplicate);
                    }
                    if turn.outcome == AgyOutcome::Pending {
                        custody.hold(&slot, AgyHoldReason::Pending(turn.key.clone()));
                    }
                    custody.turns.push(turn);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    custody.hold(&slot, AgyHoldReason::NoCheckpoint);
                }
                Err(error) => custody.hold(&slot, AgyHoldReason::Corrupt(error.to_string())),
            }
        }
        if self.current().is_none() {
            return Err(io::Error::other(
                "agy shadow reader lost custody during replay",
            ));
        }
        Ok(custody)
    }

    /// Ready after source validation, iMessage evidence and confirmation that prior Legacy work is
    /// settled. Any held slot, a pending one included, keeps the whole destination unadopted.
    pub(crate) fn adopt(
        &self,
        previous: &BTreeMap<AgySlot, AgyFrontier>,
        prior_legacy_settled: bool,
        evidence: &BTreeMap<AgySlot, AgyEvidence>,
    ) -> io::Result<AgyAdoption> {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase == Phase::Retired || self.current().is_none() {
            return Err(io::Error::other("agy shadow reader lost custody"));
        }
        *phase = Phase::Booting;
        let mut custody = self.replay_all(previous)?;
        hold_unproven(&mut custody, evidence);
        let mut permit = None;
        if prior_legacy_settled && custody.held.is_empty() {
            permit = self
                .gate
                .admit(|epoch| {
                    if epoch == self.permit.generation.get() {
                        *phase = Phase::Adopted;
                        Some(self.permit)
                    } else {
                        None
                    }
                })
                .flatten();
        }
        Ok(AgyAdoption { permit, custody })
    }

    pub(crate) fn body_owner(&self) -> Option<AgyBodyOwner> {
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase == Phase::Retired {
            return Some(AgyBodyOwner::Retired);
        }
        self.current().map(|_| AgyBodyOwner::Legacy)
    }

    /// Called after an exact row delete and a fresh successor check under the caller's input fence.
    /// Disposition must prove each whole turn settled; errors and held slots keep it active.
    pub(crate) fn retire_child(
        &self,
        permit: AgyReadPermit,
        previous: &BTreeMap<AgySlot, AgyFrontier>,
        settled: impl Fn(&AgyReplay) -> bool,
    ) -> io::Result<bool> {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase != Phase::Adopted
            || self.permit != permit
            || permit.target.destination == permit.target.logical_channel
            || self.current().is_none()
        {
            return Ok(false);
        }
        let custody = self.replay_all(previous)?;
        if !custody.held.is_empty() || !custody.turns.iter().all(|turn| settled(turn)) {
            return Ok(false);
        }
        self.gate
            .admit(|epoch| {
                if epoch != permit.generation.get() {
                    return Ok(false);
                }
                mark_child_retired(&self.root, permit.target)?;
                *phase = Phase::Retired;
                Ok(true)
            })
            .unwrap_or(Ok(false))
    }

    fn current(&self) -> Option<()> {
        (self.gate.current().epoch() == Some(self.permit.generation.get())).then_some(())
    }
}

/// An iMessage slot is adopted only when durable evidence names its exact execution.
fn hold_unproven(custody: &mut AgyCustody, evidence: &BTreeMap<AgySlot, AgyEvidence>) {
    let unproven: Vec<(AgySlot, AgyEvidenceHold)> = custody
        .turns
        .iter()
        .filter(|turn| matches!(turn.slot, AgySlot::Imessage(_)))
        .filter_map(|turn| {
            let reason = match evidence.get(&turn.slot) {
                Some(AgyEvidence::Bound(key)) if *key == turn.key => return None,
                Some(AgyEvidence::Bound(_)) => AgyEvidenceHold::Conflict,
                Some(AgyEvidence::Held(hold)) => *hold,
                None => AgyEvidenceHold::Missing,
            };
            Some((turn.slot.clone(), reason))
        })
        .collect();
    for (slot, reason) in unproven {
        custody.hold(&slot, AgyHoldReason::Evidence(reason));
    }
}

pub(crate) fn spool_destinations(root: &Path) -> io::Result<Vec<u64>> {
    let store = checked_dir(checked_dir(root.to_owned())?.join(STORE_DIR_NAME));
    let store = match store {
        Ok(store) => store,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut destinations = Vec::new();
    for channel in numeric_children(&store)? {
        match version_dir(root, channel) {
            Ok(_) => destinations.push(channel),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(destinations)
}

fn numeric_children(dir: &Path) -> io::Result<Vec<u64>> {
    let mut children = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.parse::<u64>().ok())
            && id != 0
            && name == id.to_string().as_str()
        {
            children.push(id);
        }
    }
    children.sort_unstable();
    Ok(children)
}

/// Turn slots by name. A malformed iMessage name is held; markers and temporary files are skipped.
fn slot_children(dir: &Path, custody: &mut AgyCustody) -> io::Result<Vec<AgySlot>> {
    let mut slots = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        match classify_slot(name) {
            SlotName::Slot(slot) => slots.push(slot),
            SlotName::Malformed => custody.held.push(AgyHeldSlot {
                name: name.to_owned(),
                reason: AgyHoldReason::Malformed,
            }),
            SlotName::Other => {}
        }
    }
    slots.sort_unstable();
    Ok(slots)
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod slot_tests;
