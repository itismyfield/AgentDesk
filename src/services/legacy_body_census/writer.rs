use super::{Shared, State, history, invalidate};
use crate::services::process_boot::Boot;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tokio::sync::oneshot;

pub(super) type Hook = Arc<dyn Fn() + Send + Sync>;
pub(super) type SyncParent = fn(&Path) -> std::io::Result<()>;

pub(super) struct Options {
    pub(super) doctor: Option<String>,
    pub(super) host: Option<String>,
    pub(super) timeout: Duration,
    pub(super) before_write: Hook,
    pub(super) sync_parent: Option<SyncParent>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            doctor: None,
            host: crate::config::snapshot()
                .and_then(|config| config.cluster.instance_id.clone())
                .map(|host| host.trim().to_owned())
                .filter(|host| !host.is_empty()),
            timeout: Duration::from_secs(2),
            before_write: Arc::new(|| {}),
            sync_parent: None,
        }
    }
}

struct Message {
    record: Value,
    ack: Option<oneshot::Sender<bool>>,
}

pub(super) struct Writer {
    sender: mpsc::SyncSender<Message>,
    pub(super) state: Shared,
    timeout: Duration,
}

fn persist_invalid(file: &mut std::fs::File, state: &Shared, persisted: &mut Vec<String>) {
    let fresh: Vec<_> = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .own_reasons
        .iter()
        .filter(|reason| !persisted.contains(reason))
        .cloned()
        .collect();
    if !fresh.is_empty()
        && history::append_synced(file, &json!({"kind":"observation_invalid","reasons":fresh}))
            .is_ok()
    {
        persisted.extend(fresh);
    }
}

impl Writer {
    pub(super) fn spawn(root: Option<PathBuf>, boot: Boot, options: Options) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Message>(64);
        let state = Arc::new(Mutex::new(State::new(&boot, options.host)));
        let shared = state.clone();
        let timeout = options.timeout;
        let thread = std::thread::Builder::new()
            .name("legacy-body-census-writer".into())
            .spawn(move || {
                (options.before_write)();
                let opened = root
                    .as_deref()
                    .ok_or("RUNTIME_ROOT_UNAVAILABLE")
                    .and_then(|root| match options.sync_parent {
                        Some(sync) => {
                            history::prepare_with_sync(root, &boot, options.doctor, &shared, sync)
                        }
                        None => history::prepare(root, &boot, options.doctor, &shared),
                    });
                let mut file = match opened {
                    Ok(file) => Some(file),
                    Err(reason) => {
                        invalidate(&shared, reason);
                        None
                    }
                };
                let mut persisted = Vec::new();
                if let Some(file) = file.as_mut() {
                    persist_invalid(file, &shared, &mut persisted);
                }
                while let Ok(message) = receiver.recv() {
                    (options.before_write)();
                    let gate = root
                        .as_deref()
                        .and_then(|root| history::lock_writer(root).ok());
                    let written = gate.is_some()
                        && file.as_mut().is_some_and(|file| {
                            history::append_synced(file, &message.record).is_ok()
                        });
                    if !written {
                        invalidate(&shared, "WRITE_FAILED");
                    }
                    if let Some(file) = file.as_mut() {
                        persist_invalid(file, &shared, &mut persisted);
                    }
                    if let Some(ack) = message.ack
                        && ack.send(written).is_err()
                    {
                        invalidate(&shared, "ACK_RECEIVER_DROPPED");
                        if let Some(file) = file.as_mut() {
                            persist_invalid(file, &shared, &mut persisted);
                        }
                    }
                }
            });
        if thread.is_err() {
            invalidate(&state, "WRITER_THREAD_FAILED");
        }
        Self {
            sender,
            state,
            timeout,
        }
    }

    pub(super) fn enqueue(&self, record: Value) -> Option<oneshot::Receiver<bool>> {
        let (ack, received) = oneshot::channel();
        if self
            .sender
            .try_send(Message {
                record,
                ack: Some(ack),
            })
            .is_err()
        {
            invalidate(&self.state, "WRITE_QUEUE_UNAVAILABLE");
            return None;
        }
        Some(received)
    }

    pub(super) fn try_append(&self, record: Value) {
        if self.sender.try_send(Message { record, ack: None }).is_err() {
            invalidate(&self.state, "WRITE_QUEUE_UNAVAILABLE");
        }
    }

    pub(super) async fn append(&self, record: Value) -> bool {
        match self.enqueue(record) {
            Some(received) => self.wait_for_ack(received).await,
            None => false,
        }
    }

    pub(super) async fn wait_for_ack(&self, received: oneshot::Receiver<bool>) -> bool {
        match tokio::time::timeout(self.timeout, received).await {
            Ok(Ok(true)) => true,
            Ok(_) => {
                invalidate(&self.state, "WRITE_FAILED");
                false
            }
            Err(_) => {
                invalidate(&self.state, "WRITE_TIMEOUT");
                false
            }
        }
    }
}
