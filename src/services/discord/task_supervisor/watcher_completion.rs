use super::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Returned,
    Panicked,
    Unknown,
}
struct Record {
    _cancel: Arc<AtomicBool>,
    sender: Arc<watch::Sender<Outcome>>,
    ack: Option<oneshot::Sender<QuiesceAck>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuiesceAck {
    Quiesced,
    Ambiguous,
    Busy,
}

/// Request cooperative cancellation; callers must release registry and sidecar guards first.
/// Only Quiesced certifies this watcher; timeout never aborts its pending transport.
#[allow(dead_code)]
pub async fn quiesce(cancel: &Arc<AtomicBool>, timeout: std::time::Duration) -> QuiesceAck {
    let receiver = {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(record) = records.get_mut(&key(cancel)) else {
            return QuiesceAck::Busy;
        };
        if *record.sender.borrow() != Outcome::Pending || record.ack.is_some() {
            return QuiesceAck::Busy;
        }
        let (sender, receiver) = oneshot::channel();
        record.ack = Some(sender);
        cancel.store(true, std::sync::atomic::Ordering::Release);
        receiver
    };
    tokio::time::timeout(timeout, receiver)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(QuiesceAck::Busy)
}
static RECORDS: LazyLock<Mutex<HashMap<usize, Record>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
fn key(cancel: &Arc<AtomicBool>) -> usize {
    Arc::as_ptr(cancel) as usize
}
pub struct Ticket {
    _cancel: Arc<AtomicBool>,
    receiver: watch::Receiver<Outcome>,
}
impl Ticket {
    /// Observation only: abort, cleanup panic or duplicate identity are Unknown.
    /// Unknown may precede cleanup; it never certifies task or delivery completion.
    pub async fn wait(mut self) -> Outcome {
        loop {
            let result = *self.receiver.borrow_and_update();
            if result != Outcome::Pending {
                return result;
            }
            if self.receiver.changed().await.is_err() {
                return Outcome::Unknown;
            }
        }
    }
}
/// Clone before cancellation; missing or duplicate registrations never prove completion.
pub fn observe(cancel: &Arc<AtomicBool>) -> Option<Ticket> {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    records.get(&key(cancel)).map(|r| Ticket {
        _cancel: cancel.clone(),
        receiver: r.sender.subscribe(),
    })
}
pub(super) struct Registration {
    cancel: Arc<AtomicBool>,
    sender: Arc<watch::Sender<Outcome>>,
    may_poll: bool,
    needs_cleanup: bool,
}
impl Registration {
    pub(super) fn new(cancel: Arc<AtomicBool>) -> Self {
        let sender = Arc::new(watch::channel(Outcome::Pending).0);
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let mut may_poll = false;
        let mut needs_cleanup = true;
        if let std::collections::hash_map::Entry::Vacant(entry) = records.entry(key(&cancel)) {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                sender.send_replace(Outcome::Unknown);
            } else {
                may_poll = true;
                entry.insert(Record {
                    _cancel: cancel.clone(),
                    sender: sender.clone(),
                    ack: None,
                });
            }
        } else {
            // Preserve the record identity, but invalidate every observer of this
            // ambiguous cancel Arc. finish must not overwrite this sticky Unknown.
            records
                .get(&key(&cancel))
                .unwrap()
                .sender
                .send_replace(Outcome::Unknown);
            tracing::warn!(
                "duplicate watcher completion registration; all observations are Unknown"
            );
            sender.send_replace(Outcome::Unknown);
            needs_cleanup = false;
        }
        Self {
            cancel,
            sender,
            may_poll,
            needs_cleanup,
        }
    }
    pub(super) fn may_poll(&self) -> bool {
        self.may_poll
    }
    pub(super) fn needs_cleanup(&self) -> bool {
        self.needs_cleanup
    }
    fn remove(&self) -> Option<Record> {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        if records
            .get(&key(&self.cancel))
            .is_some_and(|r| Arc::ptr_eq(&r.sender, &self.sender))
        {
            records.remove(&key(&self.cancel))
        } else {
            None
        }
    }
    pub(super) fn finish(self, result: Outcome, ambiguous: bool) {
        let record = self.remove();
        let unambiguous = *self.sender.borrow() != Outcome::Unknown;
        if unambiguous {
            self.sender.send_replace(result);
        }
        if let Some(ack) = record.and_then(|record| record.ack) {
            let result = if !unambiguous || result != Outcome::Returned {
                QuiesceAck::Busy
            } else if ambiguous {
                QuiesceAck::Ambiguous
            } else {
                QuiesceAck::Quiesced
            };
            let _ = ack.send(result);
        }
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(ack) = self.remove().and_then(|record| record.ack) {
            let _ = ack.send(QuiesceAck::Busy);
        }
    }
}

#[cfg(test)]
#[path = "watcher_completion_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "quiesce_tests.rs"]
mod quiesce_tests;
