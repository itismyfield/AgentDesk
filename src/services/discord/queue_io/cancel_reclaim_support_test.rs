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
}

static OBSERVATIONS: LazyLock<Mutex<HashMap<ChannelId, Observation>>> =
    LazyLock::new(Mutex::default);

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

pub(in crate::services::discord) struct BeforeAdmitRace(InstalledBarrier);

impl BeforeAdmitRace {
    pub(in crate::services::discord) fn install(channel: ChannelId) -> Self {
        Self(InstalledBarrier::install("before_admit", channel))
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

pub(super) async fn pause_before_admit(channel: ChannelId) {
    pause("before_admit", channel).await;
}
