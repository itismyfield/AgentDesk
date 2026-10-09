//! A channel's delegated home runs one generation at a time: each start or stop is reserved under a
//! short lock and carried out by a task the supervisor owns, whoever stops waiting for it.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;

pub type Generation = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Sigterm,
    BackendExited,
    CommittedRestart,
    Replaced,
    RowGone,
}

/// How a bundle's stop ended: every part joined, or a part whose end was not confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    Joined,
    Stuck(String),
}

/// What one generation runs for a channel; it ends only through the supervisor.
pub trait HomeBundle: Send + 'static {
    fn stop_and_join(self, reason: StopReason) -> impl Future<Output = Settled> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting(Generation),
    Running(Generation),
    Stopping(Generation),
    /// An end that was not confirmed: no later generation starts.
    Blocked(Generation, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another generation is still starting or stopping.
    Busy(Phase),
    Blocked(Generation, String),
    /// The build failed; the channel is free again.
    Failed(String),
    /// A stop arrived while it started, so it was stopped instead of run.
    Stopped(Generation),
    /// The transition ended without a result.
    Lost,
}

enum Slot<B> {
    Starting {
        generation: Generation,
        stop: Option<StopReason>,
        done: watch::Receiver<bool>,
    },
    Running {
        generation: Generation,
        bundle: B,
    },
    Stopping {
        generation: Generation,
        done: watch::Receiver<bool>,
    },
    Blocked {
        generation: Generation,
        detail: String,
    },
}

impl<B> Slot<B> {
    fn phase(&self) -> Phase {
        match self {
            Self::Starting { generation, .. } => Phase::Starting(*generation),
            Self::Running { generation, .. } => Phase::Running(*generation),
            Self::Stopping { generation, .. } => Phase::Stopping(*generation),
            Self::Blocked { generation, detail } => Phase::Blocked(*generation, detail.clone()),
        }
    }

    fn generation(&self) -> Generation {
        match self {
            Self::Starting { generation, .. }
            | Self::Running { generation, .. }
            | Self::Stopping { generation, .. }
            | Self::Blocked { generation, .. } => *generation,
        }
    }

    fn settled(generation: Generation, settled: Settled) -> Option<Self> {
        match settled {
            Settled::Joined => None,
            Settled::Stuck(detail) => Some(Self::Blocked { generation, detail }),
        }
    }
}

pub struct Supervisor<B> {
    slots: Mutex<BTreeMap<u64, Slot<B>>>,
    next: AtomicU64,
}

impl<B> Default for Supervisor<B> {
    fn default() -> Self {
        Self {
            slots: Mutex::default(),
            next: AtomicU64::new(0),
        }
    }
}

/// Ends a transition: the slot it reserved becomes `slot` unless a later generation holds it,
/// then waiters are woken. Dropped unfinished, as by a panic, it leaves the channel blocked.
struct Finish<B: HomeBundle> {
    supervisor: Arc<Supervisor<B>>,
    channel: u64,
    generation: Generation,
    done: Option<watch::Sender<bool>>,
}

impl<B: HomeBundle> Finish<B> {
    fn finish(mut self, slot: Option<Slot<B>>) {
        self.apply(slot);
    }

    fn apply(&mut self, slot: Option<Slot<B>>) {
        let Some(done) = self.done.take() else {
            return;
        };
        self.supervisor.with_slots(|slots| {
            let ours = slots.get(&self.channel).map(Slot::generation) == Some(self.generation);
            match slot {
                _ if !ours => {}
                Some(slot) => {
                    slots.insert(self.channel, slot);
                }
                None => {
                    slots.remove(&self.channel);
                }
            }
        });
        done.send_replace(true);
    }
}

impl<B: HomeBundle> Drop for Finish<B> {
    fn drop(&mut self) {
        let generation = self.generation;
        let detail = "transition ended without a result".to_owned();
        self.apply(Some(Slot::Blocked { generation, detail }));
    }
}

async fn finished(mut done: watch::Receiver<bool>) {
    let _ = done.wait_for(|done| *done).await;
}

impl<B: HomeBundle> Supervisor<B> {
    fn with_slots<R>(&self, use_slots: impl FnOnce(&mut BTreeMap<u64, Slot<B>>) -> R) -> R {
        use_slots(&mut self.slots.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn phase(&self, channel: u64) -> Option<Phase> {
        self.with_slots(|slots| slots.get(&channel).map(Slot::phase))
    }

    /// Reserves the channel's next generation now, before the returned future is polled; a
    /// running one is stopped and joined before `build` runs. Starting or stopping, it is `Busy`.
    pub fn start<F, Fut>(
        self: &Arc<Self>,
        channel: u64,
        build: F,
    ) -> impl Future<Output = Result<Generation, Refused>> + Send + 'static
    where
        F: FnOnce(Generation) -> Fut + Send + 'static,
        Fut: Future<Output = Result<B, String>> + Send + 'static,
    {
        let (done, waiting) = watch::channel(false);
        let reserved = self.with_slots(|slots| {
            match slots.get(&channel) {
                Some(Slot::Blocked { generation, detail }) => {
                    return Err(Refused::Blocked(*generation, detail.clone()));
                }
                Some(slot @ (Slot::Starting { .. } | Slot::Stopping { .. })) => {
                    return Err(Refused::Busy(slot.phase()));
                }
                Some(Slot::Running { .. }) | None => {}
            }
            let generation = self.next.fetch_add(1, Ordering::SeqCst) + 1;
            let starting = Slot::Starting {
                generation,
                stop: None,
                done: waiting,
            };
            let old = match slots.insert(channel, starting) {
                Some(Slot::Running { bundle, .. }) => Some(bundle),
                _ => None,
            };
            Ok((generation, old))
        });
        let task = reserved.map(|(generation, old)| {
            let finish = Finish {
                supervisor: Arc::clone(self),
                channel,
                generation,
                done: Some(done),
            };
            tokio::spawn(Self::transition(finish, old, build))
        });
        async move { task?.await.unwrap_or(Err(Refused::Lost)) }
    }

    async fn transition<F, Fut>(
        finish: Finish<B>,
        old: Option<B>,
        build: F,
    ) -> Result<Generation, Refused>
    where
        F: FnOnce(Generation) -> Fut,
        Fut: Future<Output = Result<B, String>>,
    {
        let (channel, generation) = (finish.channel, finish.generation);
        if let Some(old) = old
            && let Settled::Stuck(detail) = old.stop_and_join(StopReason::Replaced).await
        {
            finish.finish(Slot::settled(generation, Settled::Stuck(detail.clone())));
            return Err(Refused::Blocked(generation, detail));
        }
        let bundle = match build(generation).await {
            Ok(bundle) => bundle,
            Err(detail) => {
                finish.finish(None);
                return Err(Refused::Failed(detail));
            }
        };
        // Runs only while this generation holds the slot and no stop arrived meanwhile.
        let stopped = finish.supervisor.with_slots(|slots| {
            let stop = match slots.get(&channel) {
                Some(Slot::Starting {
                    generation: ours,
                    stop,
                    ..
                }) if *ours == generation => *stop,
                _ => Some(StopReason::Replaced),
            };
            let Some(reason) = stop else {
                slots.insert(channel, Slot::Running { generation, bundle });
                return None;
            };
            Some((reason, bundle))
        });
        let Some((reason, bundle)) = stopped else {
            let mut finish = finish;
            if let Some(done) = finish.done.take() {
                done.send_replace(true);
            }
            return Ok(generation);
        };
        let settled = bundle.stop_and_join(reason).await;
        finish.finish(Slot::settled(generation, settled));
        Err(Refused::Stopped(generation))
    }

    /// Stops the channel's running generation and waits until it settled; with `only`, a
    /// generation other than that one is left alone. Returns the channel's phase afterwards.
    pub fn stop(
        self: &Arc<Self>,
        channel: u64,
        only: Option<Generation>,
        reason: StopReason,
    ) -> impl Future<Output = Option<Phase>> + Send + 'static {
        enum Next<B> {
            Nothing,
            Wait(watch::Receiver<bool>),
            Stop(Generation, B, watch::Sender<bool>),
        }
        let next = self.with_slots(|slots| {
            let Some(slot) = slots.get_mut(&channel) else {
                return Next::Nothing;
            };
            if only.is_some_and(|only| slot.generation() != only) {
                return Next::Nothing;
            }
            match slot {
                Slot::Starting { stop, done, .. } => {
                    stop.get_or_insert(reason);
                    Next::Wait(done.clone())
                }
                Slot::Stopping { done, .. } => Next::Wait(done.clone()),
                Slot::Blocked { .. } => Next::Nothing,
                Slot::Running { .. } => {
                    let Some(Slot::Running { generation, bundle }) = slots.remove(&channel) else {
                        return Next::Nothing;
                    };
                    let (done, waiting) = watch::channel(false);
                    slots.insert(
                        channel,
                        Slot::Stopping {
                            generation,
                            done: waiting,
                        },
                    );
                    Next::Stop(generation, bundle, done)
                }
            }
        });
        let this = Arc::clone(self);
        let waiting = match next {
            Next::Nothing => None,
            Next::Wait(waiting) => Some(waiting),
            Next::Stop(generation, bundle, done) => {
                let waiting = done.subscribe();
                let finish = Finish {
                    supervisor: Arc::clone(&this),
                    channel,
                    generation,
                    done: Some(done),
                };
                tokio::spawn(async move {
                    let settled = bundle.stop_and_join(reason).await;
                    finish.finish(Slot::settled(generation, settled));
                });
                Some(waiting)
            }
        };
        async move {
            if let Some(waiting) = waiting {
                finished(waiting).await;
            }
            this.phase(channel)
        }
    }

    /// Stops every channel's generation and waits for each.
    pub async fn stop_all(self: &Arc<Self>, reason: StopReason) {
        let channels: Vec<u64> = self.with_slots(|slots| slots.keys().copied().collect());
        let stops = channels
            .into_iter()
            .map(|channel| self.stop(channel, None, reason));
        futures::future::join_all(stops).await;
    }
}

/// What the process's shutdown, backend exit and deferred restart ask of one provider's homes.
pub trait HomeLifecycle: Send + Sync + 'static {
    /// Holds new intake while a restart may still be cancelled; `resume_intake` undoes it.
    fn pause_intake(&self, nonce: &str);
    fn resume_intake(&self, nonce: &str);
    fn stop_and_join(
        &self,
        reason: StopReason,
    ) -> Pin<Box<dyn Future<Output = Settled> + Send + '_>>;
}

/// A provider with no delegated homes, as with the switch off: every call does nothing.
pub struct NoHomes;

impl HomeLifecycle for NoHomes {
    fn pause_intake(&self, _: &str) {}

    fn resume_intake(&self, _: &str) {}

    fn stop_and_join(&self, _: StopReason) -> Pin<Box<dyn Future<Output = Settled> + Send + '_>> {
        Box::pin(std::future::ready(Settled::Joined))
    }
}

/// Owns every provider's cleanup task before returning its wait future.
pub fn stop_all_and_join(
    providers: Vec<Arc<dyn HomeLifecycle>>,
    reason: StopReason,
) -> impl Future<Output = Vec<Settled>> + Send {
    let stops: Vec<_> = providers
        .into_iter()
        .map(|provider| tokio::spawn(async move { provider.stop_and_join(reason).await }))
        .collect();
    async move {
        futures::future::join_all(stops)
            .await
            .into_iter()
            .map(|stop| stop.unwrap_or_else(|error| Settled::Stuck(error.to_string())))
            .collect()
    }
}

#[cfg(test)]
#[path = "home_supervisor_tests.rs"]
mod tests;
