use super::*;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use poise::serenity_prelude::{ChannelId, HttpBuilder};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Default)]
struct Wire {
    messages: Arc<Mutex<BTreeMap<String, u64>>>,
    payloads: Arc<Mutex<Vec<Value>>>,
    lose_receipt: Arc<AtomicBool>,
}

async fn rest(State(wire): State<Wire>, request: Request<Body>) -> Response {
    assert_eq!(request.uri().path(), "/api/v10/channels/7001/messages");
    let bytes = axum::body::to_bytes(request.into_body(), 8192)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&bytes).unwrap();
    let id = {
        let mut messages = wire.messages.lock().unwrap();
        let next = 9000 + messages.len() as u64;
        let key = if payload["enforce_nonce"] == true {
            payload["nonce"].as_str().unwrap().to_owned()
        } else {
            format!("unprotected-{next}")
        };
        *messages.entry(key).or_insert(next)
    };
    wire.payloads.lock().unwrap().push(payload.clone());
    if wire.lose_receipt.swap(false, Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code":50013,"message":"fixture lost receipt after acceptance"})),
        )
            .into_response();
    }
    Json(json!({
        "id":id.to_string(), "channel_id":"7001",
        "author":{"id":"8001","username":"fixture-bot","discriminator":"0001","avatar":null,"bot":true},
        "content":payload["content"],"timestamp":"2026-10-26T00:00:00+00:00",
        "edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],
        "mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0
    })).into_response()
}

#[tokio::test]
async fn routing_notice_wire_keeps_nonce_after_lost_receipt_and_memory_restart() {
    let wire = Wire::default();
    wire.lose_receipt.store(true, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(any(rest)).with_state(wire.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let identity = OwnerIdentity::new("codex", "7001");
    let event = Transition {
        identity: &identity,
        generation: 3,
        from: "A",
        to: "B",
        home: "A",
    };
    let mut ids = Vec::new();
    for attempt in 0..3 {
        let notice = prepare(
            &event,
            Labels {
                home: "작업실",
                to: "휴대 장치",
            },
            true,
            false,
        )
        .unwrap()
        .unwrap();
        let http = HttpBuilder::new("fixture-token")
            .proxy(proxy.clone())
            .ratelimiter_disabled(true)
            .build();
        let result = ChannelId::new(7001)
            .send_message(&http, notice.discord_message())
            .await;
        if attempt == 0 {
            assert!(result.is_err());
        } else {
            ids.push(result.unwrap().id.get());
        }
    }
    server.abort();
    let _ = server.await;
    assert_eq!(ids, vec![9000, 9000]);
    assert_eq!(wire.messages.lock().unwrap().len(), 1);
    let payloads = wire.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 3);
    assert!(
        payloads
            .iter()
            .all(|value| value["nonce"] == payloads[0]["nonce"] && value["enforce_nonce"] == true)
    );
}
