#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum DeferredIdleQueuePhase {
    InitialDelay,
    Kicking,
    BackstopWait,
    BackstopKicking,
    Releasing,
}

impl DeferredIdleQueuePhase {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::InitialDelay,
            1 => Self::Kicking,
            2 => Self::BackstopWait,
            3 => Self::BackstopKicking,
            _ => Self::Releasing,
        }
    }
}

/// Process-local ownership and wake state for the only deferred queue worker of
/// a channel. Epochs are authoritative; `Notify` is only a waiter hint and may
/// retain a stale permit.
pub(in crate::services) struct DeferredIdleQueueEntry {
    pub(super) worker_epoch: u64,
    pub(super) requested_epoch: std::sync::atomic::AtomicU64,
    pub(super) observed_epoch: std::sync::atomic::AtomicU64,
    pub(super) coalesced_epoch: std::sync::atomic::AtomicU64,
    pub(super) phase: std::sync::atomic::AtomicU8,
    pub(super) notify: tokio::sync::Notify,
}

static NEXT_DEFERRED_IDLE_QUEUE_WORKER_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

impl DeferredIdleQueueEntry {
    pub(super) fn new(immediate: bool) -> Self {
        Self {
            worker_epoch: next_deferred_idle_queue_worker_epoch(),
            requested_epoch: std::sync::atomic::AtomicU64::new(u64::from(immediate)),
            observed_epoch: std::sync::atomic::AtomicU64::new(0),
            coalesced_epoch: std::sync::atomic::AtomicU64::new(0),
            phase: std::sync::atomic::AtomicU8::new(DeferredIdleQueuePhase::InitialDelay as u8),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub(super) fn phase(&self) -> DeferredIdleQueuePhase {
        DeferredIdleQueuePhase::from_u8(self.phase.load(std::sync::atomic::Ordering::Acquire))
    }

    pub(super) fn set_phase(&self, phase: DeferredIdleQueuePhase) {
        self.phase
            .store(phase as u8, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn request_immediate(&self) -> bool {
        match self.phase() {
            DeferredIdleQueuePhase::InitialDelay | DeferredIdleQueuePhase::BackstopWait => loop {
                let requested = self
                    .requested_epoch
                    .load(std::sync::atomic::Ordering::Acquire);
                if requested == u64::MAX {
                    let observed = self
                        .observed_epoch
                        .load(std::sync::atomic::Ordering::Acquire);
                    if observed != requested {
                        self.notify.notify_one();
                        return true;
                    }
                    if self
                        .requested_epoch
                        .compare_exchange(
                            requested,
                            1,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.observed_epoch
                            .store(0, std::sync::atomic::Ordering::Release);
                        self.notify.notify_one();
                        return true;
                    }
                    continue;
                }
                if self
                    .requested_epoch
                    .compare_exchange_weak(
                        requested,
                        requested + 1,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok()
                {
                    self.notify.notify_one();
                    return true;
                }
            },
            DeferredIdleQueuePhase::Kicking | DeferredIdleQueuePhase::BackstopKicking => {
                saturating_increment(&self.coalesced_epoch);
                true
            }
            DeferredIdleQueuePhase::Releasing => false,
        }
    }

    pub(super) fn observe_pending_request(&self) -> bool {
        let requested = self
            .requested_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let observed = self
            .observed_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if requested == observed {
            return false;
        }
        self.observed_epoch
            .store(requested, std::sync::atomic::Ordering::Release);
        true
    }

    pub(super) fn coalesce_requests_during_kick(&self) {
        let requested = self
            .requested_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        self.observed_epoch
            .store(requested, std::sync::atomic::Ordering::Release);
    }
}

fn next_deferred_idle_queue_worker_epoch() -> u64 {
    NEXT_DEFERRED_IDLE_QUEUE_WORKER_EPOCH
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |epoch| epoch.checked_add(1),
        )
        .unwrap_or(u64::MAX)
}

fn saturating_increment(counter: &std::sync::atomic::AtomicU64) {
    let _ = counter.fetch_update(
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
        |value| Some(value.saturating_add(1)),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeferredIdleQueueWaitOutcome {
    Deadline,
    Immediate,
}
