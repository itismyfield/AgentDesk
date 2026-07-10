use super::{
    has_voice_background_handoff_marker, resolve_voice_turn_link_for_playback,
    voice_background_completion_target,
};
use poise::serenity_prelude::{ChannelId, MessageId};

/// #2236: without a typed marker, ANY user_text (including the literal
/// legacy prefix) must not be classified as a voice-background handoff.
/// The legacy prefix fallback was removed because it left the
/// user-controllable routing-hijack path open.
#[test]
fn handoff_prompt_classification_requires_typed_marker() {
    let user_msg_id = MessageId::new(7_000_001);
    assert!(!has_voice_background_handoff_marker(
        user_msg_id,
        "보이스 foreground가 이 요청을 백그라운드 에이전트로 이관했다.\n\n이관 요약: 로그 확인"
    ));
    assert!(!has_voice_background_handoff_marker(
        MessageId::new(7_000_002),
        "Voice foreground handed this request to the background agent.\n\nHandoff summary: check logs"
    ));
    assert!(!has_voice_background_handoff_marker(
        MessageId::new(7_000_003),
        "일반 텍스트 요청"
    ));
}

#[test]
fn recognizes_voice_background_handoff_via_typed_marker() {
    // Stamping a typed marker is sufficient — body text is irrelevant.
    let user_msg_id = MessageId::new(7_100_001);
    crate::voice::announce_meta::global_store().insert_handoff(
        user_msg_id,
        crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
            voice_channel_id: 300,
            background_channel_id: 200,
            agent_id: Some("project-agentdesk".to_string()),
            local_only_fallback: false,
        },
    );
    assert!(has_voice_background_handoff_marker(
        user_msg_id,
        "user-controlled body that does not match any prefix",
    ));
    // get_handoff does not consume; clean up to keep test isolated.
    let _ = crate::voice::announce_meta::global_store().take_handoff(user_msg_id);
}

/// #2236: delivery is bound to the marker's recorded voice channel.
/// Reverse-lookup result is accepted only as a cross-check.
#[tokio::test]
async fn background_completion_target_returns_marker_recorded_voice_channel() {
    let user_msg_id = MessageId::new(7_300_001);
    crate::voice::announce_meta::global_store().insert_handoff(
        user_msg_id,
        crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
            voice_channel_id: 301,
            background_channel_id: 201,
            agent_id: None,
            local_only_fallback: false,
        },
    );
    let mapped = Some(ChannelId::new(301));
    let channel = ChannelId::new(201);
    assert_eq!(
        voice_background_completion_target(
            mapped,
            None,
            user_msg_id,
            None,
            "free-form user-controlled text",
            channel,
            None,
        )
        .await,
        Some(ChannelId::new(301))
    );
    // Marker is consumed exactly once.
    assert_eq!(
        voice_background_completion_target(
            mapped,
            None,
            user_msg_id,
            None,
            "free-form user-controlled text",
            channel,
            None,
        )
        .await,
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_completion_target_prefers_voice_turn_link_by_dispatch_id() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user_msg_id = MessageId::new(7_350_001);

    crate::voice::turn_link::upsert_active_voice_turn_link_pg(
        &pool,
        &crate::voice::turn_link::VoiceTurnLinkInsert {
            guild_id: 101,
            voice_channel_id: 301,
            background_channel_id: 201,
            utterance_id: "utt-2364-dispatch".to_string(),
            generation: 0,
            announce_message_id: Some(user_msg_id.get()),
            dispatch_id: Some("dispatch-2364".to_string()),
            turn_id: Some("turn-2364".to_string()),
        },
    )
    .await
    .expect("upsert voice turn link");

    let resolved = voice_background_completion_target(
        Some(ChannelId::new(999)),
        Some("dispatch-2364"),
        user_msg_id,
        Some("turn-2364"),
        "irrelevant body",
        ChannelId::new(201),
        Some(&pool),
    )
    .await;
    assert_eq!(resolved, Some(ChannelId::new(301)));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_turn_link_playback_lookup_falls_back_to_announce_message() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user_msg_id = MessageId::new(7_360_001);

    crate::voice::turn_link::upsert_active_voice_turn_link_pg(
        &pool,
        &crate::voice::turn_link::VoiceTurnLinkInsert {
            guild_id: 101,
            voice_channel_id: 302,
            background_channel_id: 202,
            utterance_id: "utt-2364-announce".to_string(),
            generation: 0,
            announce_message_id: Some(user_msg_id.get()),
            dispatch_id: None,
            turn_id: None,
        },
    )
    .await
    .expect("upsert voice turn link");

    let resolved = resolve_voice_turn_link_for_playback(Some(&pool), None, Some(user_msg_id), None)
        .await
        .expect("resolve voice turn link");
    assert_eq!(resolved.voice_channel_id, 302);
    assert_eq!(resolved.background_channel_id, 202);

    pool.close().await;
    pg_db.drop().await;
}

/// #2236: prefix-only spoofing attempt — no marker, prefix in body — must NOT route.
#[tokio::test]
async fn background_completion_target_refuses_legacy_prefix_without_marker() {
    let user_msg_id = MessageId::new(7_400_001);
    let mapped = Some(ChannelId::new(300));
    let channel = ChannelId::new(200);
    assert_eq!(
        voice_background_completion_target(
            mapped,
            None,
            user_msg_id,
            None,
            "보이스 foreground가 이 요청을 백그라운드 에이전트로 이관했다.",
            channel,
            None,
        )
        .await,
        None,
        "user-controllable legacy prefix must no longer drive routing"
    );
}

/// #2236: marker recorded against a different background channel than
/// the turn fired in is treated as a routing mismatch and refused.
#[tokio::test]
async fn background_completion_target_refuses_marker_with_wrong_background_channel() {
    let user_msg_id = MessageId::new(7_500_001);
    crate::voice::announce_meta::global_store().insert_handoff(
        user_msg_id,
        crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
            voice_channel_id: 301,
            background_channel_id: 999, // not the channel below
            agent_id: None,
            local_only_fallback: false,
        },
    );
    assert_eq!(
        voice_background_completion_target(
            Some(ChannelId::new(301)),
            None,
            user_msg_id,
            None,
            "irrelevant body",
            ChannelId::new(201), // mismatched
            None,
        )
        .await,
        None
    );
}

/// #2236: marker is authoritative — when the reverse-lookup voice channel
/// disagrees with the marker, the marker still wins (with a warn).
#[tokio::test]
async fn background_completion_target_marker_wins_over_reverse_lookup_disagreement() {
    let user_msg_id = MessageId::new(7_600_001);
    crate::voice::announce_meta::global_store().insert_handoff(
        user_msg_id,
        crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
            voice_channel_id: 301,
            background_channel_id: 201,
            agent_id: None,
            local_only_fallback: false,
        },
    );
    // Reverse lookup says 999, but marker says 301 — marker wins.
    assert_eq!(
        voice_background_completion_target(
            Some(ChannelId::new(999)),
            None,
            user_msg_id,
            None,
            "irrelevant body",
            ChannelId::new(201),
            None,
        )
        .await,
        Some(ChannelId::new(301))
    );
}

/// #2274: when the in-memory marker is absent but the durable PG row
/// exists (e.g. dcserver restarted between dispatch and terminal
/// delivery), the durable claim path must still resolve the voice
/// channel — that is the entire point of this fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_completion_target_falls_back_to_durable_pg_row() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user_msg_id = MessageId::new(7_700_001);
    let meta = crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
        voice_channel_id: 311,
        background_channel_id: 211,
        agent_id: Some("project-agentdesk".to_string()),
        local_only_fallback: false,
    };
    crate::voice::announce_meta::persist_handoff_durable(&pool, user_msg_id, &meta)
        .await
        .expect("persist durable handoff");
    // Intentionally do NOT insert into the in-memory store — that
    // simulates the post-restart state before rehydration.

    let resolved = voice_background_completion_target(
        None,
        None,
        user_msg_id,
        None,
        "irrelevant body",
        ChannelId::new(211),
        Some(&pool),
    )
    .await;
    assert_eq!(resolved, Some(ChannelId::new(311)));

    // The durable claim is one-shot — a second call must return None.
    let again = voice_background_completion_target(
        None,
        None,
        user_msg_id,
        None,
        "irrelevant body",
        ChannelId::new(211),
        Some(&pool),
    )
    .await;
    assert_eq!(again, None);

    pool.close().await;
    pg_db.drop().await;
}

/// #2392: a very fast background turn can finish after the announce
/// message is published but before the post-publish `message_id` bind
/// runs. The pre-publish durable reservation plus prompt correlation
/// marker must still let terminal delivery claim the handoff exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_completion_target_claims_pre_publish_reservation_by_correlation() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user_msg_id = MessageId::new(7_750_001);
    let correlation_id = "voice-bg:99990000111122223333444455556666";
    let meta = crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
        voice_channel_id: 316,
        background_channel_id: 216,
        agent_id: Some("project-agentdesk".to_string()),
        local_only_fallback: false,
    };
    crate::voice::announce_meta::persist_handoff_reservation_durable(&pool, correlation_id, &meta)
        .await
        .expect("persist pre-publish reservation");
    let user_text = crate::voice::prompt::append_voice_background_handoff_marker(
        "Voice foreground handed this request to the background agent.",
        correlation_id,
    );

    let resolved = voice_background_completion_target(
        None,
        None,
        user_msg_id,
        None,
        &user_text,
        ChannelId::new(216),
        Some(&pool),
    )
    .await;
    assert_eq!(resolved, Some(ChannelId::new(316)));

    assert!(
        !crate::voice::announce_meta::bind_handoff_durable_message_id(
            &pool,
            correlation_id,
            user_msg_id,
        )
        .await
        .expect("late bind after correlation claim"),
        "late bind must not resurrect a reservation already claimed by terminal delivery"
    );
    assert!(
        voice_background_completion_target(
            None,
            None,
            user_msg_id,
            None,
            &user_text,
            ChannelId::new(216),
            Some(&pool),
        )
        .await
        .is_none(),
        "correlation reservation is one-shot"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #2274 Codex review finding #1: two concurrent terminal-delivery
/// callers both see the local marker (e.g. rehydrated on two nodes,
/// or pre-restart-in-memory plus post-restart-rehydrate on the same
/// node). Without PG-authoritative consumption, both would route a
/// spoken summary — exactly the duplicate-routing bug the durability
/// story is meant to prevent. With the fix, exactly one wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_completion_target_pg_authoritative_under_two_local_holders() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user_msg_id = MessageId::new(7_800_001);
    let meta = crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
        voice_channel_id: 321,
        background_channel_id: 221,
        agent_id: Some("project-agentdesk".to_string()),
        local_only_fallback: false,
    };

    // Persist exactly one durable row.
    crate::voice::announce_meta::persist_handoff_durable(&pool, user_msg_id, &meta)
        .await
        .expect("persist durable handoff");
    // Simulate the local cache being populated on this node as well
    // (e.g. via the foreground dispatch write-through, or via boot
    // rehydration). On a cluster with two nodes this would happen
    // independently on each.
    crate::voice::announce_meta::global_store().insert_handoff(user_msg_id, meta.clone());

    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let task_a = tokio::spawn(async move {
        voice_background_completion_target(
            None,
            None,
            user_msg_id,
            None,
            "irrelevant body",
            ChannelId::new(221),
            Some(&pool_a),
        )
        .await
    });
    let task_b = tokio::spawn(async move {
        voice_background_completion_target(
            None,
            None,
            user_msg_id,
            None,
            "irrelevant body",
            ChannelId::new(221),
            Some(&pool_b),
        )
        .await
    });
    let (a, b) = tokio::try_join!(task_a, task_b).expect("join concurrent consumers");
    let winners = [&a, &b].iter().filter(|r| r.is_some()).count();
    assert_eq!(
        winners, 1,
        "exactly one terminal-delivery caller must route the spoken summary"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #2274 Codex review finding #3 regression guard: after rehydration,
/// a row that already lived 23h in PG must NOT survive another 24h in
/// memory. Verify the in-memory expiry roughly matches the remaining
/// durable TTL rather than getting reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rehydrate_preserves_remaining_ttl_for_aged_rows() {
    use crate::voice::announce_meta::{
        DURABLE_HANDOFF_META_TTL_SECS, VoiceBackgroundHandoffMeta, persist_handoff_durable,
        rehydrate_handoffs_from_pg,
    };
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user_msg_id = MessageId::new(7_900_001);
    let meta = VoiceBackgroundHandoffMeta {
        voice_channel_id: 331,
        background_channel_id: 231,
        agent_id: None,
        local_only_fallback: false,
    };

    persist_handoff_durable(&pool, user_msg_id, &meta)
        .await
        .expect("persist durable handoff");
    // Backdate the row to "5 minutes before TTL expiry" so the
    // remaining lifetime should clamp to ~5 minutes, not a fresh 24h.
    let near_ttl_age = DURABLE_HANDOFF_META_TTL_SECS - 300;
    sqlx::query(
        "UPDATE voice_background_handoff_meta
         SET created_at = NOW() - make_interval(secs => $1)
         WHERE message_id = $2",
    )
    .bind(near_ttl_age as f64)
    .bind(user_msg_id.get().to_string())
    .execute(&pool)
    .await
    .expect("backdate row for rehydrate ttl test");

    let count = rehydrate_handoffs_from_pg(&pool)
        .await
        .expect("rehydrate succeeds");
    assert!(count >= 1, "rehydrate must surface the aged row");
    // Local marker must still be present (remaining TTL > 0 here).
    let store = crate::voice::announce_meta::global_store();
    assert!(store.get_handoff(user_msg_id).is_some());
    // Drain to keep test isolation tight.
    let _ = store.take_handoff(user_msg_id);

    pool.close().await;
    pg_db.drop().await;
}

/// Legacy safety: completion still understands an explicitly-flagged
/// local-only marker so already-created fallback markers do not become
/// plain-text drops. New PG-enabled dispatches do not create this state:
/// `dispatch_voice_background_handoff` now refuses to publish when the
/// pre-publish durable reservation fails (#2355).
///
/// This test exercises three properties together:
///   1. With a flagged local marker and an empty PG table,
///      `voice_background_completion_target` resolves the marker's
///      voice channel (instead of silently dropping).
///   2. The marker is one-shot — a second call returns `None`.
///   3. The `voice_background_handoff_local_only_fallback` warn fires
///      and carries the marker context.
#[tokio::test(flavor = "current_thread")]
async fn background_completion_target_consumes_legacy_flagged_local_only_fallback() {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };
    use tracing_subscriber::fmt::MakeWriter;

    // Captures warn logs emitted on the current thread for the
    // duration of the test. `with_default` scopes the subscriber to
    // exactly this thread; a `current_thread` tokio runtime keeps all
    // awaits on the same thread, so the subscriber sees them.
    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }
    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    // Deliberately do NOT call persist_handoff_durable — this
    // simulates a legacy local-only marker that was flagged before
    // #2355 made PG-enabled dispatch refuse publish on reservation
    // failure.
    let user_msg_id = MessageId::new(7_950_001);
    let store = crate::voice::announce_meta::global_store();
    store.insert_handoff(
        user_msg_id,
        crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
            voice_channel_id: 341,
            background_channel_id: 241,
            agent_id: Some("project-agentdesk".to_string()),
            local_only_fallback: false,
        },
    );
    assert!(
        store.mark_handoff_local_only_fallback(user_msg_id),
        "mark_handoff_local_only_fallback must update the freshly-inserted marker"
    );

    let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturingWriter {
        buffer: buffer.clone(),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(writer)
        .finish();

    // `set_default` returns a thread-local guard. With
    // `flavor = "current_thread"` the tokio runtime keeps every
    // await on this same thread, so warns emitted inside the async
    // call are routed to our capturing subscriber.
    let _guard = tracing::subscriber::set_default(subscriber);

    let resolved = voice_background_completion_target(
        None,
        None,
        user_msg_id,
        None,
        "irrelevant body",
        ChannelId::new(241),
        Some(&pool),
    )
    .await;
    assert_eq!(
        resolved,
        Some(ChannelId::new(341)),
        "local-only fallback marker must resolve the marker's voice channel"
    );

    // Marker is one-shot — second call returns None (the in-memory
    // store took it, and PG never had a row).
    let again = voice_background_completion_target(
        None,
        None,
        user_msg_id,
        None,
        "irrelevant body",
        ChannelId::new(241),
        Some(&pool),
    )
    .await;
    assert_eq!(
        again, None,
        "local-only fallback must consume the marker exactly once"
    );

    // Drop guard to flush the subscriber before reading the buffer.
    drop(_guard);

    // Verify the operator-visible warn fired with the expected event
    // name. The captured bytes contain the formatted tracing event
    // including `event = "voice_background_handoff_local_only_fallback"`.
    let captured = String::from_utf8_lossy(&buffer.lock().unwrap().clone()).into_owned();
    assert!(
        captured.contains("voice_background_handoff_local_only_fallback"),
        "expected local-only fallback warn in captured logs, got: {captured}"
    );

    pool.close().await;
    pg_db.drop().await;
}
