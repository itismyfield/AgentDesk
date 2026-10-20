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
async fn voice_announce_same_utterance_and_generation_dedupes_at_health_layer() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let voice_id = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-2363-a",
        default_voice_announce_generation(),
    );
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: &voice_id.correlation_id,
        semantic_event_id: &voice_id.semantic_event_id,
    };

    // Issue #2363: announce send retried with identical
    // (guild, voice_channel, utterance, generation) must hit the dedupe
    // path and not produce a second Discord call.
    let first = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "voice transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(delivery_id),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let second = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "voice transcript",
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
        first,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: None,
        }
    );
    assert_eq!(
        second,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: Some("duplicate"),
        }
    );
    assert_eq!(client.posts.lock().unwrap().len(), 1);
    assert_eq!(
        voice_id.correlation_id,
        "voice:7001:8002:utt-2363-a".to_string()
    );
    assert_eq!(
        voice_id.semantic_event_id,
        "announce:generation:1".to_string()
    );
}

#[tokio::test]
async fn voice_announce_new_utterance_does_not_dedupe_against_prior_send() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let first_id = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-A",
        default_voice_announce_generation(),
    );
    let second_id = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-B",
        default_voice_announce_generation(),
    );

    let first = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "first transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(ManualOutboundDeliveryId {
                correlation_id: &first_id.correlation_id,
                semantic_event_id: &first_id.semantic_event_id,
            }),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let second = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "second transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(ManualOutboundDeliveryId {
                correlation_id: &second_id.correlation_id,
                semantic_event_id: &second_id.semantic_event_id,
            }),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;

    assert_eq!(
        first,
        ManualDeliveryOutcome::Sent {
            message_id: "message-1".to_string(),
            delivery: None,
        }
    );
    assert_eq!(
        second,
        ManualDeliveryOutcome::Sent {
            message_id: "message-2".to_string(),
            delivery: None,
        }
    );
    assert_eq!(client.posts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn voice_announce_generation_bump_breaks_dedupe_for_same_utterance() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    // Same (guild, voice_channel, utterance) but a higher generation
    // (e.g. a barge-in follow-up) must NOT dedupe against the original
    // announce — different `semantic_event_id = announce:generation:{n}`.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let gen_one = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-shared",
        default_voice_announce_generation(),
    );
    let gen_two = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-shared",
        default_voice_announce_generation() + 1,
    );
    assert_eq!(gen_one.correlation_id, gen_two.correlation_id);
    assert_ne!(gen_one.semantic_event_id, gen_two.semantic_event_id);

    let first = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "transcript",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(ManualOutboundDeliveryId {
                correlation_id: &gen_one.correlation_id,
                semantic_event_id: &gen_one.semantic_event_id,
            }),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let second = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "transcript with barge-in follow-up",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(ManualOutboundDeliveryId {
                correlation_id: &gen_two.correlation_id,
                semantic_event_id: &gen_two.semantic_event_id,
            }),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;

    assert!(matches!(
        first,
        ManualDeliveryOutcome::Sent { delivery: None, .. }
    ));
    assert!(matches!(
        second,
        ManualDeliveryOutcome::Sent { delivery: None, .. }
    ));
    assert_eq!(client.posts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn voice_announce_cross_guild_isolation_prevents_dedupe_collision() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    // Two different guilds happen to use the same utterance_id (they're
    // independent generators) — must NOT dedupe against each other.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let guild_a = voice_announce_delivery_id(
        GuildId::new(1),
        ChannelId::new(10),
        "utt-collide",
        default_voice_announce_generation(),
    );
    let guild_b = voice_announce_delivery_id(
        GuildId::new(2),
        ChannelId::new(10),
        "utt-collide",
        default_voice_announce_generation(),
    );
    assert_ne!(guild_a.correlation_id, guild_b.correlation_id);

    let first = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "from guild 1",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(ManualOutboundDeliveryId {
                correlation_id: &guild_a.correlation_id,
                semantic_event_id: &guild_a.semantic_event_id,
            }),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;
    let second = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "from guild 2",
            bot: "announce",
            source: "manual_test",
            summary: None,
            delivery_id: Some(ManualOutboundDeliveryId {
                correlation_id: &guild_b.correlation_id,
                semantic_event_id: &guild_b.semantic_event_id,
            }),
            headless_nonce_enabled: false,
            attachment: None,
        },
    )
    .await;

    assert!(matches!(
        first,
        ManualDeliveryOutcome::Sent { delivery: None, .. }
    ));
    assert!(matches!(
        second,
        ManualDeliveryOutcome::Sent { delivery: None, .. }
    ));
    assert_eq!(client.posts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn voice_announce_same_delivery_id_different_target_channel_does_not_dedupe() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    // Issue #2363 (Codex high-severity finding): the dedupe key must
    // include the resolved target channel. Otherwise an announce queued
    // first to a transcript channel and later re-routed to a different
    // target channel for the same (guild, voice_channel, utterance,
    // generation) tuple would be silently suppressed as a duplicate.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let voice_id = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-cross-target",
        default_voice_announce_generation(),
    );
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: &voice_id.correlation_id,
        semantic_event_id: &voice_id.semantic_event_id,
    };

    let to_channel_a = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "voice transcript",
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
            content: "voice transcript",
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
async fn voice_announce_different_bot_does_not_dedupe_against_announce() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    // Issue #2363 (Codex high-severity finding round 2): scoping must
    // also include the producer `bot`. Without this an external
    // `/api/discord/send` caller could send through `notify` with a
    // crafted `voice:{guild}:{voice_channel}:{utterance}` delivery id
    // and silently poison the voice announce path.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let voice_id = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-cross-bot",
        default_voice_announce_generation(),
    );
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: &voice_id.correlation_id,
        semantic_event_id: &voice_id.semantic_event_id,
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
            content: "voice transcript",
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
async fn api_send_rejects_user_supplied_voice_delivery_id_namespace() {
    let registry = HealthRegistry::new();

    for correlation_id in [
        "voice:7001:8002:utt-forged",
        "message_outbox:headless_turn:terminal:forged",
    ] {
        let body = serde_json::json!({
            "target": "channel:9000",
            "content": "forged internal delivery",
            "source": "headless_turn",
            "bot": "notify",
            "correlation_id": correlation_id,
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
}

#[tokio::test]
async fn voice_announce_duplicate_surfaces_prior_message_id_not_empty() {
    use crate::services::discord::voice_background_driver::{
        default_voice_announce_generation, voice_announce_delivery_id,
    };
    use poise::serenity_prelude::{ChannelId, GuildId};

    // Issue #2363 (Codex high-severity finding): the duplicate path must
    // return the prior delivered message id. The announce-bot driver
    // (`AnnounceBotTranscriptDriver::start`) parses `message_id` from
    // the response body and errors out on empty / non-numeric values.
    let client = MockManualOutboundClient::default();
    let dedup = OutboundDeduper::new();
    let voice_id = voice_announce_delivery_id(
        GuildId::new(7001),
        ChannelId::new(8002),
        "utt-known-id",
        default_voice_announce_generation(),
    );
    let delivery_id = ManualOutboundDeliveryId {
        correlation_id: &voice_id.correlation_id,
        semantic_event_id: &voice_id.semantic_event_id,
    };

    let _ = deliver_manual_notification(
        &client,
        &dedup,
        super::ManualNotification {
            channel_id: "9000",
            content: "voice transcript",
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
            content: "voice transcript",
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
