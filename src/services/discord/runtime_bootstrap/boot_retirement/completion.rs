use super::BootSlot;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootWorkFailure {
    Invalid(&'static str),
    Worker(String),
    Closed,
}

pub struct Completed<T> {
    epoch: u64,
    provider: String,
    value: T,
}

impl<T> Completed<T> {
    pub fn value(&self) -> &T {
        &self.value
    }
    pub(super) fn matches(&self, epoch: u64, provider: &str) -> bool {
        self.epoch == epoch && self.provider == provider
    }
}

type Outcome<T> = Result<Arc<Completed<T>>, BootWorkFailure>;
type Sender<T> = watch::Sender<Option<Outcome<T>>>;

pub struct BootWorkOnce<T> {
    jobs: Mutex<BTreeMap<(u64, String), Sender<T>>>,
}

impl<T: Send + Sync + 'static> Default for BootWorkOnce<T> {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<T: Send + Sync + 'static> BootWorkOnce<T> {
    pub async fn run_once(
        &self,
        slot: &BootSlot,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Outcome<T> {
        receive(self.start(slot.cohort.epoch, slot.provider(), work)).await
    }

    pub(super) fn start(
        &self,
        epoch: u64,
        provider: &str,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> watch::Receiver<Option<Outcome<T>>> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let key = (epoch, provider.to_owned());
        if let Some(sender) = jobs.get(&key) {
            return sender.subscribe();
        }
        let (sender, receiver) = watch::channel(None);
        jobs.insert(key, sender.clone());
        drop(jobs);
        let provider = provider.to_owned();
        // The observer owns the join even when the requesting future is dropped.
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(work)
                .await
                .map(|value| {
                    Arc::new(Completed {
                        epoch,
                        provider,
                        value,
                    })
                })
                .map_err(|error| BootWorkFailure::Worker(error.to_string()));
            sender.send_replace(Some(result));
        });
        receiver
    }
}

pub(super) async fn receive<T>(mut receiver: watch::Receiver<Option<Outcome<T>>>) -> Outcome<T> {
    loop {
        if let Some(result) = receiver.borrow_and_update().as_ref() {
            return result.clone();
        }
        receiver
            .changed()
            .await
            .map_err(|_| BootWorkFailure::Closed)?;
    }
}
