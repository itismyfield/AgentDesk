//! Supervisor-only mode moves after a Legacy close; an effect admitted before the close keeps its
//! epoch, so a held or aborted transition never strands that effect's cleanup.
use super::{Closing, Failure, Mode};
use crate::services::discord::input_runtime::ordering::{AdmissionOrder, ValidatedOrderCapability};
use crate::services::provider::ProviderKind;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(crate) const TRANSITION_HELD: &str = "turn_transition_held";

// Channels whose handback finished while catch-up still has to queue the held-back messages in order.
struct Barrier {
    generation: u64,
    owner: Option<u64>,
}
static ORDER_BARRIERS: Mutex<BTreeMap<(String, u64), Barrier>> = Mutex::new(BTreeMap::new());
static NEXT_BARRIER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn order_barrier(provider: &ProviderKind, channel: u64) -> bool {
    let barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    barriers.contains_key(&(provider.as_str().to_owned(), channel))
}

pub(crate) fn claim_order_barrier(
    provider: &ProviderKind,
    channel: u64,
    owner: u64,
) -> Result<Option<u64>, Failure> {
    let mut barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(barrier) = barriers.get_mut(&(provider.as_str().to_owned(), channel)) else {
        return Ok(None);
    };
    if barrier.owner.is_some_and(|claimed| claimed != owner) {
        return Err(Failure::StalePermit);
    }
    barrier.owner = Some(owner);
    Ok(Some(barrier.generation))
}

pub(crate) fn release_order_barrier(provider: &ProviderKind, channel: u64, owner: u64) {
    let mut barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(barrier) = barriers.get_mut(&(provider.as_str().to_owned(), channel))
        && barrier.owner == Some(owner)
    {
        barrier.owner = None;
    }
}

/// Only the completed current sweep of this handback may reopen direct Legacy ingress.
pub(crate) fn settle_order_barrier(
    order: &AdmissionOrder,
    cap: ValidatedOrderCapability,
    epoch: u64,
) -> Result<bool, Failure> {
    let (provider, channel, owner) = cap.scope();
    let key = (provider.as_str().to_owned(), channel);
    let mut barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(barrier) = barriers.get(&key) else {
        return Ok(false);
    };
    let valid = order.validates_handback(&cap, epoch, barrier.generation);
    #[cfg(test)]
    let valid = valid || super::super::mutant("drop_capability");
    if barrier.owner != Some(owner) || !valid {
        return Err(Failure::StalePermit);
    }
    barriers.remove(&key);
    Ok(true)
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
        let mut barriers = ORDER_BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
        self.release_protection_after_handback()?;
        barriers.insert(
            key,
            Barrier {
                generation: NEXT_BARRIER.fetch_add(1, Ordering::Relaxed),
                owner: None,
            },
        );
        Ok(())
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
