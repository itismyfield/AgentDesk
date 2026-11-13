//! A detached restart keeps owner delivery excluded until its result is remembered.
#![cfg(unix)]
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use crate::services::discord::{ChannelId, SharedData};
use crate::services::turn_orchestrator::restart_operation::Operation;

type Key = (String, u64);
static ACTIVE: LazyLock<Mutex<HashSet<Key>>> = LazyLock::new(Default::default);
static COMPLETED: LazyLock<Mutex<HashMap<Operation, String>>> = LazyLock::new(Default::default);
static CHANGED: LazyLock<tokio::sync::Notify> = LazyLock::new(Default::default);

pub(crate) fn completed(operation: &Operation) -> Option<String> {
    COMPLETED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(operation)
        .cloned()
}

pub(crate) fn remember(operation: &Operation, response: String) {
    COMPLETED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(operation.clone())
        .or_insert(response);
}

pub(crate) async fn wait_completed(operation: &Operation) -> Option<String> {
    loop {
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let active = {
            let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(response) = completed(operation) {
                return Some(response);
            }
            active.contains(&(operation.namespace().into(), operation.channel()))
        };
        if !active {
            return None;
        }
        notified.await;
    }
}

pub(crate) struct Gate {
    key: Key,
    shared: Arc<SharedData>,
    channel: ChannelId,
}

impl Gate {
    pub(crate) fn begin(shared: &Arc<SharedData>, channel: ChannelId) -> Result<Self, String> {
        let key = (
            format!("{}/{}", shared.provider.as_str(), shared.token_hash),
            channel.get(),
        );
        if !ACTIVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone())
        {
            return Err("restart_busy".into());
        }
        Ok(Self {
            key,
            shared: shared.clone(),
            channel,
        })
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        ACTIVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
        CHANGED.notify_waiters();
        crate::services::discord::router::message_handler::owner_wake(&self.shared, self.channel);
    }
}

pub(crate) fn inflight(provider: &str, token: &str, channel: u64) -> bool {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(format!("{provider}/{token}"), channel))
}

#[cfg(all(test, unix))]
#[path = "restart_gate_tests.rs"]
mod tests;
