//! A refused busy follow-up names its refusal on the placeholder, or without one as a reply.
use super::*;
use crate::services::discord::health::test_support::{MockDiscord, message_json};
use axum::http::Method;

#[tokio::test]
async fn a_refused_input_without_a_placeholder_learns_why_as_a_reply() {
    let channel = 7_507_301_u64;
    let messages = format!("/api/v10/channels/{channel}/messages");
    let answer = messages.clone();
    let mock = MockDiscord::start_with(std::sync::Arc::new(move |_: &Method, path: &str| {
        path.starts_with(&answer).then(|| {
            (
                200,
                message_json(7_507_399, channel, 1, "", "2026-10-29T00:00:00+00:00"),
            )
        })
    }))
    .await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (channel_id, user) = (ChannelId::new(channel), MessageId::new(7_507_302));
    let notice = claude_tui_busy_followup_refusal_notice(None);
    for placeholder in [None, Some(MessageId::new(7_507_303))] {
        apply_tui_busy_enqueue_refusal(
            &shared,
            &mock.http,
            &crate::services::provider::ProviderKind::Claude,
            (channel_id, user, placeholder),
            None,
            None,
            None,
            None,
        )
        .await;
    }
    let requests = mock.successful_requests_for(channel);
    assert_eq!(requests.len(), 2, "{requests:?}");
    let reply = &requests[0];
    assert!(reply.starts_with(&format!("POST {messages} ")), "{reply}");
    let body: serde_json::Value =
        serde_json::from_str(reply.splitn(3, ' ').nth(2).unwrap_or_default()).unwrap();
    assert_eq!(body["content"], notice, "{reply}");
    assert_eq!(
        body["message_reference"]["message_id"], "7507302",
        "a reply to the input: {reply}"
    );
    assert!(
        requests[1].starts_with(&format!("PATCH {messages}/7507303 ")),
        "a placeholder still carries the refusal: {requests:?}"
    );
}
