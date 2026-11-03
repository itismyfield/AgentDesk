use super::{Shared, State, cache::ProjectionCache, collector::Collector, history, invalidate};
use crate::services::process_boot::Boot;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
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
            host: None,
            timeout: Duration::from_secs(2),
            before_write: Arc::new(|| {}),
            sync_parent: None,
        }
    }
}

struct Message {
    record: Value,
    ack: Option<oneshot::Sender<bool>>,
    cut: u64,
    host: Option<Option<String>>,
}

pub(super) struct Writer {
    sender: mpsc::SyncSender<Message>,
    pub(super) state: Shared,
    timeout: Duration,
    pub(super) projection: Arc<ProjectionCache>,
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

fn write_drain(file: &mut Option<std::fs::File>, state: &Shared, cut: u64, closed: bool) -> bool {
    let marker = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .window
        .marker(cut, closed);
    let synced = file
        .as_mut()
        .is_some_and(|file| history::append_synced(file, &marker).is_ok());
    if synced {
        let mut status = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.window.drained = Some(cut);
        status.window.closed = closed;
    } else {
        invalidate(state, "DRAIN_WRITE_FAILED");
    }
    synced
}

impl Writer {
    pub(super) fn spawn(root: Option<PathBuf>, boot: Boot, mut options: Options) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Message>(64);
        let state = Arc::new(Mutex::new(State::new(&boot, options.host.clone())));
        let shared = state.clone();
        let timeout = options.timeout;
        let projection = Arc::new(ProjectionCache::default());
        let cached = projection.clone();
        let thread = std::thread::Builder::new()
            .name("legacy-body-census-writer".into())
            .spawn(move || {
                let mut file = None;
                let mut prepared = false;
                let mut persisted = Vec::new();
                let mut last_cut = 0;
                let mut collector = Collector::default();
                let mut refreshed = None::<Instant>;
                loop {
                    let message = match receiver.recv_timeout(Duration::from_secs(60)) {
                        Ok(message) => message,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if prepared && let Some(root) = root.as_deref() {
                                cached.refresh(
                                    &mut collector,
                                    root,
                                    &boot.id,
                                    chrono::Utc::now().timestamp_millis(),
                                );
                                refreshed = Some(Instant::now());
                            }
                            continue;
                        }
                    };
                    (options.before_write)();
                    if !prepared {
                        prepared = true;
                        let host = message
                            .host
                            .unwrap_or_else(|| options.host.take())
                            .map(|host| host.trim().to_owned())
                            .filter(|host| !host.is_empty());
                        shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .header["host"] = json!(host);
                        let opened =
                            root.as_deref()
                                .ok_or("RUNTIME_ROOT_UNAVAILABLE")
                                .and_then(|root| match options.sync_parent {
                                    Some(sync) => history::prepare_with_sync(
                                        root,
                                        &boot,
                                        options.doctor.take(),
                                        &shared,
                                        sync,
                                    ),
                                    None => history::prepare(
                                        root,
                                        &boot,
                                        options.doctor.take(),
                                        &shared,
                                    ),
                                });
                        match opened {
                            Ok(opened) => file = Some(opened),
                            Err(reason) => invalidate(&shared, reason),
                        }
                    }
                    let gate = root
                        .as_deref()
                        .and_then(|root| history::lock_writer(root).ok());
                    let written = gate.is_some()
                        && file.as_mut().is_some_and(|file| {
                            history::append_line(file, &message.record).is_ok()
                        });
                    if !written {
                        invalidate(&shared, "WRITE_FAILED");
                    }
                    if let Some(file) = file.as_mut() {
                        persist_invalid(file, &shared, &mut persisted);
                    }
                    let drained =
                        gate.is_some() && write_drain(&mut file, &shared, message.cut, false);
                    last_cut = message.cut;
                    if let Some(ack) = message.ack
                        && ack.send(written && drained).is_err()
                    {
                        invalidate(&shared, "ACK_RECEIVER_DROPPED");
                        if let Some(file) = file.as_mut() {
                            persist_invalid(file, &shared, &mut persisted);
                        }
                    }
                    drop(gate);
                    if refreshed.is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
                        && let Some(root) = root.as_deref()
                    {
                        cached.refresh(
                            &mut collector,
                            root,
                            &boot.id,
                            chrono::Utc::now().timestamp_millis(),
                        );
                        refreshed = Some(Instant::now());
                    }
                }
                let gate = root
                    .as_deref()
                    .and_then(|root| history::lock_writer(root).ok());
                if gate.is_some() {
                    if let Some(file) = file.as_mut() {
                        persist_invalid(file, &shared, &mut persisted);
                    }
                    write_drain(&mut file, &shared, last_cut, true);
                } else {
                    invalidate(&shared, "DRAIN_WRITE_FAILED");
                }
            });
        if thread.is_err() {
            invalidate(&state, "WRITER_THREAD_FAILED");
        }
        Self {
            sender,
            state,
            timeout,
            projection,
        }
    }

    pub(super) fn enqueue(&self, record: Value) -> Option<oneshot::Receiver<bool>> {
        let (ack, received) = oneshot::channel();
        self.queue(record, Some(ack), None).then_some(received)
    }

    pub(super) fn try_append(&self, record: Value) {
        self.queue(record, None, None);
    }

    fn queue(
        &self,
        record: Value,
        ack: Option<oneshot::Sender<bool>>,
        host: Option<Option<String>>,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cut) = state.window.cut.checked_add(1) else {
            state.mark("CUT_OVERFLOW");
            return false;
        };
        if self
            .sender
            .try_send(Message {
                record,
                ack,
                cut,
                host,
            })
            .is_err()
        {
            state.mark("WRITE_QUEUE_UNAVAILABLE");
            false
        } else {
            state.window.cut = cut;
            true
        }
    }

    pub(super) async fn prepare(&self, host: Option<String>) -> bool {
        let (ack, received) = oneshot::channel();
        self.queue(json!({"kind":"boot_ready"}), Some(ack), Some(host))
            && self.wait_for_ack(received).await
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
