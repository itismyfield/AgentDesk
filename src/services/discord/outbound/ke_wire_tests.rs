use super::{DiscordOutboundClient, HttpOutboundClient};
use crate::services::discord::krx_tui_o_test_host::repost_io::{Mock, reply};
use serde_json::{Value, json};

#[tokio::test]
async fn p2_allowed_mentions_default_http_client_wire_keeps_users() {
    let mock = Mock::start(vec![reply("200 OK", &[], r#"{"id":"p2-wire-message"}"#)]).await;
    let content = "<@1234> @everyone @here <@&5678> 기본 알림";
    let id = HttpOutboundClient::new(reqwest::Client::new(), "test-token", &mock.base)
        .post_message("123", content)
        .await
        .unwrap();
    assert_eq!(id, "p2-wire-message");
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/channels/123/messages");
    let payload: Value = serde_json::from_slice(&requests[0].1).unwrap();
    assert_eq!(payload["content"], content);
    assert_eq!(payload["allowed_mentions"]["parse"], json!(["users"]));
}
