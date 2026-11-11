use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serenity::model::id::ChannelId;
use tokio::sync::Notify;

#[derive(Default)]
struct Observation {
    completed_fires: usize,
    backstop_waiting: bool,
    listener_reconciles: usize,
    owner_wakes: usize,
}

static OBSERVATIONS: LazyLock<Mutex<HashMap<ChannelId, Observation>>> =
    LazyLock::new(Mutex::default);

type OwnerWakeHook = Arc<dyn Fn() + Send + Sync>;
static OWNER_WAKE_HOOKS: LazyLock<Mutex<HashMap<ChannelId, OwnerWakeHook>>> =
    LazyLock::new(Mutex::default);

pub(in crate::services::discord) fn set_owner_wake_for_tests(
    channel: ChannelId,
    hook: Option<OwnerWakeHook>,
) -> Option<OwnerWakeHook> {
    let mut hooks = OWNER_WAKE_HOOKS.lock().unwrap();
    match hook {
        Some(hook) => hooks.insert(channel, hook),
        None => hooks.remove(&channel),
    }
}

pub(in crate::services::discord) fn run_owner_wake_for_tests(channel: ChannelId) -> bool {
    let hook = OWNER_WAKE_HOOKS.lock().unwrap().get(&channel).cloned();
    if let Some(hook) = hook {
        hook();
        true
    } else {
        false
    }
}

pub(in crate::services::discord) fn record_owner_wake(channel: ChannelId) {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(channel)
        .or_default()
        .owner_wakes += 1;
}

pub(in crate::services::discord) fn owner_wakes(channel: ChannelId) -> usize {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&channel)
        .map_or(0, |state| state.owner_wakes)
}

pub(in crate::services::discord) fn completed_fires(channel: ChannelId) -> usize {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&channel)
        .map_or(0, |state| state.completed_fires)
}

pub(in crate::services::discord) fn backstop_waiting(channel: ChannelId) -> bool {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&channel)
        .is_some_and(|state| state.backstop_waiting)
}

pub(in crate::services::discord) fn listener_completed_reconciles(channel: ChannelId) -> usize {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&channel)
        .map_or(0, |state| state.listener_reconciles)
}

pub(super) fn completed_cycle(channel: ChannelId) {
    if let Some(state) = completion_state(channel) {
        state.lock().unwrap().backstop_cycles += 1;
        completion_notify(channel);
    }
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(channel)
        .or_default()
        .completed_fires += 1;
}

pub(super) fn set_backstop_waiting(channel: ChannelId, waiting: bool) {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(channel)
        .or_default()
        .backstop_waiting = waiting;
}

pub(super) fn set_listener_ready(channel: ChannelId) {
    if let Some(state) = completion_state(channel) {
        state.lock().unwrap().listener_ready = true;
        completion_notify(channel);
    }
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(channel)
        .or_default()
        .listener_reconciles += 1;
}

#[derive(Default)]
struct Barrier {
    entered: Notify,
    resume: Notify,
    released: AtomicBool,
}

type BarrierKey = (&'static str, ChannelId);
static BARRIERS: LazyLock<Mutex<HashMap<BarrierKey, Arc<Barrier>>>> = LazyLock::new(Mutex::default);

struct InstalledBarrier {
    key: BarrierKey,
    barrier: Arc<Barrier>,
}

impl InstalledBarrier {
    fn install(site: &'static str, channel: ChannelId) -> Self {
        let key = (site, channel);
        let barrier = Arc::new(Barrier::default());
        let mut barriers = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!barriers.contains_key(&key));
        barriers.insert(key, barrier.clone());
        Self { key, barrier }
    }

    async fn wait(&self, budget: Duration) -> bool {
        tokio::time::timeout(budget, self.barrier.entered.notified())
            .await
            .is_ok()
    }

    fn release(&self) {
        self.barrier.released.store(true, Ordering::Release);
        self.barrier.resume.notify_waiters();
    }
}

impl Drop for InstalledBarrier {
    fn drop(&mut self) {
        self.release();
        let mut barriers = BARRIERS.lock().unwrap_or_else(|e| e.into_inner());
        if barriers
            .get(&self.key)
            .is_some_and(|barrier| Arc::ptr_eq(barrier, &self.barrier))
        {
            barriers.remove(&self.key);
        }
    }
}

pub(in crate::services::discord) struct ShutdownRace(InstalledBarrier);

impl ShutdownRace {
    pub(in crate::services::discord) fn install(channel: ChannelId) -> Self {
        Self(InstalledBarrier::install("before_shutdown", channel))
    }
    pub(in crate::services::discord) async fn wait(&self, budget: Duration) -> bool {
        self.0.wait(budget).await
    }
    pub(in crate::services::discord) fn release(&self) {
        self.0.release();
    }
}

async fn pause(site: &'static str, channel: ChannelId) {
    let barrier = BARRIERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(site, channel))
        .cloned();
    if let Some(barrier) = barrier {
        let resumed = barrier.resume.notified();
        tokio::pin!(resumed);
        resumed.as_mut().enable();
        if barrier.released.load(Ordering::Acquire) {
            return;
        }
        barrier.entered.notify_one();
        resumed.await;
    }
}

pub(super) async fn pause_before_shutdown(channel: ChannelId) {
    pause("before_shutdown", channel).await;
}

#[derive(Default)]
struct CompletionState {
    listener_ready: bool,
    listener_events: usize,
    backstop_cycles: usize,
    pending_requests: usize,
    post_enqueue_snapshots: Vec<usize>,
    owner_boot_holds: Vec<u64>,
}

#[derive(Default)]
struct Completion {
    state: Arc<Mutex<CompletionState>>,
    changed: Notify,
}

static COMPLETIONS: LazyLock<Mutex<HashMap<ChannelId, Arc<Completion>>>> =
    LazyLock::new(Mutex::default);

fn completion_state(channel: ChannelId) -> Option<Arc<Mutex<CompletionState>>> {
    COMPLETIONS
        .lock()
        .unwrap()
        .get(&channel)
        .map(|c| c.state.clone())
}

fn completion_notify(channel: ChannelId) {
    if let Some(completion) = COMPLETIONS.lock().unwrap().get(&channel) {
        completion.changed.notify_waiters();
    }
}

#[derive(Default)]
pub(super) struct DeferredRequestAcks {
    pub(super) pending: Vec<DeferredRequestAck>,
    pub(super) evaluating: Vec<DeferredRequestAck>,
}

pub(super) struct DeferredRequestAck {
    channel: ChannelId,
    state: Arc<Mutex<CompletionState>>,
}

impl DeferredRequestAck {
    pub(super) fn complete(self) {
        self.state.lock().unwrap().pending_requests -= 1;
        completion_notify(self.channel);
    }
}

pub(super) fn deferred_request_started(channel: ChannelId) -> Option<DeferredRequestAck> {
    completion_state(channel).map(|state| {
        state.lock().unwrap().pending_requests += 1;
        DeferredRequestAck { channel, state }
    })
}

pub(super) fn listener_event_completed(channel: ChannelId) {
    if let Some(state) = completion_state(channel) {
        state.lock().unwrap().listener_events += 1;
        completion_notify(channel);
    }
}

/// Observes completed processing, including requests coalesced while an earlier request runs.
pub(in crate::services::discord) struct KickoffCompletion {
    channel: ChannelId,
    completion: Arc<Completion>,
}

impl KickoffCompletion {
    pub(in crate::services::discord) fn install(channel: ChannelId) -> Self {
        let completion = Arc::new(Completion::default());
        assert!(
            COMPLETIONS
                .lock()
                .unwrap()
                .insert(channel, completion.clone())
                .is_none()
        );
        Self {
            channel,
            completion,
        }
    }

    async fn until(&self, budget: Duration, ready: impl Fn(&CompletionState) -> bool) {
        tokio::time::timeout(budget, async {
            loop {
                let changed = self.completion.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if ready(&self.completion.state.lock().unwrap()) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("queue kickoff processing completed");
    }

    pub(in crate::services::discord) async fn owner_boot_holds(
        &self,
        message: u64,
        count: usize,
        budget: Duration,
    ) {
        self.until(budget, |state| {
            state
                .owner_boot_holds
                .iter()
                .filter(|&&id| id == message)
                .count()
                >= count
        })
        .await;
    }

    pub(in crate::services::discord) async fn post_enqueue_snapshots(
        &self,
        queue_len: usize,
        count: usize,
        budget: Duration,
    ) {
        self.until(budget, |state| {
            state
                .post_enqueue_snapshots
                .iter()
                .filter(|&&len| len == queue_len)
                .count()
                >= count
        })
        .await;
    }

    pub(in crate::services::discord) async fn listener_ready(&self, budget: Duration) {
        self.until(budget, |state| state.listener_ready).await;
    }

    pub(in crate::services::discord) async fn processed(
        &self,
        listener_events: usize,
        budget: Duration,
    ) {
        self.until(budget, |state| {
            state.listener_events >= listener_events && state.pending_requests == 0
        })
        .await;
    }

    pub(in crate::services::discord) async fn backstop_cycles(
        &self,
        cycles: usize,
        budget: Duration,
    ) {
        self.until(budget, |state| state.backstop_cycles >= cycles)
            .await;
    }

    pub(in crate::services::discord) fn pending_requests(&self) -> usize {
        self.completion.state.lock().unwrap().pending_requests
    }
}

impl Drop for KickoffCompletion {
    fn drop(&mut self) {
        let mut completions = COMPLETIONS.lock().unwrap();
        if completions
            .get(&self.channel)
            .is_some_and(|c| Arc::ptr_eq(c, &self.completion))
        {
            completions.remove(&self.channel);
        }
    }
}

/// Records the initial snapshot only after its real generic kick task finished.
pub(super) fn post_enqueue_snapshot_finished(channel: ChannelId, queue_len: usize) {
    if let Some(state) = completion_state(channel) {
        state.lock().unwrap().post_enqueue_snapshots.push(queue_len);
        completion_notify(channel);
    }
}

pub(in crate::services::discord) fn owner_boot_hold_finished(channel: ChannelId, message: u64) {
    if let Some(state) = completion_state(channel) {
        state.lock().unwrap().owner_boot_holds.push(message);
        completion_notify(channel);
    }
}
