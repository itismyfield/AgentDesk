use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
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
    fn as_ws_message(&self) -> String {
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

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(tx): State<BroadcastTx>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // Check auth token if configured
    let config = crate::config::load_graceful();
    if let Some(expected) = config.server.auth_token.as_deref() {
        if !expected.is_empty() {
            let token = query.get("token").map(|s| s.as_str()).unwrap_or("");
            if token != expected {
                return axum::response::Response::builder()
                    .status(401)
                    .body(axum::body::Body::from("unauthorized"))
                    .unwrap()
                    .into_response();
            }
        }
    }
    ws.on_upgrade(move |socket| handle_socket(socket, tx))
        .into_response()
}

async fn handle_socket(socket: WebSocket, tx: BroadcastTx) {
    let (mut sender, mut receiver) = socket.split();

    // Send connected event
    let connected = json!({"type": "connected"}).to_string();
    if sender.send(Message::Text(connected.into())).await.is_err() {
        return;
    }

    let mut rx = tx.subscribe();

    // Forward broadcast messages to this client
    let mut send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(msg) => {
                            if sender.send(Message::Text(msg.as_ws_message().into())).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!("[ws] Client lagged, skipped {n} messages");
                        }
                        Err(_) => break,
                    }
                }
                // Send ping every 30s to keep alive
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                    if sender.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Consume incoming messages (ignore them, just detect disconnect)
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = &mut send_task => { recv_task.abort(); }
        _ = &mut recv_task => { send_task.abort(); }
    }
}
