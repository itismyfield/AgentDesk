//! In-process broadcast event bus for pushing live events to dashboard clients.
//!
//! This is shared infrastructure consumed by both the HTTP/WebSocket server
//! layer (`crate::server::ws`, which owns the axum upgrade handler and re-exports
//! these primitives) and background services (`crate::services::*`). It lives at
//! the crate root — below both `server` and `services` in the dependency graph —
//! so services can emit events without reaching up into `crate::server`
//! (#3037 service→server backflow removal). The axum `ws_handler`/`handle_socket`
//! that consume these primitives remain in `crate::server::ws`.

use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, broadcast};

/// Shared broadcast sender for pushing events to all connected WS clients.
pub type BroadcastTx = Arc<BroadcastBus>;

/// Buffer for batched events — groups events by key, flushes periodically.
pub type BatchBuffer = Arc<Mutex<HashMap<String, PendingEvent>>>;

const BROADCAST_HISTORY_LIMIT: usize = 256;

#[derive(Clone, Debug)]
pub struct BroadcastEvent {
    pub id: String,
    pub event: String,
    pub data: serde_json::Value,
}

impl BroadcastEvent {
    pub(crate) fn as_ws_message(&self) -> String {
        json!({
            "id": self.id,
            "type": self.event,
            "data": self.data,
        })
        .to_string()
    }
}

#[derive(Clone, Debug)]
pub struct PendingEvent {
    event: String,
    data: serde_json::Value,
}

pub struct BroadcastBus {
    tx: broadcast::Sender<BroadcastEvent>,
    history: StdMutex<VecDeque<BroadcastEvent>>,
    next_event_id: AtomicU64,
}

impl BroadcastBus {
    fn new() -> Self {
        let (tx, _) = broadcast::channel::<BroadcastEvent>(256);
        Self {
            tx,
            history: StdMutex::new(VecDeque::with_capacity(BROADCAST_HISTORY_LIMIT)),
            next_event_id: AtomicU64::new(1),
        }
    }

    fn send(&self, event: &str, data: serde_json::Value) -> BroadcastEvent {
        let envelope = BroadcastEvent {
            id: self
                .next_event_id
                .fetch_add(1, Ordering::Relaxed)
                .to_string(),
            event: event.to_string(),
            data,
        };
        if let Ok(mut history) = self.history.lock() {
            if history.len() >= BROADCAST_HISTORY_LIMIT {
                history.pop_front();
            }
            history.push_back(envelope.clone());
        }
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastEvent> {
        self.tx.subscribe()
    }

    pub fn replay_since(&self, last_event_id: &str) -> Vec<BroadcastEvent> {
        let Ok(last_seen) = last_event_id.parse::<u64>() else {
            return Vec::new();
        };
        self.history
            .lock()
            .map(|history| {
                history
                    .iter()
                    .filter(|event| {
                        event
                            .id
                            .parse::<u64>()
                            .ok()
                            .is_some_and(|event_id| event_id > last_seen)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub fn new_broadcast() -> BroadcastTx {
    Arc::new(BroadcastBus::new())
}

/// Immediately emit an event to all connected WebSocket clients.
pub fn emit_event(tx: &BroadcastTx, event_name: &str, payload: serde_json::Value) {
    tx.send(event_name, payload);
}

/// Queue a batched event — deduplicates by key, flushed periodically.
pub fn emit_batched_event(
    buffer: &BatchBuffer,
    event_name: &str,
    key: impl Into<String>,
    payload: serde_json::Value,
) {
    let key = key.into();
    let event_name = event_name.to_string();
    let buffer = buffer.clone();
    tokio::spawn(async move {
        buffer.lock().await.insert(
            key,
            PendingEvent {
                event: event_name,
                data: payload,
            },
        );
    });
}

/// Spawn background flusher that drains batch buffer every 200ms.
pub fn spawn_batch_flusher(tx: BroadcastTx) -> BatchBuffer {
    let buffer: BatchBuffer = Arc::new(Mutex::new(HashMap::new()));
    let flush_buffer = buffer.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            interval.tick().await;
            let mut buf = flush_buffer.lock().await;
            if buf.is_empty() {
                continue;
            }
            for (_key, pending) in buf.drain() {
                tx.send(&pending.event, pending.data);
            }
        }
    });
    buffer
}
