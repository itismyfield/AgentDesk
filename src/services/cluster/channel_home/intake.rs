//! Whether a held home takes new intake: not while a drain closed it, and, for a home whose
//! runtime restores the channel itself once held, not until that restore installed its input.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{HomeGate, HomeIntake, HomeLocal};

impl HomeGate {
    /// Takes intake only once `restored` is set; the gate still admits recovery and owed POSTs.
    pub(crate) fn opens_after(mut self, restored: Option<Arc<AtomicBool>>) -> Self {
        self.opens_after = restored;
        self
    }

    /// The intake of the epoch held as `home_epoch`, read under the gate's local lock.
    pub(super) fn intake_of(&self, local: &HomeLocal, home_epoch: i64) -> HomeIntake {
        #[cfg(test)]
        let restoring = !super::super::channel_home_boot::restore_mutant("intake_ignores_restore")
            && self.restoring();
        #[cfg(not(test))]
        let restoring = self.restoring();
        if local.intake_closed == Some(home_epoch) || restoring {
            HomeIntake::Closed
        } else {
            HomeIntake::Open
        }
    }

    fn restoring(&self) -> bool {
        let restored = self.opens_after.as_ref();
        restored.is_some_and(|restored| !restored.load(Ordering::Acquire))
    }
}
