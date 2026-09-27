//! Seal bookkeeping: a unit seals once and its plan stays immutable.

use std::collections::{BTreeSet, HashMap};

use super::UnitKey;
use super::unit_plan::UnitPlan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealOutcome {
    First,
    /// Same key and plan again, e.g. a forked source copying an ancestor block.
    Repeat,
    /// Same key with a different plan: the sealed unit would have changed.
    Conflict,
}

#[derive(Debug, Default)]
pub struct SealRegistry {
    sealed: HashMap<UnitKey, UnitPlan>,
    announced: BTreeSet<UnitKey>,
}

impl SealRegistry {
    pub fn announce(&mut self, key: UnitKey) {
        if !self.sealed.contains_key(&key) {
            self.announced.insert(key);
        }
    }

    pub fn seal(&mut self, key: &UnitKey, plan: &UnitPlan) -> SealOutcome {
        self.announced.remove(key);
        match self.sealed.get(key) {
            None => {
                self.sealed.insert(key.clone(), plan.clone());
                SealOutcome::First
            }
            Some(sealed) if sealed == plan => SealOutcome::Repeat,
            Some(_) => SealOutcome::Conflict,
        }
    }

    /// Announced units whose sealing record has not been captured yet.
    pub fn unsealed(&self) -> Vec<UnitKey> {
        self.announced.iter().cloned().collect()
    }
}
