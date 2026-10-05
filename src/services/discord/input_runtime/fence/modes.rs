//! Supervisor-only mode moves after a Legacy close; an effect admitted before the close keeps its
//! epoch, so a held or aborted transition never strands that effect's cleanup.
use super::{Closing, Failure, Mode};
use crate::services::provider::ProviderKind;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) const TRANSITION_HELD: &str = "turn_transition_held";

// Channels whose handback finished while catch-up still has to queue the held-back messages in order.
static ORDER_BARRIERS: Mutex<BTreeSet<(String, u64)>> = Mutex::new(BTreeSet::new());

pub(crate) fn order_barrier(provider: &ProviderKind, channel: u64) -> bool {
    let barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    barriers.contains(&(provider.as_str().to_owned(), channel))
}

/// Ends the barrier once catch-up has settled every message held back during the transition.
pub(crate) fn settle_order_barrier(provider: &ProviderKind, channel: u64) -> bool {
    let mut barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    barriers.remove(&(provider.as_str().to_owned(), channel))
}

impl Closing {
    /// Waits for effects admitted before the close; a timeout leaves the gate Closing and reports
    /// the hold, so those effects can still finish under their own permits.
    pub(crate) async fn drain_within(&self, limit: Duration) -> Result<(), Failure> {
        if tokio::time::timeout(limit, self.drain()).await.is_ok() {
            return Ok(());
        }
        self.report(Some(TRANSITION_HELD));
        Err(Failure::Busy)
    }

    /// Opens ledger submission after a committed move; Legacy admission stays refused.
    pub(crate) fn open_ledger(&self) -> Result<(), Failure> {
        self.shift(&[Mode::Frozen, Mode::Held], Mode::LedgerOpen)?;
        self.report(None);
        Ok(())
    }

    /// Holds a moved channel; neither Legacy nor the ledger may act until a later handback or open.
    pub(crate) fn hold(&self) -> Result<(), Failure> {
        self.shift(&[Mode::Frozen, Mode::LedgerOpen], Mode::Held)?;
        self.report(Some(TRANSITION_HELD));
        Ok(())
    }

    /// Grants the population guard for returning ledger rows to the Legacy queue.
    pub(crate) fn begin_handback(&self) -> Result<(), Failure> {
        self.shift(
            &[Mode::Frozen, Mode::LedgerOpen, Mode::Held],
            Mode::Handback,
        )
    }

    /// Returns a channel the ledger never owned to Legacy; earlier permits stay valid.
    pub(crate) fn abort_close(&self, ledger_history: bool) -> Result<(), Failure> {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if ledger_history
            || !self.gate.protected.load(Ordering::Acquire)
            || state.epoch != self.epoch.load(Ordering::Acquire)
            || !matches!(state.mode, Mode::Closing | Mode::Frozen)
        {
            return Err(Failure::Mode(state.mode));
        }
        state.mode = Mode::LegacyOpen;
        drop(state);
        self.report(None);
        Ok(())
    }

    /// Releases protection after a complete handback while keeping the catch-up order barrier.
    pub(crate) fn release_after_handback(&self) -> Result<(), Failure> {
        let key = (self.provider().as_str().to_owned(), self.channel());
        let inserted = ORDER_BARRIERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        let released = self.release_protection_after_handback();
        // A refused retry keeps a barrier an earlier release installed; catch-up still owes its settle.
        if released.is_err() && inserted {
            ORDER_BARRIERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }
        released
    }

    // No permit can exist past the freeze, so these moves keep the epoch unchanged.
    fn shift(&self, from: &[Mode], to: Mode) -> Result<(), Failure> {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.gate.protected.load(Ordering::Acquire)
            || state.effects != 0
            || state.epoch != self.epoch.load(Ordering::Acquire)
            || !from.contains(&state.mode)
        {
            return Err(Failure::Mode(state.mode));
        }
        state.mode = to;
        Ok(())
    }

    fn report(&self, reason: Option<&str>) {
        *self.gate.health.lock().unwrap_or_else(|e| e.into_inner()) = reason.map(|reason| {
            format!(
                "{reason} provider={} channel={}",
                self.gate.provider.as_str(),
                self.gate.channel
            )
        });
    }
}

#[cfg(test)]
#[path = "modes_tests.rs"]
mod tests;
