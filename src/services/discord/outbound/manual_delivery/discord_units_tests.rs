use std::sync::{Arc, Mutex};

use super::{
    DiscordOutboundClient, DispatchMessagePostError, ManualDeliveryOutcome, ManualOutboundClient,
    OutboundDeduper, UtilityBotRole, deliver_manual_dm_notification, deliver_manual_notification,
};
use crate::services::discord::formatting::discord_message_units;

#[derive(Clone, Default)]
pub(super) struct MockManualOutboundClient {
    pub(super) posts: Arc<Mutex<Vec<String>>>,
    pub(super) post_targets: Arc<Mutex<Vec<String>>>,
    pub(super) dm_resolutions: Arc<Mutex<Vec<String>>>,
}

impl DiscordOutboundClient for MockManualOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let mut posts = self.posts.lock().unwrap();
        self.post_targets
            .lock()
            .unwrap()
            .push(target_channel.to_string());
        posts.push(content.to_string());
        Ok(format!("message-{}", posts.len()))
    }

    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        self.dm_resolutions
            .lock()
            .unwrap()
            .push(user_id.to_string());
        Ok("9876".to_string())
    }
}

impl ManualOutboundClient for MockManualOutboundClient {
    async fn post_text_attachment(
        &self,
        _target_channel: &str,
        _content: &str,
        _summary: Option<&str>,
    ) -> Result<String, DispatchMessagePostError> {
        Ok("attachment-message-1".to_string())
    }
}

const CHANNEL: &str = "5998091";
const DM_USER: u64 = 42;
const ANNOUNCE: &str = "announce";

async fn send(
    client: &MockManualOutboundClient,
    content: &str,
    bot: &str,
) -> ManualDeliveryOutcome {
    let dedup = OutboundDeduper::new();
    deliver_manual_notification(
        client, &dedup, CHANNEL, content, bot, "test", None, None, false,
    )
    .await
}

async fn dm(client: &MockManualOutboundClient, content: &str, bot: &str) -> ManualDeliveryOutcome {
    let dedup = OutboundDeduper::new();
    deliver_manual_dm_notification(client, &dedup, DM_USER, content, bot, None, None).await
}

fn attachment_outcome() -> ManualDeliveryOutcome {
    ManualDeliveryOutcome::Sent {
        message_id: "attachment-message-1".to_string(),
        delivery: Some("summary+txt"),
    }
}

fn single_send_outcome() -> ManualDeliveryOutcome {
    ManualDeliveryOutcome::Sent {
        message_id: "message-1".to_string(),
        delivery: None,
    }
}

fn assert_chunked(client: &MockManualOutboundClient, outcome: ManualDeliveryOutcome, body: &str) {
    let posts = client.posts.lock().unwrap().clone();
    assert!(posts.len() >= 2, "expected chunked posts, got {posts:?}");
    assert!(
        posts
            .iter()
            .all(|chunk| discord_message_units(chunk) <= 2000)
    );
    assert_eq!(posts.concat(), body);
    assert_eq!(
        outcome,
        ManualDeliveryOutcome::Sent {
            message_id: format!("message-{}", posts.len()),
            delivery: Some("chunked"),
        }
    );
}

/// 1001 supplementary scalars encode to 2002 Discord units, so the scalar
/// count stays under 2000 while the message is over Discord's limit.
fn supplementary_over_limit() -> String {
    let body = "😀".repeat(1001);
    assert_eq!(body.chars().count(), 1001);
    assert_eq!(discord_message_units(&body), 2002);
    body
}

#[tokio::test]
async fn send_routes_supplementary_body_over_2000_units_to_attachment_or_chunks() {
    let body = supplementary_over_limit();

    let client = MockManualOutboundClient::default();
    assert_eq!(send(&client, &body, ANNOUNCE).await, attachment_outcome());
    assert!(client.posts.lock().unwrap().is_empty());

    let client = MockManualOutboundClient::default();
    let outcome = send(&client, &body, UtilityBotRole::Notify.alias()).await;
    assert_chunked(&client, outcome, &body);
}

#[tokio::test]
async fn dm_routes_supplementary_body_over_2000_units_to_attachment_or_chunks() {
    let body = supplementary_over_limit();

    let client = MockManualOutboundClient::default();
    assert_eq!(dm(&client, &body, ANNOUNCE).await, attachment_outcome());
    assert!(client.posts.lock().unwrap().is_empty());
    assert_eq!(*client.dm_resolutions.lock().unwrap(), vec!["42"]);

    let client = MockManualOutboundClient::default();
    let outcome = dm(&client, &body, UtilityBotRole::Notify.alias()).await;
    assert!(
        client
            .post_targets
            .lock()
            .unwrap()
            .iter()
            .all(|t| t == "9876")
    );
    assert_chunked(&client, outcome, &body);
}

/// 1000 supplementary scalars are exactly 2000 units: one inline post with
/// the body unchanged, not compacted to the 1900-unit safe budget.
#[tokio::test]
async fn supplementary_body_at_2000_units_is_one_unmodified_post() {
    let body = "😀".repeat(1000);
    assert_eq!(discord_message_units(&body), 2000);

    let client = MockManualOutboundClient::default();
    assert_eq!(send(&client, &body, ANNOUNCE).await, single_send_outcome());
    assert_eq!(*client.posts.lock().unwrap(), vec![body.clone()]);

    let client = MockManualOutboundClient::default();
    assert_eq!(dm(&client, &body, ANNOUNCE).await, single_send_outcome());
    assert_eq!(*client.posts.lock().unwrap(), vec![body]);
}

#[tokio::test]
async fn bmp_body_keeps_the_2000_boundary() {
    let at_limit = "가".repeat(2000);
    let over_limit = "가".repeat(2001);

    let client = MockManualOutboundClient::default();
    assert_eq!(
        send(&client, &at_limit, ANNOUNCE).await,
        single_send_outcome()
    );
    assert_eq!(*client.posts.lock().unwrap(), vec![at_limit.clone()]);

    let client = MockManualOutboundClient::default();
    assert_eq!(
        dm(&client, &at_limit, ANNOUNCE).await,
        single_send_outcome()
    );
    assert_eq!(*client.posts.lock().unwrap(), vec![at_limit]);

    let client = MockManualOutboundClient::default();
    assert_eq!(
        send(&client, &over_limit, ANNOUNCE).await,
        attachment_outcome()
    );
    assert_eq!(
        dm(&client, &over_limit, ANNOUNCE).await,
        attachment_outcome()
    );
    assert!(client.posts.lock().unwrap().is_empty());
}
