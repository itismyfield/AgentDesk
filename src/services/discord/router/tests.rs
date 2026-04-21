use super::super::model_picker_interaction::build_model_picker_close_response;
use super::super::should_process_allowed_bot_turn_text;
use super::intake_gate::{
    RemovedControlReaction, classify_removed_control_reaction, content_has_explicit_user_mention,
    is_model_picker_component_custom_id, should_process_turn_message,
    should_skip_for_missing_required_mention,
};
use super::message_handler::{
    TextStopLookup, TurnKind, classify_turn_kind_from_author, lookup_text_stop_token,
};
use crate::services::discord::DiscordBotSettings;
use crate::services::provider::CancelToken;
use poise::serenity_prelude::ChannelId;
use serde_json::json;
use serenity::model::channel::{MessageType, ReactionType};
use serenity::model::id::UserId;
use serenity::model::prelude::MessageId;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

// Re-import the private helper for testing
fn should_skip_human_slash_message(
    content: &str,
    known_slash_commands: Option<&std::collections::HashSet<String>>,
) -> bool {
    if !content.starts_with('/') {
        return false;
    }

    let command_name = content[1..].split_whitespace().next().unwrap_or("");
    if command_name.is_empty() {
        return false;
    }

    known_slash_commands.is_some_and(|set| set.contains(command_name))
}

#[test]
fn turn_messages_allow_regular_and_inline_reply() {
    assert!(should_process_turn_message(MessageType::Regular));
    assert!(should_process_turn_message(MessageType::InlineReply));
}

#[test]
fn system_messages_are_not_processed_as_turns() {
    assert!(!should_process_turn_message(MessageType::ThreadCreated));
    assert!(!should_process_turn_message(
        MessageType::ThreadStarterMessage
    ));
    assert!(!should_process_turn_message(MessageType::ChatInputCommand));
}

#[test]
fn known_human_slash_messages_are_skipped() {
    let known = std::collections::HashSet::from([
        "help".to_string(),
        "clear".to_string(),
        "model".to_string(),
    ]);
    assert!(should_skip_human_slash_message("/help", Some(&known)));
    assert!(should_skip_human_slash_message("/clear now", Some(&known)));
}

#[test]
fn unregistered_human_slash_messages_fall_through() {
    let known = std::collections::HashSet::from(["help".to_string()]);
    assert!(!should_skip_human_slash_message("/unknown", Some(&known)));
    assert!(!should_skip_human_slash_message("/", Some(&known)));
    assert!(!should_skip_human_slash_message("/   ", Some(&known)));
    assert!(!should_skip_human_slash_message(
        "/unknown arg",
        Some(&known)
    ));
    assert!(!should_skip_human_slash_message("/unknown", None));
}

#[test]
fn allowed_bot_turn_text_requires_dispatch_prefix() {
    assert!(should_process_allowed_bot_turn_text(
        "DISPATCH:550e8400-e29b-41d4-a716-446655440000 [implementation] - Fix login bug"
    ));
    assert!(should_process_allowed_bot_turn_text(
        "DISPATCH:550e8400-e29b-41d4-a716-446655440000 [review-decision] - Feedback follow-up\n⛔ 코드 리뷰 금지"
    ));
    assert!(should_process_allowed_bot_turn_text(
        "DISPATCH:550e8400-e29b-41d4-a716-446655440000 [review] - Review this\n⚠️ 검토 전용 — 작업 착수 금지"
    ));
    assert!(!should_process_allowed_bot_turn_text(
        "completion_guard 수정에 OUTCOME: noop 처리도 포함해줘."
    ));
}

#[test]
fn explicit_user_mention_detects_only_real_mention_tokens() {
    let bot_id = UserId::new(42);

    assert!(content_has_explicit_user_mention("<@42> hello", bot_id));
    assert!(content_has_explicit_user_mention("hello <@!42>", bot_id));
    assert!(!content_has_explicit_user_mention("hello 42", bot_id));
    assert!(!content_has_explicit_user_mention(
        "reply context only",
        bot_id
    ));
}

#[test]
fn require_mention_guard_skips_unmentioned_messages_in_configured_channels() {
    let settings = DiscordBotSettings {
        require_mention_channel_ids: vec![777],
        ..Default::default()
    };

    assert!(should_skip_for_missing_required_mention(
        &settings,
        ChannelId::new(777),
        false,
        "hello",
        UserId::new(42),
    ));
    assert!(!should_skip_for_missing_required_mention(
        &settings,
        ChannelId::new(777),
        false,
        "hello <@42>",
        UserId::new(42),
    ));
    assert!(!should_skip_for_missing_required_mention(
        &settings,
        ChannelId::new(555),
        false,
        "hello",
        UserId::new(42),
    ));
    assert!(!should_skip_for_missing_required_mention(
        &settings,
        ChannelId::new(777),
        true,
        "hello",
        UserId::new(42),
    ));
}

#[test]
fn text_stop_lookup_keeps_active_turn_registered() {
    let channel_id = ChannelId::new(42);
    let token = Arc::new(CancelToken::new());
    let mut cancel_tokens = std::collections::HashMap::new();
    cancel_tokens.insert(channel_id, token.clone());

    match lookup_text_stop_token(&cancel_tokens, channel_id) {
        TextStopLookup::Stop(found) => {
            assert!(Arc::ptr_eq(&found, &token));
        }
        TextStopLookup::NoActiveTurn => panic!("expected active turn to be stoppable"),
        TextStopLookup::AlreadyStopping => panic!("fresh token should not look cancelled"),
    }

    assert_eq!(
        cancel_tokens.len(),
        1,
        "text stop lookup must not remove the active-turn marker"
    );
    assert!(
        cancel_tokens.contains_key(&channel_id),
        "active turn should stay registered until turn finalization cleans it up"
    );
}

#[test]
fn text_stop_lookup_detects_inflight_cancellation() {
    let channel_id = ChannelId::new(42);
    let token = Arc::new(CancelToken::new());
    token.cancelled.store(true, Ordering::Relaxed);
    let mut cancel_tokens = std::collections::HashMap::new();
    cancel_tokens.insert(channel_id, token);

    assert!(matches!(
        lookup_text_stop_token(&cancel_tokens, channel_id),
        TextStopLookup::AlreadyStopping
    ));
}

#[test]
fn intake_gate_enqueue_stays_responsive_while_policy_tick_times_out() {
    let db = crate::db::test_db();
    let config = crate::config::Config {
        policies: crate::config::PoliciesConfig {
            dir: std::path::PathBuf::from("/nonexistent"),
            hot_reload: false,
        },
        ..Default::default()
    };
    let engine = crate::engine::PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker_engine = engine.clone();
    let blocker = std::thread::spawn(move || {
        blocker_engine
            .block_actor_for_test(entered_tx, release_rx)
            .unwrap();
    });
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("policy engine actor should enter the blocker");

    let shared = super::super::make_shared_data_for_tests();
    let shared_for_runtime = shared.clone();
    let engine_for_runtime = engine.clone();
    let db_for_runtime = db.clone();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let runtime_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async move {
            let channel_id = ChannelId::new(4242);
            let tick_engine = engine_for_runtime.clone();
            let tick_db = db_for_runtime.clone();
            let tick_task = tokio::spawn(async move {
                crate::server::fire_tick_hook_by_name_for_test(
                    &tick_engine,
                    &tick_db,
                    "OnTick1min",
                    "1min",
                    Duration::from_millis(75),
                )
                .await
            });
            tokio::task::yield_now().await;

            let enqueued = super::intake_gate::enqueue_soft_intervention_for_test(
                &shared_for_runtime,
                channel_id,
                UserId::new(99),
                MessageId::new(1001),
                "gateway payload",
            )
            .await;
            let snapshot = shared_for_runtime.mailbox(channel_id).snapshot().await;
            let tick_outcome = tick_task.await.expect("tick task should join cleanly");
            (enqueued, snapshot.intervention_queue.len(), tick_outcome)
        });
        result_tx.send(result).unwrap();
    });

    let (enqueued, queued_count, tick_outcome) = result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("current-thread runtime should stay responsive while tick hook waits");

    assert!(enqueued, "intake_gate should still enqueue the message");
    assert_eq!(
        queued_count, 1,
        "queued intervention should be visible immediately"
    );
    assert_eq!(tick_outcome, crate::server::PolicyTickHookOutcome::Timeout);

    release_tx.send(()).unwrap();
    runtime_thread.join().unwrap();
    blocker.join().unwrap();
}

#[test]
fn classify_removed_control_reaction_only_matches_queue_and_stop_emojis() {
    assert_eq!(
        classify_removed_control_reaction(&ReactionType::Unicode("📬".to_string())),
        Some(RemovedControlReaction::CancelQueuedTurn)
    );
    assert_eq!(
        classify_removed_control_reaction(&ReactionType::Unicode("⏳".to_string())),
        Some(RemovedControlReaction::StopActiveTurn)
    );
    assert_eq!(
        classify_removed_control_reaction(&ReactionType::Unicode("✅".to_string())),
        None
    );
}

/// mid:* cleanup should use the longer MSG_DEDUP_TTL (60s),
/// while bot-specific entries (dispatch:*, msg:*) use INTAKE_DEDUP_TTL (30s).
/// Verifies that bot cleanup does not prematurely evict mid:* entries.
#[test]
fn mid_entries_survive_bot_cleanup() {
    use std::time::{Duration, Instant};

    let map: dashmap::DashMap<String, (Instant, bool)> = dashmap::DashMap::new();
    let now = Instant::now();

    // Simulate: mid:* entry inserted 40s ago (within 60s TTL, outside 30s TTL)
    let mid_time = now - Duration::from_secs(40);
    map.insert("mid:123".to_string(), (mid_time, false));

    // Simulate: dispatch:* entry inserted 40s ago (outside 30s TTL)
    map.insert("dispatch:abc".to_string(), (mid_time, false));

    // Simulate: fresh bot entry inserted just now
    map.insert("msg:456".to_string(), (now, false));

    // Bot cleanup: retain non-mid entries only if within 30s TTL
    let intake_dedup_ttl = Duration::from_secs(30);
    map.retain(|k, v| {
        if k.starts_with("mid:") {
            true // preserved; cleaned by universal dedup cleanup
        } else {
            now.duration_since(v.0) < intake_dedup_ttl
        }
    });

    // mid:* should survive bot cleanup
    assert!(
        map.contains_key("mid:123"),
        "mid:* entry must survive bot cleanup"
    );
    // dispatch:* older than 30s should be removed
    assert!(
        !map.contains_key("dispatch:abc"),
        "expired dispatch:* should be removed"
    );
    // fresh msg:* should survive
    assert!(map.contains_key("msg:456"), "fresh msg:* should survive");

    // Universal mid:* cleanup with 60s TTL
    let msg_dedup_ttl = Duration::from_secs(60);
    map.retain(|k, v| {
        if k.starts_with("mid:") {
            now.duration_since(v.0) < msg_dedup_ttl
        } else {
            true
        }
    });

    // mid:* at 40s should still survive (within 60s)
    assert!(
        map.contains_key("mid:123"),
        "mid:* within TTL must survive universal cleanup"
    );

    // Now simulate mid:* at 65s ago (outside 60s TTL)
    let old_mid_time = now - Duration::from_secs(65);
    map.insert("mid:old".to_string(), (old_mid_time, false));
    map.retain(|k, v| {
        if k.starts_with("mid:") {
            now.duration_since(v.0) < msg_dedup_ttl
        } else {
            true
        }
    });
    assert!(
        !map.contains_key("mid:old"),
        "expired mid:* must be cleaned by universal cleanup"
    );
}

/// Thread-preference dedup: once a message is processed as thread context,
/// subsequent thread duplicates (e.g. gateway reconnection) must be blocked.
/// Only parent→thread promotion is allowed, not thread→thread re-processing.
#[test]
fn thread_dedup_blocks_duplicate_thread_context() {
    use std::time::{Duration, Instant};

    let map: dashmap::DashMap<String, (Instant, bool)> = dashmap::DashMap::new();
    let now = Instant::now();
    let msg_dedup_ttl = Duration::from_secs(60);

    // Case 1: First seen as parent context, then thread arrives → allow
    map.insert("mid:100".to_string(), (now, false)); // was_thread = false
    let entry = map.get("mid:100").unwrap();
    let (ts, was_thread) = *entry;
    drop(entry);
    // is_thread_context=true, was_thread=false → should allow
    let allow = now.duration_since(ts) < msg_dedup_ttl && !was_thread; // this is the "allow" condition for thread promotion
    assert!(allow, "thread should be allowed when previous was parent");

    // Case 2: First seen as thread context, then thread arrives again → block
    map.insert("mid:200".to_string(), (now, true)); // was_thread = true
    let entry = map.get("mid:200").unwrap();
    let (ts2, was_thread2) = *entry;
    drop(entry);
    // is_thread_context=true, was_thread=true → should block
    let allow2 = now.duration_since(ts2) < msg_dedup_ttl && !was_thread2;
    assert!(!allow2, "duplicate thread context must be blocked");

    // Case 3: First seen as thread context, then parent arrives → block
    let entry = map.get("mid:200").unwrap();
    let (ts3, _was_thread3) = *entry;
    drop(entry);
    // is_thread_context=false → always blocked by the main branch
    let is_dup = now.duration_since(ts3) < msg_dedup_ttl;
    assert!(is_dup, "parent duplicate after thread must be blocked");
}

#[test]
fn model_picker_component_dispatch_matches_all_actions() {
    let channel_id = ChannelId::new(42);
    let custom_ids = [
        format!("agentdesk:model-picker:{}", channel_id.get()),
        format!("agentdesk:model-submit:{}", channel_id.get()),
        format!("agentdesk:model-reset:{}", channel_id.get()),
        format!("agentdesk:model-cancel:{}", channel_id.get()),
    ];

    for custom_id in custom_ids {
        assert!(
            is_model_picker_component_custom_id(&custom_id, channel_id),
            "expected model picker dispatch for {custom_id}"
        );
    }

    assert!(!is_model_picker_component_custom_id(
        "agentdesk:other:42",
        channel_id
    ));
}

#[test]
fn model_picker_close_response_acknowledges_component_close() {
    let payload = serde_json::to_value(build_model_picker_close_response())
        .expect("close response should serialize");

    assert_eq!(payload["type"], json!(6));
    assert_eq!(payload["data"], json!(null));
}

// ── #796 regression ────────────────────────────────────────────────────
//
// The race handler in `handle_text_message` historically deleted the new
// turn's placeholder whenever it lost the start-turn race to an in-flight
// turn. For background-task notifications (e.g. a `Bash run_in_background`
// completion relayed through the notify bot), that placeholder is the only
// user-visible record of the event — deleting it silently destroys
// information. The tests below pin the classification helper that drives
// the new "preserve placeholder for background-trigger turns" branch.

#[test]
fn turn_kind_classifies_notify_bot_authored_message_as_background_trigger() {
    let notify_bot_id = 9_999_001u64;
    let kind = classify_turn_kind_from_author(notify_bot_id, Some(notify_bot_id));
    assert_eq!(kind, TurnKind::BackgroundTrigger);
    assert!(kind.is_background_trigger());
}

#[test]
fn turn_kind_classifies_human_user_message_as_foreground() {
    let notify_bot_id = 9_999_001u64;
    let human_user_id = 1_234_567u64;
    let kind = classify_turn_kind_from_author(human_user_id, Some(notify_bot_id));
    assert_eq!(kind, TurnKind::Foreground);
    assert!(!kind.is_background_trigger());
}

#[test]
fn turn_kind_defaults_to_foreground_when_notify_bot_id_unresolved() {
    // If the notify bot lookup fails (registry missing or token unconfigured),
    // we must conservatively treat the turn as foreground — the legacy
    // race-handler delete behavior is the safe default for unknown senders.
    let kind = classify_turn_kind_from_author(42, None);
    assert_eq!(kind, TurnKind::Foreground);
    assert!(!kind.is_background_trigger());
}

#[test]
fn turn_kind_does_not_misclassify_announce_bot_as_background_trigger() {
    // Announce-bot messages today flow through the legacy turn path
    // (DISPATCH: prefix etc.) and must NOT be silently exempt from
    // race-handler delete — that would regress the existing protection.
    // Only the dedicated notify bot id triggers the BackgroundTrigger
    // classification.
    let notify_bot_id = 9_999_001u64;
    let announce_bot_id = 9_999_002u64;
    let kind = classify_turn_kind_from_author(announce_bot_id, Some(notify_bot_id));
    assert_eq!(kind, TurnKind::Foreground);
}
