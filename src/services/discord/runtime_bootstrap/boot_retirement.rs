//! Dormant boot authority; installing it does not connect a Legacy publication sink.

mod cohort;
mod completion;
mod publication;

pub use super::super::health::legacy_supervision::boot_status::{
    BootPhase, BootRetirementHealth, BootSlotState,
};
pub use cohort::{BootCohort, BootSlot};
pub use completion::{BootWorkFailure, BootWorkOnce, Completed};
pub use publication::BootPublication;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

static PROCESS: OnceLock<Arc<BootCohort>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootSelection {
    pub runtime_kind: String,
    pub turn_channels: BTreeSet<u64>,
}

#[derive(Clone, Debug)]
pub struct BootBot {
    pub slot: String,
    pub provider: String,
    pub utility: bool,
    pub selection: BootSelection,
}

pub struct BootRoster {
    bots: BTreeMap<String, BootBot>,
    providers: BTreeMap<String, BootSelection>,
}

impl BootRoster {
    pub fn new(bots: Vec<BootBot>) -> Result<Self, BootWorkFailure> {
        let mut roster = Self {
            bots: BTreeMap::new(),
            providers: BTreeMap::new(),
        };
        for mut bot in bots {
            bot.provider.make_ascii_lowercase();
            if roster.bots.contains_key(&bot.slot)
                || roster
                    .providers
                    .get(&bot.provider)
                    .is_some_and(|selection| selection != &bot.selection)
            {
                return Err(BootWorkFailure::Invalid(
                    "duplicate slot or conflicting provider snapshot",
                ));
            }
            roster
                .providers
                .insert(bot.provider.clone(), bot.selection.clone());
            roster.bots.insert(bot.slot.clone(), bot);
        }
        Ok(roster)
    }
}

impl BootCohort {
    pub fn install_process(roster: BootRoster) -> Result<Arc<Self>, BootWorkFailure> {
        Self::install_in(&PROCESS, roster)
    }

    fn install_in(
        cell: &OnceLock<Arc<Self>>,
        roster: BootRoster,
    ) -> Result<Arc<Self>, BootWorkFailure> {
        let cohort = Arc::new(Self::new(roster));
        cell.set(cohort.clone())
            .map_err(|_| BootWorkFailure::Invalid("process epoch already installed"))?;
        Ok(cohort)
    }
}

#[cfg(test)]
mod cohort_tests;
#[cfg(test)]
mod completion_tests;
#[cfg(test)]
mod publication_tests;
