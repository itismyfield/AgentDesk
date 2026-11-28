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
    AgyFrontier, AgyOutcome, AgyReplay, AgyTarget, checked_dir, child_retired, mark_child_retired,
    replay, version_dir,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgyReadPermit {
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

pub struct AgyShadowHost {
    root: PathBuf,
    gate: Arc<OwnershipGate>,
    permit: AgyReadPermit,
    phase: Mutex<Phase>,
}

impl AgyShadowHost {
    pub fn start(
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

    /// Restore all known turns, including those with a saved frontier whose spool disappeared.
    pub(crate) fn restore(
        &self,
        previous: &BTreeMap<u64, AgyFrontier>,
    ) -> io::Result<Vec<AgyReplay>> {
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase == Phase::Retired || self.current().is_none() {
            return Err(io::Error::other("agy shadow reader lost custody"));
        }
        self.replay_all(previous)
    }

    fn replay_all(&self, previous: &BTreeMap<u64, AgyFrontier>) -> io::Result<Vec<AgyReplay>> {
        checked_dir(self.root.clone())?;
        if child_retired(&self.root, self.permit.target)? {
            return Err(io::Error::other("agy shadow reader lost custody"));
        }
        let AgyTarget { destination, .. } = self.permit.target;
        let turns = match version_dir(&self.root, destination) {
            Ok(dir) => numeric_children(&dir)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        for message in previous.keys() {
            if turns.binary_search(message).is_err() {
                return Err(io::Error::other(
                    "agy spool missing after a committed frontier",
                ));
            }
        }
        let records = turns
            .into_iter()
            .map(|message| {
                replay(
                    &self.root,
                    self.permit.target,
                    message,
                    previous.get(&message),
                )
            })
            .collect::<io::Result<Vec<_>>>()?;
        if self.current().is_none() {
            return Err(io::Error::other(
                "agy shadow reader lost custody during replay",
            ));
        }
        Ok(records)
    }

    /// Ready after source validation and confirmation that prior Legacy work is settled.
    pub fn adopt(
        &self,
        previous: &BTreeMap<u64, AgyFrontier>,
        prior_legacy_settled: bool,
    ) -> io::Result<(Option<AgyReadPermit>, Vec<AgyReplay>)> {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase == Phase::Retired || self.current().is_none() {
            return Err(io::Error::other("agy shadow reader lost custody"));
        }
        *phase = Phase::Booting;
        let turns = self.replay_all(previous)?;
        let mut permit = None;
        if prior_legacy_settled && turns.iter().all(|turn| turn.outcome != AgyOutcome::Pending) {
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
        Ok((permit, turns))
    }

    pub(crate) fn body_owner(&self) -> Option<AgyBodyOwner> {
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if *phase == Phase::Retired {
            return Some(AgyBodyOwner::Retired);
        }
        self.current().map(|_| AgyBodyOwner::Legacy)
    }

    /// Called after an exact row delete and a fresh successor check under the caller's input fence.
    /// Disposition must prove each whole turn settled; errors and pending spools keep it active.
    pub(crate) fn retire_child(
        &self,
        permit: AgyReadPermit,
        previous: &BTreeMap<u64, AgyFrontier>,
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
        let turns = self.replay_all(previous)?;
        if turns
            .iter()
            .any(|turn| turn.outcome == AgyOutcome::Pending || !settled(turn))
        {
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

pub fn spool_destinations(root: &Path) -> io::Result<Vec<u64>> {
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

#[cfg(all(test, unix))]
mod tests;
