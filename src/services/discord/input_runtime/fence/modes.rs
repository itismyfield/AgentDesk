//! Supervisor-only mode moves after a Legacy close; an effect admitted before the close keeps its
//! epoch, so a held or aborted transition never strands that effect's cleanup.
use super::{Closing, Failure, Gate, Mode};
use crate::services::discord::input_runtime::supervisor::ordering::{
    self, ValidatedOrderCapability,
};
use crate::services::provider::ProviderKind;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) const TRANSITION_HELD: &str = "turn_transition_held";

pub(crate) fn order_barrier(provider: &ProviderKind, channel: u64) -> bool {
    ordering::order_barrier(provider, channel)
}

/// Only the completed current sweep of this handback may reopen direct Legacy ingress.
pub(crate) fn settle_order_barrier(cap: ValidatedOrderCapability) -> Result<bool, Failure> {
    ordering::settle_handback(cap)
}

/// The Gate a handback release opened and the epoch that release produced; only a Closing makes one.
pub(crate) struct ReleaseProof {
    pub(super) gate: Arc<Gate>,
    pub(super) epoch: u64,
}

impl ReleaseProof {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The release still stands: no restore or later close has moved this Gate since.
    pub(crate) fn current(&self) -> bool {
        let state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch == self.epoch
            && state.mode == Mode::LegacyOpen
            && !self.gate.protected.load(Ordering::Acquire)
    }

    pub(crate) fn names(&self, closing: &Closing) -> bool {
        Arc::ptr_eq(&self.gate, &closing.gate)
    }
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

    /// Close checks the live Gate rather than the drive's readiness cache.
    pub(crate) fn ensure_held_current(
        &self,
        provider: &ProviderKind,
        channel: u64,
    ) -> Result<(), Failure> {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        if super::super::supervisor::mutant("skip_close_current_held") {
            return Ok(());
        }
        self.check_close_current(
            &state,
            provider,
            channel,
            &[Mode::Frozen, Mode::LedgerOpen, Mode::Held],
        )?;
        state.mode = Mode::Held;
        drop(state);
        self.report(Some(TRANSITION_HELD));
        Ok(())
    }

    /// The final Held check and ACK share the Gate lock, after ledger work has completed.
    pub(crate) fn with_held_current<T>(
        &self,
        provider: &ProviderKind,
        channel: u64,
        ack: impl FnOnce() -> T,
    ) -> Result<T, Failure> {
        let state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        if super::super::supervisor::mutant("skip_close_final_held") {
            return Ok(ack());
        }
        self.check_close_current(&state, provider, channel, &[Mode::Held])?;
        #[cfg(test)]
        if super::super::supervisor::mutant("drop_close_ack_serialization") {
            drop(state);
            return Ok(ack());
        }
        Ok(ack())
    }

    fn check_close_current(
        &self,
        state: &super::State,
        provider: &ProviderKind,
        channel: u64,
        from: &[Mode],
    ) -> Result<(), Failure> {
        if state.exhausted {
            return Err(Failure::GenerationExhausted);
        }
        if provider != self.provider()
            || channel != self.channel()
            || !self.gate.protected.load(Ordering::Acquire)
            || state.epoch != self.epoch.load(Ordering::Acquire)
        {
            return Err(Failure::StalePermit);
        }
        if state.effects != 0 {
            return Err(Failure::Busy);
        }
        if !from.contains(&state.mode) {
            return Err(Failure::Mode(state.mode));
        }
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
        if state.exhausted {
            return Err(Failure::GenerationExhausted);
        }
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

    /// Releases protection after a complete handback while keeping the catch-up order barrier; the
    /// live producer's overflow handle and pending sources cross the boundary whole.
    pub(crate) fn release_after_handback_with_pending(
        &self,
        snapshot: &ordering::PendingSource,
    ) -> Result<(), Failure> {
        ordering::install_handback_snapshot(self, snapshot, || {
            self.release_protection_after_handback()
        })
    }

    // No permit can exist past the freeze, so these moves keep the epoch unchanged.
    fn shift(&self, from: &[Mode], to: Mode) -> Result<(), Failure> {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        let exhausted = state.exhausted;
        #[cfg(test)]
        let exhausted =
            exhausted && !super::super::supervisor::mutant("allow_shift_after_exhaustion");
        if exhausted {
            return Err(Failure::GenerationExhausted);
        }
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

#[cfg(test)]
#[path = "g1b_modes_tests.rs"]
mod g1b_tests;
