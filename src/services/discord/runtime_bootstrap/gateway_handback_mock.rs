use super::*;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get};

#[derive(Clone, Default)]
pub(super) struct Events {
    pub(super) ready: Arc<AtomicUsize>,
    pub(super) ready_at: Arc<std::sync::Mutex<Option<Instant>>>,
    pub(super) messages: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl serenity::EventHandler for Events {
    async fn ready(&self, _: serenity::Context, _: serenity::Ready) {
        *self.ready_at.lock().unwrap() = Some(Instant::now());
        self.ready.fetch_add(1, Ordering::SeqCst);
    }

    async fn message(&self, _: serenity::Context, _: serenity::Message) {
        self.messages.fetch_add(1, Ordering::SeqCst);
    }
}

async fn gateway(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        let hello = json!({"op": 10, "d": {"heartbeat_interval": 1000}});
        if socket
            .send(Message::Text(hello.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
        let mut sequence = 0;
        while let Some(Ok(Message::Text(text))) = socket.recv().await {
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            let event = match frame["op"].as_u64() {
                Some(2) => {
                    sequence += 1;
                    json!({"op": 0, "s": sequence, "t": "READY", "d": {
                        "v": 10, "user": serenity::CurrentUser::default(), "guilds": [],
                        "session_id": "isolated-gateway", "resume_gateway_url": "ws://127.0.0.1:1",
                        "application": {"id": "1", "flags": 0}, "shard": [0, 1]
                    }})
                }
                Some(1) => {
                    if socket
                        .send(Message::Text(json!({"op": 11}).to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    sequence += 1;
                    let mut message = serenity::Message::default();
                    message.id = serenity::MessageId::new(sequence);
                    message.channel_id = serenity::ChannelId::new(1);
                    message.content = "gateway remains responsive".into();
                    json!({"op": 0, "s": sequence, "t": "MESSAGE_CREATE", "d": message})
                }
                _ => continue,
            };
            if socket
                .send(Message::Text(event.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

pub(super) async fn client() -> (serenity::Client, Events, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let websocket = format!("ws://{address}/gateway");
    let gateway_url = websocket.clone();
    let app = Router::new().route("/gateway", get(gateway)).route(
        "/api/v10/gateway",
        get(move || {
            let url = gateway_url.clone();
            async move { Json(json!({"url": url})) }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let events = Events::default();
    let http = serenity::HttpBuilder::new("isolated-test-token")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .build();
    let client =
        serenity::ClientBuilder::new_with_http(http, serenity::GatewayIntents::DIRECT_MESSAGES)
            .event_handler(events.clone())
            .await
            .unwrap();
    *client.ws_url.lock().await = websocket;
    (client, events, server)
}
