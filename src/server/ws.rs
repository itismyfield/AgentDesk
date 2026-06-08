//! WebSocket upgrade handler (axum) for the dashboard live feed.
//!
//! The broadcast bus primitives (`BroadcastBus`, `BroadcastTx`, `emit_event`,
//! `emit_batched_event`, …) live in `crate::eventbus` so background services can
//! emit events without a service→server backflow (#3037). They are re-exported
//! here for backward compatibility, so existing `crate::server::ws::*` call sites
//! continue to resolve unchanged.

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::broadcast;

pub use crate::eventbus::{
    BatchBuffer, BroadcastEvent, BroadcastTx, emit_event, new_broadcast, spawn_batch_flusher,
};

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(tx): State<BroadcastTx>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
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

    // #2050 P1 finding 2 — accept `?since=<id>` (or legacy `?last_event_id=`)
    // query parameter, or `Last-Event-Id` header, so reconnecting clients can
    // replay events they missed while disconnected. The id is the numeric
    // envelope id assigned by BroadcastBus.
    let last_event_id = query
        .get("since")
        .or_else(|| query.get("last_event_id"))
        .cloned()
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });

    ws.on_upgrade(move |socket| handle_socket(socket, tx, last_event_id))
        .into_response()
}

async fn handle_socket(socket: WebSocket, tx: BroadcastTx, last_event_id: Option<String>) {
    let (mut sender, mut receiver) = socket.split();

    // Send connected event
    let connected = json!({"type": "connected"}).to_string();
    if sender.send(Message::Text(connected.into())).await.is_err() {
        return;
    }

    // Subscribe BEFORE replaying history so events emitted *after* the replay
    // snapshot is taken still arrive via the live broadcast channel. The
    // overlap window may produce duplicates (same envelope id), but clients
    // dedupe by id so this is acceptable in exchange for zero loss.
    let mut rx = tx.subscribe();

    // Flush any events that happened after the client's last seen id (#2050 P1 #2).
    if let Some(since) = last_event_id.as_deref().filter(|s| !s.is_empty()) {
        for replay in tx.replay_since(since) {
            if sender
                .send(Message::Text(replay.as_ws_message().into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }

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
                // Send ping every 5s — #2050 P3 finding 21. The previous
                // 30s tick let `send_task` linger up to 30s after a client
                // disconnect before the next failed write tripped `break`.
                // Under rapid HMR / large reconnect storms that produced
                // pile-ups of stale tasks. 5s gives the loop a chance to
                // observe broadcast errors and exit promptly while still
                // being conservative on bandwidth.
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
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
