//! Process-local wakeups for newly published channel work.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::sync::Notify;

type Listeners = HashMap<u64, Vec<Weak<Notify>>>;
static LISTENERS: LazyLock<Mutex<Listeners>> = LazyLock::new(Mutex::default);

/// Called after a producer publishes work; each active run has its own stored permit.
pub(crate) fn wake(channel: u64) {
    let listeners = LISTENERS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(runs) = listeners.get(&channel) {
        for notify in runs.iter().filter_map(Weak::upgrade) {
            notify.notify_one();
        }
    }
}

pub(crate) struct Listener(u64, Arc<Notify>);

impl Listener {
    pub(crate) fn new(channel: u64) -> Self {
        let notify = Arc::new(Notify::new());
        let mut listeners = LISTENERS.lock().unwrap_or_else(|error| error.into_inner());
        listeners
            .entry(channel)
            .or_default()
            .push(Arc::downgrade(&notify));
        Self(channel, notify)
    }

    pub(crate) async fn notified(&self) {
        self.1.notified().await;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let mut listeners = LISTENERS.lock().unwrap_or_else(|error| error.into_inner());
        if let std::collections::hash_map::Entry::Occupied(mut entry) = listeners.entry(self.0) {
            entry
                .get_mut()
                .retain(|run| !std::ptr::eq(run.as_ptr(), Arc::as_ptr(&self.1)));
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }
}
