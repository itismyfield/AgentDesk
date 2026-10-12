//! Supervisor and post-handback scan authority; fetch tickets precede all external IO.
use crate::services::discord::input_runtime::fence::modes::ReleaseProof;
use crate::services::discord::input_runtime::fence::{Closing, Failure};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::input_key::is_discord_key;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_HANDBACK: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static HANDBACK_CALLBACKS: AtomicU64 = AtomicU64::new(0);
static BARRIERS: Mutex<BTreeMap<(String, u64), HandbackEntry>> = Mutex::new(BTreeMap::new());

/// Exhaustion refuses for good and is never the namespace verdict, so a producer stays armed.
pub(crate) const EXHAUSTED: Failure = Failure::GenerationExhausted;

/// MAX is the exhausted sentinel, so no generation ever issues it as current.
fn next_generation(value: u64) -> Option<u64> {
    #[cfg(test)]
    if mutant("max_generation_remains_valid") {
        return value.checked_add(1);
    }
    value.checked_add(1).filter(|next| *next != u64::MAX)
}

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
    exhausted: bool,
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
    // A scan or dirty generation that cannot advance closes this order for good.
    exhausted: bool,
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
    release: ReleaseProof,
    order: AdmissionOrder,
    settled_overflow: Option<u64>,
}

/// A callback that ran keeps its own result apart from the settlement that followed it.
#[derive(Debug)]
pub(crate) struct HandbackExecution<T> {
    pub(crate) outcome: Result<T, Failure>,
    pub(crate) settlement: Result<(), Failure>,
    // Only a fully settled execution may continue the scan.
    pub(crate) next: Option<ScanCapability>,
}

impl AdmissionOrder {
    pub(crate) fn new(
        provider: ProviderKind,
        channel: u64,
        epoch: u64,
        frontier: u64,
    ) -> Result<Self, Failure> {
        let owner = NEXT_OWNER
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                #[cfg(test)]
                if mutant("reuse_owner_generation") {
                    return Some(value.wrapping_add(1));
                }
                value.checked_add(1)
            })
            .map_err(|_| EXHAUSTED)?;
        Ok(Self {
            stamp: Stamp {
                provider: provider.clone(),
                channel,
                owner,
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
            exhausted: false,
        })
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.exhausted || self.overflow.generation() == u64::MAX
    }

    pub(crate) fn exhaust(&mut self) {
        self.exhausted = true;
    }

    pub(crate) fn invalidate(&mut self, epoch: u64) {
        self.stamp.epoch = epoch;
        self.next_scan();
    }

    // A scan generation that cannot advance would leave old capabilities current, so it latches.
    fn next_scan(&mut self) {
        match next_generation(self.stamp.scan) {
            Some(scan) => self.stamp.scan = scan,
            None => self.exhausted = true,
        }
    }

    pub(crate) fn offer_ready(&self) -> bool {
        let generation = self.overflow.generation();
        let proved = generation != u64::MAX && generation == self.verified_overflow;
        #[cfg(test)]
        let proved = proved || mutant("drop_offer_proof");
        self.pending.is_empty() && self.barrier.is_none() && proved && !self.exhausted
    }

    pub(crate) fn pending_overflow(&self) -> PendingOverflow {
        self.overflow.clone()
    }

    pub(crate) fn pending_snapshot(&self) -> PendingSource {
        PendingSource {
            sources: self.pending.iter().copied().collect(),
            dirty_generation: self.stamp.dirty,
            overflow: self.overflow.clone(),
            exhausted: self.exhausted(),
        }
    }

    /// The entry for a live notice: one non-Discord key refuses the whole notice before any change,
    /// and an order that cannot name a new dirty generation refuses it without taking it.
    pub(crate) fn admit_pending(&mut self, sources: &[u64]) -> Result<PendingSource, Failure> {
        let mixed = !discord_sources(sources);
        #[cfg(test)]
        let mixed = mixed && !mutant("drop_hm1_batch_namespace_preflight");
        if mixed {
            return Err(Failure::StalePermit);
        }
        let full = self.exhausted || next_generation(self.stamp.dirty).is_none();
        #[cfg(test)]
        let full = full && !mutant("reuse_dirty_generation");
        if full {
            // A refused notice cannot be tracked, so no later scan may prove this order complete.
            #[cfg(test)]
            let latch = !mutant("skip_dirty_exhaustion_latch");
            #[cfg(not(test))]
            let latch = true;
            self.exhausted |= latch;
            return Err(EXHAUSTED);
        }
        Ok(self.pending(sources))
    }

    /// Retries re-record sources this order already checked, so they cannot fail; when the dirty
    /// generation cannot advance the sources still stay pending and the order latches exhausted.
    pub(crate) fn pending(&mut self, sources: &[u64]) -> PendingSource {
        debug_assert!(discord_sources(sources));
        let sources: Vec<_> = sources
            .iter()
            .copied()
            .filter(|s| *s > self.stamp.frontier)
            .collect();
        if !sources.is_empty() {
            self.pending.extend(&sources);
            match next_generation(self.stamp.dirty) {
                Some(dirty) => self.stamp.dirty = dirty,
                None => self.exhausted = true,
            }
        }
        PendingSource {
            sources,
            dirty_generation: self.stamp.dirty,
            overflow: self.overflow.clone(),
            exhausted: self.exhausted(),
        }
    }

    /// Only this pre-fetch snapshot can authorize the returned immutable page.
    pub(crate) fn fetch_ticket(&mut self, epoch: u64) -> Result<FetchTicket, Failure> {
        let overflow = self.overflow.generation();
        if epoch != self.stamp.epoch || overflow == u64::MAX {
            return Err(Failure::StalePermit);
        }
        let scan = next_generation(self.stamp.scan);
        #[cfg(test)]
        let scan = scan.or(mutant("reuse_scan_generation").then_some(0));
        let Some(scan) = scan.filter(|_| !self.exhausted) else {
            self.exhausted = true;
            return Err(EXHAUSTED);
        };
        self.stamp.overflow = overflow;
        self.stamp.scan = scan;
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
        let foreign_horizon = horizon != 0 && !is_discord_key(horizon);
        #[cfg(test)]
        let foreign_horizon = foreign_horizon && !mutant("allow_non_discord_scan_horizon");
        // Sources are sorted and bounded by the horizon, so a Discord horizon bounds them all.
        if !current
            || foreign_horizon
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
        !self.exhausted
            && stamp.provider == self.stamp.provider
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
            // The deferred head bounds a prefix unless the prefix retries that head itself.
            && self.barrier.is_none_or(|b| {
                let before = sources.last().is_none_or(|s| *s <= b);
                let retries_head = sources.binary_search(&b).is_ok();
                #[cfg(test)]
                let (before, retries_head) = (
                    before && !mutant("require_barrier_in_every_prefix"),
                    retries_head && !mutant("barrier_last_source_ceiling"),
                );
                before || retries_head
            });
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
        self.next_scan();
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

/// A stale release or an exhausted order keeps the barrier: neither can prove the sweep current.
pub(crate) fn order_barrier(provider: &ProviderKind, channel: u64) -> bool {
    BARRIERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(provider.as_str().to_owned(), channel))
        .is_some_and(|entry| {
            entry.settled_overflow != Some(entry.order.overflow.generation())
                || entry.order.exhausted
                || !entry.release.current()
        })
}

#[cfg(test)]
pub(crate) fn handback_count_for_test() -> usize {
    BARRIERS.lock().unwrap_or_else(|e| e.into_inner()).len()
}

#[cfg(test)]
pub(crate) fn handback_callback_count_for_test() -> u64 {
    HANDBACK_CALLBACKS.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn note_handback_callback_for_test() {
    HANDBACK_CALLBACKS.fetch_add(1, Ordering::AcqRel);
}

/// Runs a Gate protection change under the handback serializer, so it lands only between callbacks.
pub(in crate::services::discord::input_runtime) fn serialized<T>(change: impl FnOnce() -> T) -> T {
    let _entries = match BARRIERS.try_lock() {
        Ok(entries) => entries,
        Err(_) => {
            #[cfg(test)]
            CONTENDED.with(|hook| hook.borrow_mut().take().map(|hook| hook()));
            BARRIERS.lock().unwrap_or_else(|e| e.into_inner())
        }
    };
    change()
}

// Fires on the waiting thread once a serialized change finds a callback holding the lock.
#[cfg(test)]
thread_local! {
    pub(crate) static CONTENDED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// One lock covers the Gate's own `release` (only fence::modes can supply it) and installation;
/// every refusal precedes it, and a reinstall inherits only a settled same-Gate, same-handle entry.
pub(in crate::services::discord::input_runtime) fn install_handback_snapshot(
    closing: &Closing,
    snapshot: &PendingSource,
    release: impl FnOnce() -> Result<ReleaseProof, Failure>,
) -> Result<(), Failure> {
    let (provider, channel) = (closing.provider().clone(), closing.channel());
    let foreign = snapshot.overflow.provider != provider
        || snapshot.overflow.channel != channel
        || !discord_sources(&snapshot.sources);
    #[cfg(test)]
    let late = mutant("release_before_snapshot_namespace_check");
    #[cfg(not(test))]
    let late = false;
    if foreign && !late {
        return Err(Failure::StalePermit);
    }
    let exhausted = snapshot.exhausted || snapshot.overflow.generation() == u64::MAX;
    #[cfg(test)]
    let exhausted = exhausted && !mutant("drop_exhausted_snapshot_refusal");
    if exhausted {
        return Err(EXHAUSTED);
    }
    let mut entries = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let key = (provider.as_str().to_owned(), channel);
    if let Some(entry) = entries.get(&key) {
        let order = &entry.order;
        let settled = order.pending.is_empty()
            && order.barrier.is_none()
            && !order.exhausted
            && entry.settled_overflow == Some(order.overflow.generation());
        #[cfg(test)]
        let settled = settled || mutant("replace_unsettled_handback");
        let same_handle = Arc::ptr_eq(&order.overflow.generation, &snapshot.overflow.generation);
        #[cfg(test)]
        let same_handle = same_handle || mutant("accept_different_overflow_on_reinstall");
        let inherits = settled
            && same_handle
            && entry.release.names(closing)
            && snapshot.dirty_generation >= order.stamp.dirty;
        if !inherits {
            return Err(Failure::StalePermit);
        }
    }
    #[cfg(test)]
    let late_owner = mutant("owner_allocation_after_release");
    #[cfg(not(test))]
    let late_owner = false;
    let order = if late_owner {
        None
    } else {
        Some(AdmissionOrder::new(provider.clone(), channel, 0, 0)?)
    };
    // Both fallible allocations precede release; failures leave gaps but never reuse an ID.
    let handback = NEXT_HANDBACK
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            #[cfg(test)]
            if mutant("reuse_handback_generation") {
                return Some(value.wrapping_add(1));
            }
            next_generation(value)
        })
        .map_err(|_| EXHAUSTED)?;
    let release = release()?;
    if foreign && late {
        return Err(Failure::StalePermit);
    }
    let mut order = match order {
        Some(order) => order,
        None => AdmissionOrder::new(provider, channel, release.epoch(), 0)?,
    };
    order.stamp.epoch = release.epoch();
    order.pending.extend(&snapshot.sources);
    order.stamp.dirty = snapshot.dirty_generation;
    order.overflow = snapshot.overflow.clone();
    #[cfg(test)]
    if mutant("fresh_overflow_on_release") {
        order.overflow = PendingOverflow::new(order.stamp.provider.clone(), channel);
    }
    order.stamp.handback = Some(handback);
    entries.insert(
        key,
        HandbackEntry {
            release,
            order,
            settled_overflow: None,
        },
    );
    Ok(())
}

// Every port first proves the release still stands; a refusal leaves the entry untouched, and only
// an accepted change re-arms the barrier.
fn with_handback<T>(
    provider: &ProviderKind,
    channel: u64,
    #[cfg(test)] port: &str,
    rearm: bool,
    action: impl FnOnce(&mut AdmissionOrder, u64) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let mut entries = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = entries
        .get_mut(&(provider.as_str().to_owned(), channel))
        .ok_or(Failure::StalePermit)?;
    #[cfg(test)]
    if mutant("mutate_handback_before_validation") {
        entry.settled_overflow = None;
    }
    let current = entry.release.current();
    #[cfg(test)]
    let current =
        current || mutant("drop_current_gate") || mutant(&format!("drop_current_gate_on_{port}"));
    if entry.order.stamp.provider != *provider || !current {
        return Err(Failure::StalePermit);
    }
    let result = action(&mut entry.order, entry.release.epoch());
    if rearm && result.is_ok() {
        entry.settled_overflow = None;
    }
    result
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
    with_handback(
        provider,
        channel,
        #[cfg(test)]
        "pending",
        true,
        |order, _| order.admit_pending(sources),
    )
}

pub(crate) fn handback_fetch_ticket(
    provider: &ProviderKind,
    channel: u64,
) -> Result<FetchTicket, Failure> {
    with_handback(
        provider,
        channel,
        #[cfg(test)]
        "ticket",
        true,
        |order, epoch| order.fetch_ticket(epoch),
    )
}

pub(crate) fn handback_begin_scan(
    ticket: FetchTicket,
    sources: Vec<u64>,
    horizon: u64,
    complete_fetch: bool,
) -> Result<ScanCapability, Failure> {
    let (provider, channel) = (ticket.stamp.provider.clone(), ticket.stamp.channel);
    with_handback(
        &provider,
        channel,
        #[cfg(test)]
        "begin",
        true,
        |order, epoch| order.begin_scan(ticket, epoch, sources, horizon, complete_fetch),
    )
}

/// Observation only: it changes nothing and lends no authority to run outside the serializer.
pub(crate) fn handback_permits_sources(
    cap: &ScanCapability,
    sources: &[u64],
) -> Result<(), Failure> {
    with_handback(
        &cap.ticket.stamp.provider,
        cap.ticket.stamp.channel,
        #[cfg(test)]
        "permits",
        false,
        |order, epoch| order.permits_sources(cap, epoch, sources),
    )
}

/// The synchronous durable callback shares the entry lock with validation and settlement.
/// Fetch and materialization finish before this call; async Legacy ports need a serialized loan.
/// The outer error means the callback never ran; once it ran, its result and the settlement are
/// both returned, and any failure keeps every source as a retry obligation.
pub(crate) fn handback_commit_from_scan<T>(
    mut cap: ScanCapability,
    sources: &[u64],
    commit: impl FnOnce() -> Result<T, Failure>,
) -> Result<HandbackExecution<T>, Failure> {
    let (provider, channel) = (cap.ticket.stamp.provider.clone(), cap.ticket.stamp.channel);
    with_handback(
        &provider,
        channel,
        #[cfg(test)]
        "commit",
        true,
        |order, epoch| {
            order.permits_sources(&cap, epoch, sources)?;
            #[cfg(test)]
            note_handback_callback_for_test();
            let outcome = commit();
            // permits_sources refuses an empty range, so the head exists.
            let settlement = match &outcome {
                Ok(_) => order.settle_sources(&mut cap, epoch, sources),
                Err(_) => order.defer(&cap, epoch, sources[0]),
            };
            let settled = outcome.is_ok() && settlement.is_ok();
            let retain = !settled;
            #[cfg(test)]
            let retain = retain && !mutant("drop_retry_sources_after_execution");
            if retain {
                order.pending(sources);
            }
            #[cfg(test)]
            let outcome = match mutant("overwrite_persistence_with_deferral_error") {
                true if outcome.is_err() => settlement.and(outcome),
                _ => outcome,
            };
            #[cfg(test)]
            if mutant("collapse_executed_result_to_error") {
                settlement?;
            }
            #[cfg(test)]
            let settled = settled || mutant("return_capability_after_failed_settlement");
            Ok(HandbackExecution {
                outcome,
                settlement,
                next: settled.then_some(cap),
            })
        },
    )
}

pub(crate) fn handback_settle_sources(
    mut cap: ScanCapability,
    sources: &[u64],
) -> Result<ScanCapability, Failure> {
    let (provider, channel) = (cap.ticket.stamp.provider.clone(), cap.ticket.stamp.channel);
    with_handback(
        &provider,
        channel,
        #[cfg(test)]
        "settle",
        true,
        |order, epoch| {
            order.settle_sources(&mut cap, epoch, sources)?;
            Ok(cap)
        },
    )
}

pub(crate) fn handback_defer(cap: &ScanCapability, source: u64) -> Result<(), Failure> {
    with_handback(
        &cap.ticket.stamp.provider,
        cap.ticket.stamp.channel,
        #[cfg(test)]
        "defer",
        true,
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
    with_handback(
        &provider,
        channel,
        #[cfg(test)]
        "complete",
        true,
        |order, epoch| order.complete(cap, epoch),
    )
}

/// A missing entry or a stale release is refused, never reported as an already settled no-op.
pub(crate) fn settle_handback(cap: ValidatedOrderCapability) -> Result<bool, Failure> {
    let stamp = &cap.0.ticket.stamp;
    let key = (stamp.provider.as_str().to_owned(), stamp.channel);
    let mut entries = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = entries.get_mut(&key).ok_or(Failure::StalePermit)?;
    let current = entry.release.current();
    #[cfg(test)]
    let current = current || mutant("drop_current_gate_on_final");
    let valid = current
        && entry.order.completed(&cap.0, entry.release.epoch())
        && cap.0.ticket.post_pending_fetch
        && stamp.handback.is_some();
    #[cfg(test)]
    let valid = valid || mutant("drop_capability");
    if !valid {
        return Err(Failure::StalePermit);
    }
    if entry.settled_overflow == Some(stamp.overflow) {
        return Ok(false);
    }
    // The settlement retires this capability by advancing scan, so it cannot settle at the limit.
    let advances = next_generation(entry.order.stamp.scan).is_some();
    #[cfg(test)]
    let advances = advances || mutant("success_after_exhausted_settlement");
    if !advances {
        entry.order.exhausted = true;
        return Err(EXHAUSTED);
    }
    // Keep the shared handle: overflow racing this acknowledgement re-arms the barrier.
    entry.settled_overflow = Some(stamp.overflow);
    entry.order.next_scan();
    Ok(true)
}

#[cfg(test)]
fn mutant(name: &str) -> bool {
    crate::services::discord::input_runtime::supervisor::mutant(name)
}

#[cfg(test)]
#[path = "ordering_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "g1b_ordering_tests.rs"]
pub(crate) mod g1b_tests;
