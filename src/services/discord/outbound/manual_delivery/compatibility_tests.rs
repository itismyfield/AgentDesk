use super::*;

use super::discord_units_tests::MockManualOutboundClient;
use crate::services::discord::health::{HealthRegistry, handle_send};

#[test]
fn synthetic_routine_pair_is_complete_and_message_id_deterministic() {
    let first =
        synthetic_routine_pair(42, "message-7", "posted briefing", Some("morning-briefing"));
    let retry =
        synthetic_routine_pair(42, "message-7", "posted briefing", Some("morning-briefing"));

    assert_eq!(first.0, "manual-discord:42:message-7");
    assert_eq!(first.0, retry.0);
    assert_eq!(first.1, "(routine morning-briefing posted)");
    assert!(!first.1.trim().is_empty());
    assert_eq!(first.2, "posted briefing");
    assert!(!first.2.trim().is_empty());
}

#[test]
fn synthetic_routine_pair_uses_non_empty_default_label() {
    let pair = synthetic_routine_pair(42, "message-8", "body", Some("  "));
    assert_eq!(pair.1, "(routine routine posted)");
}

#[test]
fn manual_delivery_log_emoji_preserves_legacy_mapping() {
    assert_eq!(
        manual_delivery_log_emoji(UtilityBotRole::Announce.alias()),
        "📨"
    );
    assert_eq!(
        manual_delivery_log_emoji(UtilityBotRole::Notify.alias()),
        "🔔"
    );
    assert_eq!(manual_delivery_log_emoji("provider"), "📨");
}

// Contract: a custody notice for a DM under a long runtime root is one post from the provider
// bot that keeps the fixed line, the episode id and the failed-copy line.
#[tokio::test]
async fn a_custody_notice_under_a_long_root_is_one_provider_bot_post() {
    let (client, root) = (MockManualOutboundClient::default(), "r".repeat(3000));
    let dir = std::path::Path::new("/")
        .join(root)
        .join("discord_custody/claude");
    let dir = dir.join("e".repeat(64));
    let text = crate::services::discord::inflight::custody_notice_text(&dir, "claude");
    let (dedup, source) = (OutboundDeduper::new(), "boot_custody_notice");
    deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "5998091",
            content: &text,
            bot: "claude",
            source,
            summary: None,
            delivery_id: None,
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let (posts, episode) = (client.posts.lock().unwrap().clone(), "e".repeat(64));
    let kept = ["재시작으로 이 턴 출력 일부가", &episode, "보존 실패"];
    assert_eq!(posts.len(), 1);
    assert!(kept.iter().all(|line| posts[0].contains(line)), "{posts:?}");
}

#[tokio::test]
async fn manual_dm_notification_uses_v3_dm_target_and_dedupes_before_resolve() {
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: "senddm:42",
        semantic_event_id: "senddm:42:hello",
    };

    let first = deliver_manual_dm_notification(
        &client,
        &dedup,
        42,
        "hello",
        "announce",
        None,
        Some(delivery_id),
    )
    .await;
    let second = deliver_manual_dm_notification(
        &client,
        &dedup,
        42,
        "hello",
        "announce",
        None,
        Some(delivery_id),
    )
    .await;

    assert_eq!(
        first,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: None
        }
    );
    assert_eq!(
        second,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: Some("duplicate")
        }
    );
    assert_eq!(
        client.dm_resolutions.lock().unwrap().clone(),
        vec!["42".to_string()]
    );
    assert_eq!(
        client.post_targets.lock().unwrap().clone(),
        vec!["9876".to_string()]
    );
    assert_eq!(
        client.posts.lock().unwrap().clone(),
        vec!["hello".to_string()]
    );
}

#[tokio::test]
async fn manual_same_delivery_id_different_target_channel_does_not_dedupe() {
    // The dedupe key includes the resolved target channel, so the same delivery id
    // sent to another channel is a new delivery.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: "manual:9000:utt-cross-target",
        semantic_event_id: "deliver:1",
    };

    let to_channel_a = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let to_channel_b = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9001",
            content: "transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;

    assert_eq!(
        to_channel_a,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: None,
        }
    );
    assert_eq!(
        to_channel_b,
        ManualDeliveryOutcome::Sent {
            message_id: "message-2".to_string(),
            delivery: None,
        }
    );
    assert_eq!(client.posts.lock().unwrap().len(), 2);
    assert_eq!(
        client.post_targets.lock().unwrap().clone(),
        vec!["9000".to_string(), "9001".to_string()]
    );
}

#[tokio::test]
async fn manual_same_delivery_id_different_bot_does_not_dedupe() {
    // The dedupe key includes the producer bot, so one bot's delivery id cannot
    // suppress another bot's send.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: "manual:9000:utt-cross-bot",
        semantic_event_id: "deliver:1",
    };

    let from_notify = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "notify payload",
            bot: "notify",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let from_announce = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;

    assert_eq!(
        from_notify,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: None,
        }
    );
    assert_eq!(
        from_announce,
        ManualDeliveryOutcome::Sent {
            message_id: "message-2".to_string(),
            delivery: None,
        }
    );
    assert_eq!(client.posts.lock().unwrap().len(), 2);
}

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

#[tokio::test]
async fn manual_duplicate_surfaces_prior_message_id_not_empty() {
    // The duplicate path returns the prior delivered message id for callers that parse it.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: "manual:9000:utt-known-id",
        semantic_event_id: "deliver:1",
    };

    let _ = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let dup = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;

    match dup {
        ManualDeliveryOutcome::Sent {
            message_id,
            delivery,
        } => {
            assert!(
                !message_id.is_empty(),
                "duplicate must return prior message id, not empty"
            );
            assert!(
                message_id
                    .chars()
                    .all(|c| c == '-' || c.is_ascii_alphanumeric()),
                "message_id should be parseable by callers"
            );
            assert_eq!(delivery, Some("duplicate"));
        }
        other => panic!("expected Sent(duplicate), got {other:?}"),
    }
}
