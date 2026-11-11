use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};

use crate::domain::outbound::{
    DeliveryResult, DiscordOutboundPolicy, OutboundDelivery, OutboundIntent, OutboundTargetKey,
    first_raw_message_id,
};
use crate::services::discord::outbound::HttpOutboundClient;

async fn create(State(posts): State<Arc<AtomicUsize>>, Json(_): Json<Value>) -> Json<Value> {
    let n = posts.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({ "id": format!("{}", 9000 + n) }))
}

#[tokio::test(flavor = "current_thread")]
async fn same_intent_from_two_http_clients_posts_once_and_replays_the_receipt() {
    let posts = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/channels/{channel}/messages", post(create))
        .with_state(posts.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let correlation = format!("port-dedup:{}", uuid::Uuid::new_v4());
    let intent = || {
        OutboundIntent::new(
            correlation.clone(),
            "summary",
            "body",
            OutboundTargetKey::channel(4242),
            DiscordOutboundPolicy::review_notification(),
        )
    };
    let first = HttpOutboundClient::new(reqwest::Client::new(), "token-a", base.clone())
        .deliver(intent())
        .await;
    let second = HttpOutboundClient::new(reqwest::Client::new(), "token-b", base)
        .deliver(intent())
        .await;
    server.abort();

    let DeliveryResult::Sent { messages, .. } = first else {
        panic!("first delivery should post: {first:?}");
    };
    let DeliveryResult::Duplicate {
        existing_messages, ..
    } = second
    else {
        panic!("second delivery should replay the receipt: {second:?}");
    };
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    assert_eq!(first_raw_message_id(&messages).as_deref(), Some("9001"));
    assert_eq!(
        first_raw_message_id(&existing_messages).as_deref(),
        Some("9001")
    );
}
