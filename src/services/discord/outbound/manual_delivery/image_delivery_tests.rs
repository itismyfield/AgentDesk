use super::production_nonce_tests::discord_message_json;
use super::*;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type WirePayload = (Value, Vec<u8>);

#[derive(Clone, Default)]
struct Wire {
    payloads: Arc<Mutex<Vec<WirePayload>>>,
    messages: Arc<Mutex<BTreeMap<String, u64>>>,
    lose_next_receipt: Arc<std::sync::atomic::AtomicBool>,
}

async fn rest(State(state): State<Wire>, req: Request<Body>) -> Response {
    if req.method() != axum::http::Method::POST
        || req.uri().path() != "/api/v10/channels/7001/messages"
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = req
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = axum::body::to_bytes(req.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    let payload = if content_type.starts_with("multipart/") {
        let text = String::from_utf8_lossy(&body);
        let part = text
            .find("name=\"payload_json\"")
            .expect("multipart payload JSON part");
        let start = part + text[part..].find("\r\n\r\n").unwrap() + 4;
        let end = start + text[start..].find("\r\n--").unwrap();
        serde_json::from_str::<Value>(&text[start..end]).unwrap()
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let nonce = payload
        .get("nonce")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let content = payload["content"].as_str().unwrap_or("").to_owned();
    let id = {
        let mut messages = state.messages.lock().unwrap();
        let next = 9000 + messages.len() as u64;
        let key = if payload["enforce_nonce"] == true {
            nonce.unwrap()
        } else {
            format!("unprotected-{next}")
        };
        *messages.entry(key).or_insert(next)
    };
    state.payloads.lock().unwrap().push((payload, body));
    if state
        .lose_next_receipt
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"code":50013,"message":"receipt lost after acceptance"})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(discord_message_json(id, &content))).into_response()
}

async fn client() -> (
    Wire,
    SerenityManualOutboundClient,
    tokio::task::JoinHandle<()>,
) {
    let state = Wire::default();
    let app = Router::new().fallback(any(rest)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = Arc::new(
        serenity::all::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    (state, SerenityManualOutboundClient { http }, task)
}

async fn send_image(
    client: &SerenityManualOutboundClient,
    dedup: &OutboundDeduper,
    image: crate::services::message_outbox::OutboxAttachment<'_>,
    correlation: &str,
    semantic: &str,
) -> (&'static str, String) {
    send_resolved_manual_message(
        client,
        dedup,
        ResolvedManualMessage {
            channel_id_raw: 7001,
            target: "channel:7001",
            pg_pool: None,
            record_transcript: false,
            transcript_source_label: None,
            notification: ManualNotification {
                channel_id: "7001",
                content: "image body 한국어",
                bot: "notify",
                source: "scheduled_message",
                summary: None,
                delivery_id: Some(ManualOutboundDeliveryId {
                    correlation_id: correlation,
                    semantic_event_id: semantic,
                }),
                headless_nonce_enabled: false,
                attachment: Some(image),
            },
        },
    )
    .await
}

#[tokio::test]
async fn binary_wire_nonce_dedupes_retries_across_process_memory_loss() {
    let (state, client, task) = client().await;
    let data = b"\x89PNG\r\n\x1a\n\0\xffexact upload bytes";
    let image = crate::services::message_outbox::OutboxAttachment {
        filename: "image.png",
        content_type: "image/png",
        data,
    };
    let mut ids = Vec::new();
    for (correlation, semantic) in [
        (
            "message_outbox:scheduled_message:first:channel:7001",
            "message_outbox:41:deliver",
        ),
        (
            "message_outbox:scheduled_message:retry:channel:7001",
            "message_outbox:041:deliver",
        ),
        (
            "message_outbox:scheduled_message:next:channel:7001",
            "message_outbox:42:deliver",
        ),
    ] {
        // A new in-memory deduper models a process restart or a lost HTTP receipt.
        let (status, body) = send_image(
            &client,
            &OutboundDeduper::new(),
            image,
            correlation,
            semantic,
        )
        .await;
        assert_eq!(status, "200 OK", "wire delivery failed: {body}");
        ids.push(serde_json::from_str::<Value>(&body).unwrap()["message_id"].clone());
    }
    let payloads = state.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 3, "all retry attempts must reach the wire");
    let expected = crate::services::discord::outbound::outbound_fingerprint(&[
        "scheduled-image-message-outbox-delivery",
        "41",
    ]);
    assert_eq!(
        payloads[0].0["nonce"], expected,
        "binary upload must connect the durable outbox nonce"
    );
    assert_eq!(payloads[1].0["nonce"], expected);
    assert_ne!(payloads[2].0["nonce"], expected);
    for (payload, body) in payloads.iter() {
        assert_eq!(
            payload["enforce_nonce"], true,
            "retry dedupe must be enforced at Discord"
        );
        assert_eq!(payload["content"], "image body 한국어");
        assert!(
            body.windows(data.len()).any(|v| v == data),
            "multipart upload must preserve binary bytes"
        );
        assert!(String::from_utf8_lossy(body).contains("filename=\"image.png\""));
        assert!(String::from_utf8_lossy(body).contains("image/png"));
    }
    assert_eq!(
        ids[0], ids[1],
        "a restarted worker must not create a second Discord message"
    );
    assert_ne!(ids[0], ids[2]);
    assert_eq!(state.messages.lock().unwrap().len(), 2);
    task.abort();
}

#[tokio::test]
async fn manual_text_compatibility_wrapper_keeps_wire_bytes() {
    let (state, client, task) = client().await;
    let dedup = OutboundDeduper::new();
    let (status, _) = send_resolved_manual_message_with_client(
        &client,
        &dedup,
        7001,
        "channel:7001",
        "text 한국어",
        "scheduled_message",
        "notify",
        None,
        None,
        None,
        false,
        None,
        false,
    )
    .await;
    assert_eq!(status, "200 OK");
    let (status, _) = send_resolved_manual_message(
        &client,
        &dedup,
        ResolvedManualMessage {
            channel_id_raw: 7001,
            target: "channel:7001",
            pg_pool: None,
            record_transcript: false,
            transcript_source_label: None,
            notification: ManualNotification {
                channel_id: "7001",
                content: "text 한국어",
                bot: "notify",
                source: "scheduled_message",
                summary: None,
                delivery_id: None,
                headless_nonce_enabled: false,
                attachment: None,
            },
        },
    )
    .await;
    assert_eq!(status, "200 OK");
    let payloads = state.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 2);
    assert_eq!(
        payloads[0].1, payloads[1].1,
        "legacy manual API must produce identical text-only request bytes"
    );
    assert!(payloads[0].0.get("nonce").is_none());
    assert_eq!(payloads[0].0["enforce_nonce"], false);
    task.abort();
}

#[tokio::test]
async fn image_without_durable_identity_never_falls_back_to_text() {
    let client = super::discord_units_tests::MockManualOutboundClient::default();
    let data = b"image";
    let result = deliver_manual_notification(
        &client,
        &OutboundDeduper::new(),
        ManualNotification {
            channel_id: "7001",
            content: "text",
            bot: "notify",
            source: "scheduled_message",
            summary: None,
            delivery_id: None,
            headless_nonce_enabled: false,
            attachment: Some(crate::services::message_outbox::OutboxAttachment {
                filename: "image.png",
                content_type: "image/png",
                data,
            }),
        },
    )
    .await;
    assert!(matches!(result, ManualDeliveryOutcome::Failed { .. }));
    assert!(
        client.posts.lock().unwrap().is_empty(),
        "invalid image identity must make zero text sends"
    );
}

#[test]
fn binary_error_logging_uses_display_only() {
    for source in [
        include_str!("image_delivery.rs"),
        include_str!("../send_gate.rs"),
        include_str!("../../../message_outbox.rs"),
    ] {
        assert!(
            !source.contains("{error:?}"),
            "image errors must not log PG DETAIL with Debug"
        );
        assert!(
            !source.contains("error = ?error"),
            "image errors must use Display fields"
        );
    }
}

#[tokio::test]
async fn lost_binary_receipt_retries_with_same_nonce_and_reservation() {
    let (state, client, task) = client().await;
    state
        .lose_next_receipt
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let image = crate::services::message_outbox::OutboxAttachment {
        filename: "image.png",
        content_type: "image/png",
        data: b"binary retry bytes",
    };
    let dedup = OutboundDeduper::new();
    let correlation = "message_outbox:scheduled_message:retry:channel:7001";
    let semantic = "message_outbox:99:deliver";
    let failed = send_image(&client, &dedup, image, correlation, semantic).await;
    assert_eq!(
        failed.0, "500 Internal Server Error",
        "the lost receipt must really fail the first attempt"
    );
    let retried = send_image(&client, &dedup, image, correlation, semantic).await;
    assert_eq!(
        retried.0, "200 OK",
        "failed in-memory reservation must permit retry"
    );
    let payloads = state.payloads.lock().unwrap();
    assert_eq!(
        payloads.len(),
        2,
        "retry must actually reach binary upload again"
    );
    assert_eq!(payloads[0].0["nonce"], payloads[1].0["nonce"]);
    assert_eq!(
        state.messages.lock().unwrap().len(),
        1,
        "Discord nonce must reuse the accepted message after a lost receipt"
    );
    task.abort();
}
