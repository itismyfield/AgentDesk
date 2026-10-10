//! Dormant boot authority; publication sinks are connected only by a later boot adapter.
mod cohort;
mod completion;
mod publication;

pub use super::super::health::legacy_supervision::boot_status::{
    BootPhase, BootRetirementHealth, BootSlotState,
};
pub use cohort::{BootCohort, BootSlot};
use completion::BootWorkFailure::Invalid;
pub use completion::{BootResult, BootWorkFailure, BootWorkOnce, Completed};
pub use publication::BootPublication;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

static PROCESS: OnceLock<()> = OnceLock::new();
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

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
    bots: Vec<BootBot>,
    providers: BTreeMap<String, BootSelection>,
}

impl BootRoster {
    pub fn new(mut bots: Vec<BootBot>) -> BootResult<Self> {
        for bot in &mut bots {
            bot.provider.make_ascii_lowercase();
        }
        for (index, bot) in bots.iter().enumerate() {
            if bots[..index].iter().any(|prior| {
                prior.slot == bot.slot
                    || (prior.provider == bot.provider && prior.selection != bot.selection)
            }) {
                return Err(Invalid("duplicate slot or conflicting provider snapshot"));
            }
        }
        let providers = bots
            .iter()
            .filter(|bot| !bot.utility)
            .map(|bot| (bot.provider.clone(), bot.selection.clone()))
            .collect();
        Ok(Self { bots, providers })
    }
}
impl<T: Send + Sync + 'static> BootCohort<T> {
    pub fn install_process(roster: BootRoster) -> BootResult<Arc<Self>> {
        Self::install_in(&PROCESS, roster)
    }
    fn install_in(cell: &OnceLock<()>, roster: BootRoster) -> BootResult<Arc<Self>> {
        cell.set(())
            .map_err(|_| Invalid("process epoch already installed"))?;
        Ok(Arc::new(Self::new(
            NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            roster,
        )))
    }
}
#[cfg(test)]
mod cohort_tests;
#[cfg(test)]
mod completion_tests;
