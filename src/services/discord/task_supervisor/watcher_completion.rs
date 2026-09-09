use super::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::watch;

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
/// Clone before cancellation. A task that ended before lookup also returns None:
/// absence is Unknown, never evidence that the watcher joined.
/// This observes one registration, not all tasks sharing an Arc. After its record
/// is removed, a third registration may complete while a duplicate still lives.
/// Retirement consumers must separately establish unique spawn incarnation.
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
}
impl Registration {
    pub(super) fn new(cancel: Arc<AtomicBool>) -> Self {
        let sender = Arc::new(watch::channel(Outcome::Pending).0);
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        if let std::collections::hash_map::Entry::Vacant(entry) = records.entry(key(&cancel)) {
            entry.insert(Record {
                _cancel: cancel.clone(),
                sender: sender.clone(),
            });
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
        }
        Self { cancel, sender }
    }
    fn remove(&self) {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        if records
            .get(&key(&self.cancel))
            .is_some_and(|r| Arc::ptr_eq(&r.sender, &self.sender))
        {
            records.remove(&key(&self.cancel));
        }
    }
    pub(super) fn finish(self, result: Outcome) {
        self.remove();
        if *self.sender.borrow() != Outcome::Unknown {
            self.sender.send_replace(result);
        }
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
#[path = "watcher_completion_tests.rs"]
mod tests;
