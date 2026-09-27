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
    quiesce: Option<oneshot::Sender<oneshot::Sender<bool>>>,
    active: usize,
    observed: bool,
    ambiguous: bool,
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
/// Observe one registration only; missing is Unknown, never evidence of completion.
/// A later registration may be observed while older duplicates remain alive.
pub fn observe(cancel: &Arc<AtomicBool>) -> Option<Ticket> {
    let records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    records
        .get(&key(cancel))
        .filter(|r| r.observed)
        .map(|r| Ticket {
            _cancel: cancel.clone(),
            receiver: r.sender.subscribe(),
        })
}
/// Stop this registered reader; success follows its future and cleanup destruction.
/// Missing, duplicate, uncertain, aborted, or already requested registrations fail closed.
#[allow(dead_code)]
pub async fn quiesce(cancel: &Arc<AtomicBool>) -> Result<(), ()> {
    let (ack, completed) = oneshot::channel();
    {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let record = records.get_mut(&key(cancel)).ok_or(())?;
        if !record.observed || record.ambiguous || *record.sender.borrow() != Outcome::Pending {
            return Err(());
        }
        record.quiesce.take().ok_or(())?.send(ack).map_err(|_| ())?;
    }
    match completed.await {
        Ok(true) => Ok(()),
        _ => Err(()),
    }
}
pub(super) struct Registration {
    cancel: Arc<AtomicBool>,
    sender: Arc<watch::Sender<Outcome>>,
    quiesce: oneshot::Receiver<oneshot::Sender<bool>>,
    ack: Option<oneshot::Sender<bool>>,
    active: bool,
}
impl Registration {
    pub(super) fn new(cancel: Arc<AtomicBool>) -> Self {
        let sender = Arc::new(watch::channel(Outcome::Pending).0);
        let (request, quiesce) = oneshot::channel();
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = records.get_mut(&key(&cancel)) {
            record.active += 1;
            record.ambiguous = true;
            if record.observed {
                record.sender.send_replace(Outcome::Unknown);
                sender.send_replace(Outcome::Unknown);
                tracing::warn!(
                    "duplicate watcher completion registration; all observations are Unknown"
                );
            } else {
                // Preserve legacy observation lookup; remaining duplicates still forbid ACK.
                record.sender = sender.clone();
                record.quiesce = Some(request);
                record.observed = true;
            }
        } else {
            records.insert(
                key(&cancel),
                Record {
                    _cancel: cancel.clone(),
                    sender: sender.clone(),
                    quiesce: Some(request),
                    active: 1,
                    observed: true,
                    ambiguous: false,
                },
            );
        }
        Self {
            cancel,
            sender,
            quiesce,
            ack: None,
            active: true,
        }
    }
    pub(super) async fn quiesce_requested(&mut self) {
        self.ack = Some(match (&mut self.quiesce).await {
            Ok(ack) => ack,
            Err(_) => std::future::pending().await,
        });
    }
    fn remove(&mut self, records: &mut HashMap<usize, Record>) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        let Some(record) = records.get_mut(&key(&self.cancel)) else {
            return false;
        };
        let current = Arc::ptr_eq(&record.sender, &self.sender);
        let unambiguous = current && record.observed && !record.ambiguous;
        record.active -= 1;
        if current {
            record.observed = false;
            record.quiesce = None;
        }
        if record.active == 0 {
            records.remove(&key(&self.cancel));
        }
        unambiguous
    }
    pub(super) fn finish(mut self, result: Outcome) {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let unambiguous = self.remove(&mut records);
        if *self.sender.borrow() != Outcome::Unknown {
            self.sender.send_replace(result);
        }
        if let Some(ack) = self.ack.take() {
            let _ = ack.send(unambiguous);
        }
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if self.active && *self.sender.borrow() == Outcome::Pending {
            self.sender.send_replace(Outcome::Unknown);
        }
        self.remove(&mut RECORDS.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

#[cfg(test)]
#[path = "watcher_completion_tests.rs"]
mod tests;
