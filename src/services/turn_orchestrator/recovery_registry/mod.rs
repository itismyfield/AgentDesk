//! The process-wide recovery registry: one core per process keeps a recovery slot for each channel
//! with a scoped install attempt or a kept responsibility. Only tests connect it.
#![cfg(unix)]

pub(crate) mod entry;
pub(crate) mod rehydrate;
pub(crate) mod witness;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use self::entry::Slot;
use super::recovery_custody::CustodyStore;
use super::recovery_custody::model::{Inspection, Phase};

/// Initial policy constants, not measurements: slots keeping install responsibility, and
/// concurrent install workers.
const INSTALL_SLOTS: usize = 1_024;
const INSTALL_WORKERS: usize = 2;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistryRefusal {
    /// Every slot keeps a responsibility; a new channel is refused and none is evicted.
    SlotsFull,
    WorkersBusy,
    /// Not this slot's channel, lane or current attempt.
    Foreign,
    /// Off or a scope change closed the attempt to new workers and arms.
    AdmissionClosed,
    /// The earlier attempt is still open, or has a worker or an arm that has not drained.
    NotQuiescent {
        open: bool,
        workers: usize,
        arms: usize,
    },
    /// The custody operation is armed, indeterminate or held: its outcome or a reconcile first.
    Undecided,
    /// Only a slot whose PG row is closed is reclaimed.
    NotClosed,
    /// The slot was reclaimed; the channel resolves a new one.
    Retired,
}

/// What a core's slots share: the process identity, the identities issued so far and the install
/// worker places.
pub(super) struct Shared {
    process_incarnation: String,
    issued: AtomicU64,
    worker_limit: usize,
    workers: Mutex<usize>,
}

impl Shared {
    /// A slot or attempt identity, never issued twice by this process.
    fn next_id(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn workers(&self) -> MutexGuard<'_, usize> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn take_worker(&self) -> bool {
        let mut workers = self.workers();
        let free = *workers < self.worker_limit;
        if free {
            *workers += 1;
        }
        free
    }

    fn release_worker(&self) {
        *self.workers() -= 1;
    }
}

pub(crate) struct RecoveryCore {
    shared: Arc<Shared>,
    slot_limit: usize,
    slots: Mutex<HashMap<u64, Arc<Slot>>>,
}

impl RecoveryCore {
    fn new(slot_limit: usize, worker_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                process_incarnation: uuid::Uuid::new_v4().to_string(),
                issued: AtomicU64::new(0),
                worker_limit,
                workers: Mutex::new(0),
            }),
            slot_limit,
            slots: Mutex::default(),
        })
    }
}

static CORE: LazyLock<Arc<RecoveryCore>> =
    LazyLock::new(|| RecoveryCore::new(INSTALL_SLOTS, INSTALL_WORKERS));

/// A handle on the process's one core: every `Default` and every clone sees the same slots.
#[derive(Clone)]
pub(crate) struct RecoveryRegistry(Arc<RecoveryCore>);

impl Default for RecoveryRegistry {
    fn default() -> Self {
        Self(CORE.clone())
    }
}

impl RecoveryRegistry {
    /// A core of its own, so a test's limits and slots stay out of the process's one core.
    #[cfg(test)]
    pub(crate) fn isolated(slot_limit: usize, worker_limit: usize) -> Self {
        Self(RecoveryCore::new(slot_limit, worker_limit))
    }

    #[cfg(test)]
    fn unlocked(&self) -> bool {
        self.0.slots.try_lock().is_ok() && self.0.shared.workers.try_lock().is_ok()
    }

    pub(crate) fn core(&self) -> &Arc<RecoveryCore> {
        &self.0
    }

    pub(crate) fn process_incarnation(&self) -> &str {
        &self.0.shared.process_incarnation
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<u64, Arc<Slot>>> {
        self.0
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The channel's slot, without creating one.
    pub(crate) fn peek(&self, channel: u64) -> Option<Arc<Slot>> {
        self.slots().get(&channel).cloned()
    }

    /// The channel's slot, created on first use. At the limit a new channel is refused; no
    /// existing slot is evicted for it, whatever its age.
    pub(crate) fn resolve(&self, channel: u64) -> Result<Arc<Slot>, RegistryRefusal> {
        let mut slots = self.slots();
        if let Some(slot) = slots.get(&channel) {
            return Ok(slot.clone());
        }
        if slots.len() >= self.0.slot_limit {
            return Err(RegistryRefusal::SlotsFull);
        }
        let slot = Slot::new(channel, self.0.shared.clone());
        slots.insert(channel, slot.clone());
        Ok(slot)
    }

    /// Frees the slot of an attempt that ended with no responsibility: closed and drained here
    /// and closed in PG. The PG row and its sticky history stay as they are.
    pub(crate) async fn reclaim(
        &self,
        custody: &CustodyStore,
        channel: u64,
    ) -> Result<(), RegistryRefusal> {
        let slot = self.peek(channel).ok_or(RegistryRefusal::Foreign)?;
        let ended = slot.drained()?;
        let closed = matches!(
            custody.inspect(&channel.to_string()).await,
            Ok(Inspection::Row(row)) if row.phase == Phase::Closed && row.active_operation.is_none()
        );
        if !closed {
            return Err(RegistryRefusal::NotClosed);
        }
        slot.retire(ended)?;
        let mut slots = self.slots();
        if slots
            .get(&channel)
            .is_some_and(|current| Arc::ptr_eq(current, &slot))
        {
            slots.remove(&channel);
        }
        Ok(())
    }
}

#[cfg(test)]
mod registry_pg_tests;
