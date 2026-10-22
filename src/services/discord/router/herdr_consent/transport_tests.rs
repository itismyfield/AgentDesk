use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::session_host::policy::consent::card::Button;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU16, Ordering},
};

#[derive(Clone)]
struct HttpState {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    content: Arc<Mutex<String>>,
    status: Arc<AtomicU16>,
    malformed: Arc<AtomicBool>,
}

async fn respond(
    axum::extract::State(state): axum::extract::State<HttpState>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    let method = request.method().clone();
    let bytes = axum::body::to_bytes(request.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    state
        .calls
        .lock()
        .unwrap()
        .push((method.to_string(), body.clone()));
    if method == axum::http::Method::GET {
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let status = state.status.load(Ordering::SeqCst);
    if status != 200 {
        return (
            axum::http::StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({"code":50013,"message":"fixture rejection"})),
        )
            .into_response();
    }
    if state.malformed.load(Ordering::SeqCst) {
        return (axum::http::StatusCode::OK, "{invalid-json").into_response();
    }
    if let Some(content) = body.get("content").and_then(Value::as_str) {
        *state.content.lock().unwrap() = content.into();
    }
    let content = state.content.lock().unwrap().clone();
    let mut message =
        serde_json::to_value(discord_mock::user_message(940_487_400_000_051, &content)).unwrap();
    message["components"] = body.get("components").cloned().unwrap_or(json!([]));
    axum::Json(message).into_response()
}

#[tokio::test(flavor = "current_thread")]
async fn live_cards_preserve_body_components_and_send_failure_classification() {
    let state = HttpState {
        calls: Arc::new(Mutex::new(Vec::new())),
        content: Arc::new(Mutex::new(String::new())),
        status: Arc::new(AtomicU16::new(200)),
        malformed: Arc::new(AtomicBool::new(false)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(respond)
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    let mut cards = LiveCards {
        http,
        channel: serenity::ChannelId::new(discord_mock::CHANNEL_ID),
    };
    let mut card = Card {
        content: "guidance @everyone <@&123>".into(),
        buttons: vec![Button {
            label: "yes",
            custom_id: "hresume:v1:opaque:yes".into(),
            disabled: true,
        }],
    };
    let sent = cards.send(&card).await;
    assert!(
        matches!(sent, Ok(940_487_400_000_051)),
        "transport_send_message_identity_contract"
    );
    card.content = "current replacement guidance".into();
    card.buttons[0].disabled = false;
    assert!(
        cards.replace(940_487_400_000_051, &card).await.is_ok(),
        "transport_replace_card_contract"
    );
    *state.content.lock().unwrap() = "newer concurrent replacement guidance".into();
    assert!(
        cards.disable(940_487_400_000_051).await.is_ok(),
        "transport_disable_succeeds_contract"
    );
    let calls = state.calls.lock().unwrap().clone();
    assert!(
        calls.iter().all(|(method, _)| method != "GET"),
        "transport_disable_never_reads_contract"
    );
    let writes = calls;
    assert_eq!(
        writes.len(),
        3,
        "transport_send_replace_disable_write_count_contract"
    );
    assert_eq!(writes[0].1["content"], "guidance @everyone <@&123>");
    assert_eq!(
        writes[0].1["components"][0]["components"][0]["disabled"],
        true
    );
    assert_eq!(writes[1].1["content"], card.content);
    assert_eq!(
        writes[1].1["components"][0]["components"][0]["disabled"],
        false
    );
    assert!(
        writes[2].1.get("content").is_none(),
        "transport_disable_omits_content_contract"
    );
    assert_eq!(
        state.content.lock().unwrap().as_str(),
        "newer concurrent replacement guidance",
        "transport_disable_preserves_current_content_contract"
    );
    assert_eq!(
        writes[2].1["components"],
        json!([]),
        "transport_disable_empty_components_contract"
    );
    for (_, body) in writes {
        assert_eq!(
            body["allowed_mentions"]["parse"],
            json!(["users"]),
            "transport_shared_mention_policy_contract"
        );
    }
    for status in [400, 401, 403, 404] {
        state.status.store(status, Ordering::SeqCst);
        assert!(
            matches!(cards.send(&card).await, Err(SendFailure::Definitive(_))),
            "transport_definitive_http_status_contract: {status}"
        );
        assert!(
            matches!(
                cards.replace(940_487_400_000_051, &card).await,
                Err(SendFailure::Definitive(_))
            ),
            "transport_replace_definitive_http_status_contract: {status}"
        );
        assert!(
            matches!(
                cards.disable(940_487_400_000_051).await,
                Err(SendFailure::Definitive(_))
            ),
            "transport_disable_definitive_http_status_contract: {status}"
        );
    }
    state.status.store(418, Ordering::SeqCst);
    assert!(
        matches!(cards.send(&card).await, Err(SendFailure::Unknown(_))),
        "transport_unknown_http_status_contract"
    );
    assert!(
        matches!(
            cards.replace(940_487_400_000_051, &card).await,
            Err(SendFailure::Unknown(_))
        ),
        "transport_replace_unknown_http_status_contract"
    );
    assert!(
        matches!(
            cards.disable(940_487_400_000_051).await,
            Err(SendFailure::Unknown(_))
        ),
        "transport_disable_unknown_http_status_contract"
    );
    state.status.store(200, Ordering::SeqCst);
    state.malformed.store(true, Ordering::SeqCst);
    assert!(
        matches!(cards.send(&card).await, Err(SendFailure::Unknown(_))),
        "transport_unknown_response_contract"
    );
    assert!(
        matches!(
            cards.replace(940_487_400_000_051, &card).await,
            Err(SendFailure::Unknown(_))
        ),
        "transport_replace_unknown_response_contract"
    );
    assert!(
        matches!(
            cards.disable(940_487_400_000_051).await,
            Err(SendFailure::Unknown(_))
        ),
        "transport_disable_unknown_response_contract"
    );
    server.abort();
}
