#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeferredIdleQueueKickoffProfile {
    Normal,
    ImmediateOnce,
}

impl DeferredIdleQueueKickoffProfile {
    pub(super) fn initial_presleep(self, normal: std::time::Duration) -> std::time::Duration {
        match self {
            Self::Normal => normal,
            Self::ImmediateOnce => std::time::Duration::ZERO,
        }
    }

    pub(super) fn is_immediate(self) -> bool {
        matches!(self, Self::ImmediateOnce)
    }
}

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

    pub(super) fn try_begin_releasing(&self, expected: DeferredIdleQueuePhase) -> bool {
        self.phase
            .compare_exchange(
                expected as u8,
                DeferredIdleQueuePhase::Releasing as u8,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    pub(super) fn request_checkpoint(&self) -> (u64, u64) {
        (
            self.requested_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            self.coalesced_epoch
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    pub(super) fn exactly_owned_by(&self, current: &std::sync::Arc<Self>) -> bool {
        std::ptr::eq(std::sync::Arc::as_ptr(current), self)
            && current.worker_epoch == self.worker_epoch
    }

    pub(super) fn finish_retire_exact(
        &self,
        channels: &dashmap::DashMap<
            serenity::all::ChannelId,
            std::sync::Arc<DeferredIdleQueueEntry>,
        >,
        channel_id: serenity::all::ChannelId,
        expected_phase: DeferredIdleQueuePhase,
        request_checkpoint: (u64, u64),
    ) -> bool {
        let owns_entry = channels
            .get(&channel_id)
            .is_some_and(|current| self.exactly_owned_by(current.value()));
        if !owns_entry || self.request_checkpoint() != request_checkpoint {
            self.set_phase(expected_phase);
            return false;
        }
        true
    }

    pub(super) fn accept_schedule(&self, immediate: bool) -> bool {
        if immediate {
            return self.request_immediate();
        }
        loop {
            let phase = self.phase();
            if phase == DeferredIdleQueuePhase::Releasing {
                return false;
            }
            saturating_increment(&self.coalesced_epoch);
            let current = self.phase();
            if current == phase {
                return true;
            }
            if current == DeferredIdleQueuePhase::Releasing {
                return false;
            }
        }
    }

    pub(super) fn request_immediate(&self) -> bool {
        loop {
            let phase = self.phase();
            match phase {
                DeferredIdleQueuePhase::InitialDelay | DeferredIdleQueuePhase::BackstopWait => {
                    loop {
                        let requested = self
                            .requested_epoch
                            .load(std::sync::atomic::Ordering::Acquire);
                        if requested == u64::MAX {
                            let observed = self
                                .observed_epoch
                                .load(std::sync::atomic::Ordering::Acquire);
                            if observed != requested {
                                self.notify.notify_one();
                                break;
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
                                break;
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
                            break;
                        }
                    }
                }
                DeferredIdleQueuePhase::Kicking | DeferredIdleQueuePhase::BackstopKicking => {
                    saturating_increment(&self.coalesced_epoch);
                }
                DeferredIdleQueuePhase::Releasing => return false,
            }

            let current = self.phase();
            if current == phase {
                return true;
            }
            if current == DeferredIdleQueuePhase::Releasing {
                return false;
            }
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

pub(super) async fn wait_for_deferred_idle_queue_delay(
    entry: &DeferredIdleQueueEntry,
    delay: std::time::Duration,
) -> DeferredIdleQueueWaitOutcome {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if entry.observe_pending_request() {
            return DeferredIdleQueueWaitOutcome::Immediate;
        }
        let notified = entry.notify.notified();
        if entry.observe_pending_request() {
            return DeferredIdleQueueWaitOutcome::Immediate;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return DeferredIdleQueueWaitOutcome::Deadline;
            }
            _ = notified => {
                if entry.observe_pending_request() {
                    return DeferredIdleQueueWaitOutcome::Immediate;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeferredIdleQueueWaitOutcome {
    Deadline,
    Immediate,
}
