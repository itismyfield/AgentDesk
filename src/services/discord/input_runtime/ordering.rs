//! Memory-only, supervisor-owned oldest-first scan authority; hints never prove durable receipt.
use super::fence::{Failure, modes};
use crate::services::provider::ProviderKind;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct PendingSource {
    pub(crate) sources: Vec<u64>,
    pub(crate) dirty_generation: u64,
}

pub(crate) struct AdmissionOrder {
    provider: ProviderKind,
    channel: u64,
    owner: u64,
    epoch: u64,
    scan: u64,
    dirty: u64,
    frontier: u64,
    pending: BTreeSet<u64>,
    barrier: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ScanCapability {
    provider: ProviderKind,
    channel: u64,
    owner: u64,
    epoch: u64,
    scan: u64,
    dirty: u64,
    frontier: u64,
    sources: VecDeque<u64>,
    horizon: u64,
    complete_fetch: bool,
    post_pending_fetch: bool,
    handback: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ValidatedOrderCapability(ScanCapability);

impl ValidatedOrderCapability {
    pub(crate) fn scope(&self) -> (&ProviderKind, u64, u64) {
        (&self.0.provider, self.0.channel, self.0.owner)
    }
}

impl Drop for AdmissionOrder {
    fn drop(&mut self) {
        modes::release_order_barrier(&self.provider, self.channel, self.owner);
    }
}

impl AdmissionOrder {
    pub(crate) fn new(provider: ProviderKind, channel: u64, epoch: u64, frontier: u64) -> Self {
        Self {
            provider,
            channel,
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            epoch,
            scan: 0,
            dirty: 0,
            frontier,
            pending: BTreeSet::new(),
            barrier: None,
        }
    }

    pub(crate) fn invalidate(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.scan += 1;
    }

    pub(crate) fn pending(&mut self, sources: &[u64]) -> PendingSource {
        let sources: Vec<_> = sources
            .iter()
            .copied()
            .filter(|s| *s > self.frontier)
            .collect();
        if !sources.is_empty() {
            self.pending.extend(&sources);
            self.dirty += 1;
        }
        PendingSource {
            sources,
            dirty_generation: self.dirty,
        }
    }

    pub(crate) fn begin_scan(
        &mut self,
        epoch: u64,
        sources: Vec<u64>,
        horizon: u64,
        complete_fetch: bool,
    ) -> Result<ScanCapability, Failure> {
        if epoch != self.epoch
            || horizon < self.frontier
            || sources.first().is_some_and(|s| *s <= self.frontier)
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
        let handback = modes::claim_order_barrier(&self.provider, self.channel, self.owner)?;
        self.scan += 1;
        Ok(ScanCapability {
            provider: self.provider.clone(),
            channel: self.channel,
            owner: self.owner,
            epoch,
            scan: self.scan,
            dirty: self.dirty,
            frontier: self.frontier,
            sources: sources.into(),
            horizon,
            complete_fetch,
            post_pending_fetch: self.pending.is_empty() && self.barrier.is_none(),
            handback,
        })
    }

    fn current(&self, cap: &ScanCapability, epoch: u64) -> bool {
        let epoch_matches = epoch == self.epoch && cap.epoch == epoch;
        #[cfg(test)]
        let epoch_matches = epoch_matches || super::mutant("drop_epoch");
        cap.provider == self.provider
            && cap.channel == self.channel
            && cap.owner == self.owner
            && epoch_matches
            && cap.scan == self.scan
            && cap.dirty == self.dirty
            && cap.frontier == self.frontier
    }

    pub(crate) fn permits(
        &self,
        cap: &ScanCapability,
        epoch: u64,
        source: u64,
    ) -> Result<(), Failure> {
        if !self.current(cap, epoch)
            || cap.sources.front() != Some(&source)
            || self.barrier.is_some_and(|barrier| source > barrier)
        {
            return Err(Failure::StalePermit);
        }
        Ok(())
    }

    /// Called only after positive durable responsibility or final non-target settlement.
    pub(crate) fn settle(
        &mut self,
        cap: &mut ScanCapability,
        epoch: u64,
        source: u64,
    ) -> Result<(), Failure> {
        self.permits(cap, epoch, source)?;
        cap.sources.pop_front();
        self.pending.remove(&source);
        self.frontier = source;
        cap.frontier = source;
        if self.barrier == Some(source) {
            self.barrier = None;
        }
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
        let retain = retain && !super::mutant("drop_deferred_pending");
        if retain {
            self.pending.insert(source);
        }
        self.barrier = Some(source);
        self.scan += 1;
        Ok(())
    }

    pub(crate) fn complete(
        &self,
        cap: ScanCapability,
        epoch: u64,
    ) -> Result<ValidatedOrderCapability, Failure> {
        if !self.current(&cap, epoch)
            || !cap.complete_fetch
            || !cap.sources.is_empty()
            || !self.pending.is_empty()
            || self.barrier.is_some()
            || cap.horizon < self.frontier
        {
            return Err(Failure::StalePermit);
        }
        Ok(ValidatedOrderCapability(cap))
    }

    pub(crate) fn validates_handback(
        &self,
        cap: &ValidatedOrderCapability,
        epoch: u64,
        generation: u64,
    ) -> bool {
        self.current(&cap.0, epoch)
            && cap.0.handback == Some(generation)
            && cap.0.complete_fetch
            && cap.0.post_pending_fetch
            && cap.0.sources.is_empty()
            && self.pending.is_empty()
            && self.barrier.is_none()
            && cap.0.horizon >= self.frontier
    }
}

#[cfg(test)]
#[path = "ordering_tests.rs"]
mod tests;
