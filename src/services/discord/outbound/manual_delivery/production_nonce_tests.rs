use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{ManualOutboundDeliveryId, send_resolved_manual_message_with_client_and_nonce_rollout};
use crate::services::discord::outbound::send_gate::production_manual_outbound_client;
use crate::services::discord::outbound::{OutboundDeduper, outbound_fingerprint};

const CHANNEL_ID: u64 = 7001;
const BOT_ID: u64 = 8002;

#[derive(Clone, Default)]
struct WireState {
    payloads: Arc<Mutex<Vec<Value>>>,
}

fn discord_user_json() -> Value {
    json!({
        "id": BOT_ID.to_string(),
        "username": "nonce-test-bot",
        "discriminator": "0001",
        "global_name": null,
        "avatar": null,
        "bot": true,
        "system": false,
        "mfa_enabled": false,
        "banner": null,
        "accent_color": null,
        "locale": "en-US",
        "verified": true,
        "email": null,
        "flags": 0,
        "premium_type": 0,
        "public_flags": 0,
        "member": null,
        "primary_guild": null,
        "avatar_decoration_data": null,
        "collectibles": null
    })
}

fn discord_message_json(id: u64, content: &str) -> Value {
    json!({
        "id": id.to_string(),
        "channel_id": CHANNEL_ID.to_string(),
        "author": discord_user_json(),
        "content": content,
        "timestamp": "2026-08-24T00:00:00.000000+00:00",
        "edited_timestamp": null,
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "mention_channels": [],
        "attachments": [],
        "embeds": [],
        "reactions": [],
        "nonce": null,
        "pinned": false,
        "webhook_id": null,
        "type": 0,
        "activity": null,
        "application": null,
        "application_id": null,
        "message_reference": null,
        "flags": 0,
        "referenced_message": null,
        "message_snapshots": [],
        "interaction": null,
        "interaction_metadata": null,
        "thread": null,
        "components": [],
        "sticker_items": [],
        "position": null,
        "role_subscription_data": null,
        "guild_id": null,
        "member": null,
        "poll": null
    })
}

async fn discord_rest(State(state): State<WireState>, request: Request<Body>) -> Response {
    if request.method() != Method::POST
        || request.uri().path() != format!("/api/v10/channels/{CHANNEL_ID}/messages")
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .expect("read create-message body");
    let payload: Value = serde_json::from_slice(&body).expect("serialized Discord JSON");
    let content = payload["content"].as_str().unwrap_or_default().to_string();
    let id = {
        let mut payloads = state.payloads.lock().expect("payload lock");
        payloads.push(payload);
        9000 + payloads.len() as u64
    };
    Json(discord_message_json(id, &content)).into_response()
}

#[tokio::test]
async fn production_serenity_inline_path_serializes_canonical_durable_row_nonces() {
    let state = WireState::default();
    let payloads = state.payloads.clone();
    let app = Router::new().fallback(any(discord_rest)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Discord proxy");
    let proxy = format!("http://{}", listener.local_addr().expect("proxy address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve Discord proxy");
    });
    let http = Arc::new(
        serenity::all::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    let client = production_manual_outbound_client(http);

    async fn send(
        client: &super::SerenityManualOutboundClient,
        source: &str,
        row_id_spelling: &str,
        correlation_variant: &str,
        content: &str,
        enabled: bool,
    ) {
        let correlation_id =
            format!("message_outbox:headless_turn:{correlation_variant}:session-{row_id_spelling}");
        let semantic_event_id = format!("message_outbox:{row_id_spelling}:deliver");
        // A fresh deduper bypasses process-local suppression so a second
        // attempt reaches Discord. This proves a stable wire nonce for
        // Discord's bounded recent-nonce retention window; it does not claim
        // indefinite deduplication across an arbitrarily late restart.
        let dedup = OutboundDeduper::new();
        let result = send_resolved_manual_message_with_client_and_nonce_rollout(
            client,
            &dedup,
            CHANNEL_ID,
            &format!("channel:{CHANNEL_ID}"),
            content,
            source,
            "notify",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &correlation_id,
                semantic_event_id: &semantic_event_id,
            }),
            None,
            false,
            None,
            enabled,
            None,
        )
        .await;
        assert_eq!(result.0, "200 OK");
    }

    send(
        &client,
        "headless_turn",
        "41",
        "terminal-a",
        "first content",
        true,
    )
    .await;
    send(
        &client,
        "headless_turn",
        "041",
        "retry-b",
        "changed retry content",
        true,
    )
    .await;
    send(
        &client,
        "headless_turn",
        "42",
        "terminal-a",
        "distinct row",
        true,
    )
    .await;
    send(
        &client,
        "headless_turn",
        "43",
        "terminal-a",
        "rollout disabled",
        false,
    )
    .await;
    send(
        &client,
        "lifecycle_notifier",
        "44",
        "terminal-a",
        "wrong source",
        true,
    )
    .await;
    server.abort();

    let payloads = payloads.lock().expect("payload lock");
    assert_eq!(payloads.len(), 5);
    let expected_41 = outbound_fingerprint(&["headless-message-outbox-delivery", "41"]);
    let expected_42 = outbound_fingerprint(&["headless-message-outbox-delivery", "42"]);
    assert_eq!(payloads[0]["nonce"], expected_41);
    assert_eq!(payloads[0]["enforce_nonce"], true);
    assert_eq!(payloads[1]["nonce"], expected_41);
    assert_eq!(payloads[1]["enforce_nonce"], true);
    assert_eq!(payloads[2]["nonce"], expected_42);
    assert_eq!(payloads[2]["enforce_nonce"], true);
    assert_eq!(payloads[0]["nonce"], payloads[1]["nonce"]);
    assert_ne!(payloads[0]["nonce"], payloads[2]["nonce"]);
    for payload in &payloads[3..] {
        assert!(payload.get("nonce").is_none());
        assert_eq!(payload.get("enforce_nonce"), Some(&Value::Bool(false)));
    }
}
