use crate::services::discord::health::HealthRegistry;
use crate::services::discord::outbound::send_api::handle_send;

#[tokio::test]
async fn api_send_rejects_user_supplied_message_outbox_delivery_id_namespace() {
    let registry = HealthRegistry::new();

    let body = serde_json::json!({
        "target": "channel:9000",
        "content": "forged internal delivery",
        "source": "headless_turn",
        "bot": "notify",
        "correlation_id": "message_outbox:headless_turn:terminal:forged",
        "semantic_event_id": "message_outbox:41:deliver"
    })
    .to_string();
    let (status, body) = handle_send(&registry, None, &body).await;
    let response: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(status, "400 Bad Request");
    assert_eq!(response["ok"], false);
    assert_eq!(
        response["error"],
        "delivery_id correlation namespace is reserved"
    );
}
