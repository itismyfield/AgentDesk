//! A detached restart keeps owner delivery excluded until its result is remembered.
#![cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::services::discord::{ChannelId, SharedData};
use crate::services::turn_orchestrator::restart_operation::Operation;

type Key = (String, u64);
static ACTIVE: LazyLock<Mutex<HashMap<Key, Arc<std::time::Instant>>>> =
    LazyLock::new(Default::default);
/// A restart holding the channel longer than this is logged; it is never released by force.
const SLOW: std::time::Duration = std::time::Duration::from_secs(120);
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

/// A new process remembers nothing: only the owner's durable ticket still names the operation.
#[cfg(test)]
pub(crate) fn forget_for_tests(operation: &Operation) {
    COMPLETED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(operation);
}

pub(crate) async fn wait_completed(operation: &Operation) -> Option<String> {
    let key = (operation.namespace().into(), operation.channel());
    // Later restarts must not extend a waiter past the gate it first observed.
    let mut observed = None;
    loop {
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let active = {
            let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(response) = completed(operation) {
                return Some(response);
            }
            let Some(current) = active.get(&key) else {
                return None;
            };
            let first = observed.get_or_insert_with(|| current.clone());
            Arc::ptr_eq(first, current)
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
        {
            let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
            match active.entry(key.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Arc::new(std::time::Instant::now()));
                }
                std::collections::hash_map::Entry::Occupied(_) => return Err("restart_busy".into()),
            }
        }
        Ok(Self {
            key,
            shared: shared.clone(),
            channel,
        })
    }
}

/// How long the channel's running restart has held its gate, logged once it is slow.
pub(crate) fn held(shared: &SharedData, channel: ChannelId) -> Option<std::time::Duration> {
    let key = (
        format!("{}/{}", shared.provider.as_str(), shared.token_hash),
        channel.get(),
    );
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let held = active.get(&key)?.elapsed();
    if held > SLOW {
        tracing::warn!(
            channel_id = channel.get(),
            held_secs = held.as_secs(),
            "restart gate held long"
        );
    }
    Some(held)
}

impl Drop for Gate {
    fn drop(&mut self) {
        let began = ACTIVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
        if let Some(held) = began
            .map(|began| began.elapsed())
            .filter(|held| *held > SLOW)
        {
            let (channel_id, held_secs) = (self.channel.get(), held.as_secs());
            tracing::warn!(
                channel_id,
                held_secs,
                "restart gate released after a long hold"
            );
        }
        CHANGED.notify_waiters();
        crate::services::discord::router::message_handler::owner_wake(&self.shared, self.channel);
    }
}

pub(crate) fn inflight(provider: &str, token: &str, channel: u64) -> bool {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&(format!("{provider}/{token}"), channel))
}

#[cfg(all(test, unix))]
#[path = "restart_gate_tests.rs"]
mod tests;
