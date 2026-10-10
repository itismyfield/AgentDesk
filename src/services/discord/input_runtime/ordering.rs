//! Supervisor and post-handback scan authority; fetch tickets precede all external IO.
use crate::services::discord::input_runtime::fence::Failure;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::input_key::is_discord_key;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_HANDBACK: AtomicU64 = AtomicU64::new(1);
static BARRIERS: Mutex<BTreeMap<(String, u64), HandbackEntry>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Debug)]
pub(crate) struct PendingOverflow {
    provider: ProviderKind,
    channel: u64,
    generation: Arc<AtomicU64>,
}

impl PendingOverflow {
    fn new(provider: ProviderKind, channel: u64) -> Self {
        Self {
            provider,
            channel,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Saturation permanently closes admission rather than making an old generation current.
    pub(crate) fn mark_dirty(&self) {
        #[cfg(test)]
        if mutant("drop_pending_overflow") {
            return;
        }
        let _ = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.saturating_add(1))
            });
    }
}

#[derive(Debug)]
pub(crate) struct PendingSource {
    pub(crate) sources: Vec<u64>,
    pub(crate) dirty_generation: u64,
    pub(crate) overflow: PendingOverflow,
}

#[derive(Clone, Debug)]
struct Stamp {
    provider: ProviderKind,
    channel: u64,
    owner: u64,
    epoch: u64,
    scan: u64,
    dirty: u64,
    overflow: u64,
    frontier: u64,
    handback: Option<u64>,
}

pub(crate) struct AdmissionOrder {
    stamp: Stamp,
    pending: BTreeSet<u64>,
    barrier: Option<u64>,
    overflow: PendingOverflow,
    verified_overflow: u64,
}

#[derive(Debug)]
pub(crate) struct FetchTicket {
    stamp: Stamp,
    post_pending_fetch: bool,
}

#[derive(Debug)]
pub(crate) struct ScanCapability {
    ticket: FetchTicket,
    sources: VecDeque<u64>,
    horizon: u64,
    complete_fetch: bool,
}

#[derive(Debug)]
pub(crate) struct ValidatedOrderCapability(ScanCapability);

struct HandbackEntry {
    release_epoch: u64,
    order: AdmissionOrder,
    settled_overflow: Option<u64>,
}

impl AdmissionOrder {
    pub(crate) fn new(provider: ProviderKind, channel: u64, epoch: u64, frontier: u64) -> Self {
        Self {
            stamp: Stamp {
                provider: provider.clone(),
                channel,
                owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
                epoch,
                scan: 0,
                dirty: 0,
                overflow: 0,
                frontier,
                handback: None,
            },
            pending: BTreeSet::new(),
            barrier: None,
            overflow: PendingOverflow::new(provider, channel),
            verified_overflow: 0,
        }
    }

    pub(crate) fn invalidate(&mut self, epoch: u64) {
        self.stamp.epoch = epoch;
        self.stamp.scan += 1;
    }

    pub(crate) fn offer_ready(&self) -> bool {
        let generation = self.overflow.generation();
        let proved = generation != u64::MAX && generation == self.verified_overflow;
        #[cfg(test)]
        let proved = proved || mutant("drop_offer_proof");
        self.pending.is_empty() && self.barrier.is_none() && proved
    }

    pub(crate) fn pending_overflow(&self) -> PendingOverflow {
        self.overflow.clone()
    }

    pub(crate) fn pending_snapshot(&self) -> PendingSource {
        PendingSource {
            sources: self.pending.iter().copied().collect(),
            dirty_generation: self.stamp.dirty,
            overflow: self.overflow.clone(),
        }
    }

    /// The entry for a live notice: one non-Discord key refuses the whole notice before any change.
    pub(crate) fn admit_pending(&mut self, sources: &[u64]) -> Result<PendingSource, Failure> {
        if !discord_sources(sources) {
            return Err(Failure::StalePermit);
        }
        Ok(self.pending(sources))
    }

    /// Retries re-record sources this order already checked, so they cannot fail.
    pub(crate) fn pending(&mut self, sources: &[u64]) -> PendingSource {
        debug_assert!(discord_sources(sources));
        let sources: Vec<_> = sources
            .iter()
            .copied()
            .filter(|s| *s > self.stamp.frontier)
            .collect();
        if !sources.is_empty() {
            self.pending.extend(&sources);
            self.stamp.dirty += 1;
        }
        PendingSource {
            sources,
            dirty_generation: self.stamp.dirty,
            overflow: self.overflow.clone(),
        }
    }

    /// Only this pre-fetch snapshot can authorize the returned immutable page.
    pub(crate) fn fetch_ticket(&mut self, epoch: u64) -> Result<FetchTicket, Failure> {
        self.stamp.overflow = self.overflow.generation();
        if epoch != self.stamp.epoch || self.stamp.overflow == u64::MAX {
            return Err(Failure::StalePermit);
        }
        self.stamp.scan += 1;
        Ok(FetchTicket {
            stamp: self.stamp.clone(),
            post_pending_fetch: self.pending.is_empty() && self.barrier.is_none(),
        })
    }

    pub(crate) fn begin_scan(
        &mut self,
        ticket: FetchTicket,
        epoch: u64,
        sources: Vec<u64>,
        horizon: u64,
        complete_fetch: bool,
    ) -> Result<ScanCapability, Failure> {
        let current = self.current(&ticket.stamp, epoch);
        #[cfg(test)]
        let current = current || mutant("drop_ticket_validation");
        // Sources are sorted and bounded by the horizon, so a Discord horizon bounds them all.
        if !current
            || (horizon != 0 && !is_discord_key(horizon))
            || horizon < self.stamp.frontier
            || sources.first().is_some_and(|s| *s <= self.stamp.frontier)
            || sources.windows(2).any(|p| p[0] >= p[1])
            || sources.last().is_some_and(|s| *s > horizon)
            || self
                .pending
                .range(..=horizon)
                .any(|s| sources.binary_search(s).is_err())
            || self
                .barrier
                .is_some_and(|s| s <= horizon && sources.binary_search(&s).is_err())
        {
            return Err(Failure::StalePermit);
        }
        Ok(ScanCapability {
            ticket,
            sources: sources.into(),
            horizon,
            complete_fetch,
        })
    }

    fn current(&self, stamp: &Stamp, epoch: u64) -> bool {
        let epoch_matches = epoch == self.stamp.epoch && stamp.epoch == epoch;
        #[cfg(test)]
        let epoch_matches = epoch_matches
            || mutant("drop_epoch")
            || (stamp.handback.is_some() && mutant("drop_release_epoch"));
        let overflow_matches =
            stamp.overflow != u64::MAX && stamp.overflow == self.overflow.generation();
        #[cfg(test)]
        let overflow_matches = overflow_matches || mutant("drop_overflow_generation");
        stamp.provider == self.stamp.provider
            && stamp.channel == self.stamp.channel
            && stamp.owner == self.stamp.owner
            && epoch_matches
            && stamp.scan == self.stamp.scan
            && stamp.dirty == self.stamp.dirty
            && overflow_matches
            && stamp.frontier == self.stamp.frontier
            && stamp.handback == self.stamp.handback
    }

    pub(crate) fn permits(
        &self,
        cap: &ScanCapability,
        epoch: u64,
        source: u64,
    ) -> Result<(), Failure> {
        self.permits_sources(cap, epoch, &[source])
    }

    pub(crate) fn permits_sources(
        &self,
        cap: &ScanCapability,
        epoch: u64,
        sources: &[u64],
    ) -> Result<(), Failure> {
        let range = !sources.is_empty()
            && sources.len() <= cap.sources.len()
            && cap.sources.iter().take(sources.len()).eq(sources.iter())
            && sources
                .iter()
                .all(|s| *s > self.stamp.frontier && *s <= cap.horizon)
            && self
                .barrier
                .is_none_or(|b| sources.last().is_none_or(|s| *s <= b));
        #[cfg(test)]
        let range = range || mutant("drop_source_range");
        if !self.current(&cap.ticket.stamp, epoch) || !range {
            return Err(Failure::StalePermit);
        }
        Ok(())
    }

    pub(crate) fn settle(
        &mut self,
        cap: &mut ScanCapability,
        epoch: u64,
        source: u64,
    ) -> Result<(), Failure> {
        self.settle_sources(cap, epoch, &[source])
    }

    /// The entire checked prefix settles only after its durable commit or final disposition.
    pub(crate) fn settle_sources(
        &mut self,
        cap: &mut ScanCapability,
        epoch: u64,
        sources: &[u64],
    ) -> Result<(), Failure> {
        self.permits_sources(cap, epoch, sources)?;
        for source in sources {
            cap.sources.pop_front();
            self.pending.remove(source);
            self.stamp.frontier = *source;
            if self.barrier == Some(*source) {
                self.barrier = None;
            }
        }
        cap.ticket.stamp.frontier = self.stamp.frontier;
        Ok(())
    }

    pub(crate) fn defer(
        &mut self,
        cap: &ScanCapability,
        epoch: u64,
        source: u64,
    ) -> Result<(), Failure> {
        self.permits(cap, epoch, source)?;
        let retain = true;
        #[cfg(test)]
        let retain = retain && !mutant("drop_deferred_pending");
        if retain {
            self.pending.insert(source);
        }
        self.barrier = Some(source);
        self.stamp.scan += 1;
        Ok(())
    }

    fn completed(&self, cap: &ScanCapability, epoch: u64) -> bool {
        let pending_settled = self.pending.is_empty();
        #[cfg(test)]
        let pending_settled = pending_settled || mutant("drop_handback_pending");
        self.current(&cap.ticket.stamp, epoch)
            && cap.complete_fetch
            && cap.sources.is_empty()
            && pending_settled
            && self.barrier.is_none()
            && cap.horizon >= self.stamp.frontier
    }

    pub(crate) fn complete(
        &mut self,
        cap: ScanCapability,
        epoch: u64,
    ) -> Result<ValidatedOrderCapability, Failure> {
        if !self.completed(&cap, epoch) {
            return Err(Failure::StalePermit);
        }
        self.verified_overflow = cap.ticket.stamp.overflow;
        Ok(ValidatedOrderCapability(cap))
    }
}

/// Only Discord snowflakes order a scan; external and synthetic keys never enter it.
fn discord_sources(sources: &[u64]) -> bool {
    sources.iter().all(|source| is_discord_key(*source))
}

pub(crate) fn order_barrier(provider: &ProviderKind, channel: u64) -> bool {
    BARRIERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(provider.as_str().to_owned(), channel))
        .is_some_and(|entry| entry.settled_overflow != Some(entry.order.overflow.generation()))
}

/// The same lock covers protection release and installation, so ingress never sees a gap.
pub(in crate::services::discord::input_runtime) fn install_handback(
    provider: ProviderKind,
    channel: u64,
    pending: &[u64],
    dirty: u64,
    release: impl FnOnce() -> Result<u64, Failure>,
) -> Result<(), Failure> {
    let snapshot = PendingSource {
        sources: pending.to_vec(),
        dirty_generation: dirty,
        overflow: PendingOverflow::new(provider.clone(), channel),
    };
    install_handback_snapshot(provider, channel, &snapshot, release)
}

pub(in crate::services::discord::input_runtime) fn install_handback_snapshot(
    provider: ProviderKind,
    channel: u64,
    snapshot: &PendingSource,
    release: impl FnOnce() -> Result<u64, Failure>,
) -> Result<(), Failure> {
    if snapshot.overflow.provider != provider
        || snapshot.overflow.channel != channel
        || !discord_sources(&snapshot.sources)
    {
        return Err(Failure::StalePermit);
    }
    let mut entries = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let release_epoch = release()?;
    let mut order = AdmissionOrder::new(provider.clone(), channel, release_epoch, 0);
    order.pending.extend(&snapshot.sources);
    order.stamp.dirty = snapshot.dirty_generation;
    order.overflow = snapshot.overflow.clone();
    order.stamp.handback = Some(NEXT_HANDBACK.fetch_add(1, Ordering::Relaxed));
    entries.insert(
        (provider.as_str().to_owned(), channel),
        HandbackEntry {
            release_epoch,
            order,
            settled_overflow: None,
        },
    );
    Ok(())
}

fn with_handback<T>(
    provider: &ProviderKind,
    channel: u64,
    action: impl FnOnce(&mut AdmissionOrder, u64) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let mut entries = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = entries
        .get_mut(&(provider.as_str().to_owned(), channel))
        .ok_or(Failure::StalePermit)?;
    if entry.order.stamp.provider != *provider {
        return Err(Failure::StalePermit);
    }
    entry.settled_overflow = None;
    action(&mut entry.order, entry.release_epoch)
}

pub(crate) fn handback_pending(
    provider: &ProviderKind,
    channel: u64,
    sources: &[u64],
) -> Result<PendingSource, Failure> {
    // Refused before the entry is touched, so a settled barrier stays settled.
    if !discord_sources(sources) {
        return Err(Failure::StalePermit);
    }
    with_handback(provider, channel, |order, _| order.admit_pending(sources))
}

pub(crate) fn handback_fetch_ticket(
    provider: &ProviderKind,
    channel: u64,
) -> Result<FetchTicket, Failure> {
    with_handback(provider, channel, |order, epoch| order.fetch_ticket(epoch))
}

pub(crate) fn handback_begin_scan(
    ticket: FetchTicket,
    sources: Vec<u64>,
    horizon: u64,
    complete_fetch: bool,
) -> Result<ScanCapability, Failure> {
    let (provider, channel) = (ticket.stamp.provider.clone(), ticket.stamp.channel);
    with_handback(&provider, channel, |order, epoch| {
        order.begin_scan(ticket, epoch, sources, horizon, complete_fetch)
    })
}

pub(crate) fn handback_permits_sources(
    cap: &ScanCapability,
    sources: &[u64],
) -> Result<(), Failure> {
    with_handback(
        &cap.ticket.stamp.provider,
        cap.ticket.stamp.channel,
        |order, epoch| order.permits_sources(cap, epoch, sources),
    )
}

/// The synchronous durable callback shares the entry lock with validation and settlement.
/// Fetch and materialization finish before this call; async Legacy ports need a serialized loan.
pub(crate) fn handback_commit_from_scan<T>(
    mut cap: ScanCapability,
    sources: &[u64],
    commit: impl FnOnce() -> Result<T, Failure>,
) -> Result<(T, ScanCapability), Failure> {
    let (provider, channel) = (cap.ticket.stamp.provider.clone(), cap.ticket.stamp.channel);
    with_handback(&provider, channel, |order, epoch| {
        order.permits_sources(&cap, epoch, sources)?;
        let committed = match commit() {
            Ok(committed) => committed,
            Err(failure) => {
                // Preserve the persistence cause and range even if dirty invalidates deferral.
                if let Some(source) = sources.first() {
                    let _ = order.defer(&cap, epoch, *source);
                }
                let retain = true;
                #[cfg(test)]
                let retain = retain && !mutant("drop_uncertain_retry");
                if retain {
                    order.pending(sources);
                }
                return Err(failure);
            }
        };
        if let Err(failure) = order.settle_sources(&mut cap, epoch, sources) {
            let retain = true;
            #[cfg(test)]
            let retain = retain && !mutant("drop_settlement_retry");
            if retain {
                order.pending(sources);
            }
            return Err(failure);
        }
        Ok((committed, cap))
    })
}

pub(crate) fn handback_settle_sources(
    mut cap: ScanCapability,
    sources: &[u64],
) -> Result<ScanCapability, Failure> {
    let (provider, channel) = (cap.ticket.stamp.provider.clone(), cap.ticket.stamp.channel);
    with_handback(&provider, channel, |order, epoch| {
        order.settle_sources(&mut cap, epoch, sources)?;
        Ok(cap)
    })
}

pub(crate) fn handback_defer(cap: &ScanCapability, source: u64) -> Result<(), Failure> {
    with_handback(
        &cap.ticket.stamp.provider,
        cap.ticket.stamp.channel,
        |order, epoch| order.defer(cap, epoch, source),
    )
}

pub(crate) fn handback_complete(cap: ScanCapability) -> Result<ValidatedOrderCapability, Failure> {
    let post_pending = cap.ticket.post_pending_fetch;
    #[cfg(test)]
    let post_pending = post_pending || mutant("drop_post_pending_fetch");
    if !post_pending {
        return Err(Failure::StalePermit);
    }
    let (provider, channel) = (cap.ticket.stamp.provider.clone(), cap.ticket.stamp.channel);
    with_handback(&provider, channel, |order, epoch| {
        order.complete(cap, epoch)
    })
}

pub(crate) fn settle_handback(cap: ValidatedOrderCapability) -> Result<bool, Failure> {
    let stamp = &cap.0.ticket.stamp;
    let key = (stamp.provider.as_str().to_owned(), stamp.channel);
    let mut entries = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(entry) = entries.get_mut(&key) else {
        return Ok(false);
    };
    if entry.settled_overflow == Some(entry.order.overflow.generation()) {
        return Ok(false);
    }
    let valid = entry.order.completed(&cap.0, entry.release_epoch)
        && cap.0.ticket.post_pending_fetch
        && stamp.handback.is_some();
    #[cfg(test)]
    let valid = valid || mutant("drop_capability");
    if !valid {
        return Err(Failure::StalePermit);
    }
    // Keep the shared handle: overflow racing this acknowledgement re-arms the barrier.
    entry.settled_overflow = Some(stamp.overflow);
    entry.order.stamp.scan += 1;
    Ok(true)
}

#[cfg(test)]
fn mutant(name: &str) -> bool {
    crate::services::discord::input_runtime::supervisor::mutant(name)
}

#[cfg(test)]
#[path = "ordering_tests.rs"]
mod tests;
