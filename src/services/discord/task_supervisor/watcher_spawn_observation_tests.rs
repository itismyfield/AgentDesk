use super::{SharedData, watcher_completion::Outcome};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::task::AbortHandle;

type Records = Mutex<Vec<Arc<Record>>>;
static SCOPES: LazyLock<Mutex<HashMap<usize, Weak<Records>>>> = LazyLock::new(Mutex::default);
static NEXT_SUBMISSION: AtomicU64 = AtomicU64::new(1);

pub(super) struct Record {
    id: u64,
    cancel: Arc<AtomicBool>,
    task: Mutex<Option<AbortHandle>>,
    outcome: Mutex<Outcome>,
}

pub(in crate::services::discord) struct Scope {
    shared: Arc<SharedData>,
    records: Arc<Records>,
}

impl Scope {
    pub(in crate::services::discord) fn new(shared: &Arc<SharedData>) -> Self {
        let records = Arc::new(Mutex::new(Vec::new()));
        let previous = SCOPES
            .lock()
            .unwrap()
            .insert(Arc::as_ptr(shared) as usize, Arc::downgrade(&records));
        assert!(previous.is_none(), "one spawn observer per fixture runtime");
        Self {
            shared: shared.clone(),
            records,
        }
    }

    pub(in crate::services::discord) fn submissions(&self) -> usize {
        self.records.lock().unwrap().len()
    }

    pub(in crate::services::discord) async fn cancel_and_join_all(&self) -> Vec<(u64, Outcome)> {
        let records = self.records.lock().unwrap().clone();
        for record in &records {
            crate::services::discord::tmux::tmux_watcher::cancel_for_test(
                &record.cancel,
                Ordering::Release,
            );
        }
        self.join_all().await
    }

    pub(in crate::services::discord) async fn join_all(&self) -> Vec<(u64, Outcome)> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let records = self.records.lock().unwrap().clone();
            let complete = records.iter().all(|record| {
                record
                    .task
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(AbortHandle::is_finished)
            });
            if complete {
                return records
                    .iter()
                    .map(|record| {
                        let outcome = *record.outcome.lock().unwrap();
                        assert_ne!(
                            outcome,
                            Outcome::Pending,
                            "submission {} missed completion",
                            record.id
                        );
                        (record.id, outcome)
                    })
                    .collect();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "submitted fixture tasks did not all join"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        SCOPES
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(&self.shared) as usize));
    }
}

pub(super) fn submitted(shared: &Arc<SharedData>, cancel: &Arc<AtomicBool>) -> Option<Arc<Record>> {
    let records = SCOPES
        .lock()
        .unwrap()
        .get(&(Arc::as_ptr(shared) as usize))
        .and_then(Weak::upgrade)?;
    let record = Arc::new(Record {
        id: NEXT_SUBMISSION.fetch_add(1, Ordering::Relaxed),
        cancel: cancel.clone(),
        task: Mutex::new(None),
        outcome: Mutex::new(Outcome::Pending),
    });
    // Each actual submission gets a record, even when it reuses the same cancel Arc.
    records.lock().unwrap().push(record.clone());
    Some(record)
}

pub(super) fn attach(record: &Option<Arc<Record>>, task: AbortHandle) {
    if let Some(record) = record {
        *record.task.lock().unwrap() = Some(task);
    }
}

pub(super) fn finished(record: &Option<Arc<Record>>, outcome: Outcome) {
    if let Some(record) = record {
        *record.outcome.lock().unwrap() = outcome;
    }
}
