//! Pauses two production consumers after fresh guards, before the automatic dequeue.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;
use tokio::sync::Notify;

tokio::task_local! {
    static ORIGIN: &'static str;
}

static RACES: LazyLock<dashmap::DashMap<ChannelId, Arc<Race>>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Default)]
struct Race {
    arrived: Mutex<Vec<&'static str>>,
    arrival: Notify,
    finished: Mutex<Vec<&'static str>>,
    finish: Notify,
    released: AtomicBool,
    release: Notify,
}

pub(in crate::services::discord) async fn with_origin<T>(
    origin: &'static str,
    future: impl Future<Output = T>,
) -> T {
    if ORIGIN.try_with(|value| *value).is_ok() {
        future.await
    } else {
        ORIGIN.scope(origin, future).await
    }
}

pub(super) struct Participant {
    race: Arc<Race>,
    origin: &'static str,
}

impl Drop for Participant {
    fn drop(&mut self) {
        let mut finished = self
            .race
            .finished
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !finished.contains(&self.origin) {
            finished.push(self.origin);
        }
        drop(finished);
        self.race.finish.notify_waiters();
    }
}

pub(super) async fn pause(channel: ChannelId) -> Option<Participant> {
    let Ok(origin @ ("turn_completion_event" | "idle_queue_backstop")) = ORIGIN.try_with(|o| *o)
    else {
        return None;
    };
    let Some(race) = RACES.get(&channel).map(|r| r.value().clone()) else {
        return None;
    };
    {
        let mut arrivals = race
            .arrived
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !arrivals.contains(&origin) {
            arrivals.push(origin);
        }
    }
    race.arrival.notify_waiters();
    loop {
        let released = race.release.notified();
        tokio::pin!(released);
        released.as_mut().enable();
        if race.released.load(Ordering::Acquire) {
            return Some(Participant {
                race: race.clone(),
                origin,
            });
        }
        released.await;
    }
}

pub(in crate::services::discord) struct DequeueRace {
    channel: ChannelId,
    race: Arc<Race>,
}

impl DequeueRace {
    pub(in crate::services::discord) fn install(channel: ChannelId) -> Self {
        let race = Arc::new(Race::default());
        assert!(
            RACES.insert(channel, race.clone()).is_none(),
            "one dequeue race per channel"
        );
        Self { channel, race }
    }

    pub(in crate::services::discord) async fn wait_for_both(
        &self,
        timeout: Duration,
    ) -> Vec<&'static str> {
        let _ = tokio::time::timeout(timeout, async {
            loop {
                let arrived = self.race.arrival.notified();
                tokio::pin!(arrived);
                arrived.as_mut().enable();
                if self
                    .race
                    .arrived
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .len()
                    == 2
                {
                    break;
                }
                arrived.await;
            }
        })
        .await;
        self.race
            .arrived
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(in crate::services::discord) async fn wait_for_both_finished(
        &self,
        timeout: Duration,
    ) -> Vec<&'static str> {
        let _ = tokio::time::timeout(timeout, async {
            loop {
                let finished = self.race.finish.notified();
                tokio::pin!(finished);
                finished.as_mut().enable();
                if self
                    .race
                    .finished
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .len()
                    == 2
                {
                    break;
                }
                finished.await;
            }
        })
        .await;
        self.race
            .finished
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(in crate::services::discord) fn release(&self) {
        self.race.released.store(true, Ordering::Release);
        self.race.release.notify_waiters();
    }
}

impl Drop for DequeueRace {
    fn drop(&mut self) {
        self.release();
        RACES.remove_if(&self.channel, |_, race| Arc::ptr_eq(race, &self.race));
    }
}
