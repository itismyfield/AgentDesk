use super::{Shared, State, history, invalidate};
use crate::services::process_boot::Boot;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tokio::sync::oneshot;

pub(super) type Hook = Arc<dyn Fn() + Send + Sync>;

struct Message {
    record: Value,
    ack: oneshot::Sender<bool>,
}

pub(super) struct Writer {
    sender: mpsc::SyncSender<Message>,
    pub(super) state: Shared,
    timeout: Duration,
}

fn persist_invalid(file: &mut std::fs::File, state: &Shared) {
    let record = {
        let status = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status
            .invalid
            .then(|| json!({"kind":"observation_invalid","reasons":status.reasons}))
    };
    if let Some(record) = record {
        let _ = history::append_line(file, &record);
    }
}

impl Writer {
    pub(super) fn spawn(
        root: Option<PathBuf>,
        boot: Boot,
        doctor: Option<String>,
        timeout: Duration,
        before_write: Hook,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Message>(64);
        let state = Arc::new(Mutex::new(State::new(&boot)));
        let shared = state.clone();
        let thread = std::thread::Builder::new()
            .name("legacy-body-census-writer".into())
            .spawn(move || {
                before_write();
                let opened = root
                    .as_deref()
                    .ok_or("RUNTIME_ROOT_UNAVAILABLE")
                    .and_then(|root| history::prepare(root, &boot, doctor, &shared));
                let mut file = match opened {
                    Ok(file) => Some(file),
                    Err(reason) => {
                        invalidate(&shared, reason);
                        None
                    }
                };
                while let Ok(message) = receiver.recv() {
                    before_write();
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
                        persist_invalid(file, &shared);
                    }
                    if message.ack.send(written).is_err() {
                        invalidate(&shared, "ACK_RECEIVER_DROPPED");
                        if let Some(file) = file.as_mut() {
                            persist_invalid(file, &shared);
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

    pub(super) async fn append(&self, record: Value) -> bool {
        let (ack, received) = oneshot::channel();
        if self.sender.try_send(Message { record, ack }).is_err() {
            invalidate(&self.state, "WRITE_QUEUE_UNAVAILABLE");
            return false;
        }
        #[cfg(test)]
        if super::tests::mutant("remove_timeout") {
            return received.await.unwrap_or(false);
        }
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
