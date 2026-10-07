use super::*;
use crate::db::session_transcripts::NativeClearBoundary;
use crate::services::claude_tui::host_input::NativeClearSubmission;
use crate::services::discord::commands::control::native::{
    switch_on_for_tests,
    tests::{Fake, Fixture, runtime},
};
use crate::services::tui_prompt_dedupe::binding_events::BindingCause;
use std::sync::atomic::Ordering;

async fn acquire(
    fixture: &Fixture,
    http: &Arc<serenity::Http>,
    message: u64,
    uploads: &[crate::services::cluster::attachment_transfer::uploads::Upload],
) -> Option<IntakeRuntimeTransition> {
    acquire_after_redirect_or_requeue(
        (
            http,
            &fixture.shared,
            "native-router-test",
            &ProviderKind::Claude,
        ),
        (fixture.channel_id, fixture.channel_id),
        (
            TurnKind::Foreground,
            UserId::new(1),
            MessageId::new(message),
            "held prompt",
        ),
        (&None, false, false),
        (uploads, &None),
        (false, &None, None, false),
        (Some("old".into()), false, String::new()),
    )
    .await
    .expect("native admission preserves the input or acquires the transition")
}

#[test]
fn native_hold_requeues_the_real_router_input_and_uploads_durably_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(80).await;
        let effects = Arc::new(Fake::default());
        let enabled = switch_on_for_tests(effects.clone());
        let pool: &sqlx::PgPool = &fixture.pool;
        fixture.unresolved().await;
        fixture.record("new", BindingCause::Clear, false);
        effects.save_fails.store(true, Ordering::SeqCst);
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy("http://127.0.0.1:1")
                .ratelimiter_disabled(true)
                .build(),
        );
        let uploads = vec!["owned-upload".into()];
        let message = 6_577_801;
        assert!(
            fixture
                .shared
                .session_transition_lock(fixture.channel_id)
                .try_lock_owned()
                .is_ok(),
            "the lock is free; native Hold causes the requeue"
        );

        let transition = acquire(&fixture, &http, message, &uploads).await;

        assert!(
            transition.is_none(),
            "native Hold must reach durable enqueue"
        );
        let snapshot =
            crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel_id).await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        let (durable, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &ProviderKind::Claude,
            &fixture.shared.token_hash,
            fixture.channel_id,
        );
        assert_eq!(
            durable.len(),
            1,
            "the queue survives an in-memory mailbox loss"
        );
        for queued in [&snapshot.intervention_queue[0], &durable[0]] {
            assert_eq!(queued.message_id, MessageId::new(message));
            assert_eq!(queued.source_message_ids, [MessageId::new(message)]);
            assert_eq!(queued.text, "held prompt");
            assert_eq!(queued.pending_uploads, uploads);
        }
        assert!(matches!(
            crate::db::session_transcripts::native_channel_clear_state(
                pool,
                &fixture.channel_id.get().to_string()
            )
            .await
            .unwrap(),
            NativeClearBoundary::Unresolved { .. }
        ));
        assert_eq!(
            effects.calls(),
            ["save:new"],
            "router invoked native recovery"
        );
        drop(enabled);
        fixture.drop_db().await;
    });
}

async fn text_context(
    http: Arc<serenity::Http>,
) -> (serenity::Context, tokio::task::JoinHandle<()>) {
    use self::serenity::gateway::{
        Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner, ShardRunnerOptions,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|ws: axum::extract::ws::WebSocketUpgrade| async {
            ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ws_url = Arc::new(tokio::sync::Mutex::new(format!("ws://{address}")));
    let data = Arc::new(tokio::sync::RwLock::new(serenity::prelude::TypeMap::new()));
    let cache = Arc::new(serenity::Cache::default());
    let intents = serenity::GatewayIntents::empty();
    let (manager, _) = ShardManager::new(ShardManagerOptions {
        data: data.clone(),
        event_handlers: vec![],
        raw_event_handlers: vec![],
        framework: Arc::new(std::sync::OnceLock::new()),
        shard_index: 0,
        shard_init: 0,
        shard_total: 1,
        voice_manager: None,
        ws_url: ws_url.clone(),
        cache: cache.clone(),
        http: http.clone(),
        intents,
        presence: None,
    });
    let shard = Shard::new(
        ws_url,
        "test-token",
        serenity::ShardInfo {
            id: serenity::ShardId(0),
            total: 1,
        },
        intents,
        None,
    )
    .await
    .unwrap();
    let runner = ShardRunner::new(ShardRunnerOptions {
        data: data.clone(),
        event_handlers: vec![],
        raw_event_handlers: vec![],
        framework: None,
        manager,
        shard,
        voice_manager: None,
        cache: cache.clone(),
        http: http.clone(),
    });
    let context = serenity::Context {
        data,
        shard: ShardMessenger::new(&runner),
        shard_id: serenity::ShardId(0),
        http,
        cache,
    };
    (context, server)
}

#[test]
fn held_native_boundary_accepts_clear_through_the_text_command_router_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(81).await;
        let effects = Arc::new(Fake::default());
        let enabled = switch_on_for_tests(effects.clone());
        let pool: &sqlx::PgPool = &fixture.pool;
        fixture.unresolved().await;
        fixture.record("new", BindingCause::Clear, false);
        effects.save_fails.store(true, Ordering::SeqCst);
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy("http://127.0.0.1:1")
                .ratelimiter_disabled(true)
                .build(),
        );
        assert!(acquire(&fixture, &http, 6_577_811, &[]).await.is_none());
        assert!(matches!(
            crate::db::session_transcripts::native_channel_clear_state(
                pool,
                &fixture.channel_id.get().to_string()
            )
            .await
            .unwrap(),
            NativeClearBoundary::Unresolved { .. }
        ));
        effects.save_fails.store(false, Ordering::SeqCst);
        *effects.submitted.lock().unwrap() = Some(NativeClearSubmission::NotSent);
        let (ctx, server) = text_context(http.clone()).await;
        let mut message = serenity::Message::default();
        message.id = MessageId::new(6_577_812);
        message.channel_id = fixture.channel_id;
        message.author.id = UserId::new(1);
        message.content = "!clear".into();
        let voice_root = tempfile::tempdir().unwrap();
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.audio.recordings_dir = voice_root.path().to_path_buf();
        voice_config.keep_recordings = true;
        let data = crate::services::discord::Data {
            shared: fixture.shared.clone(),
            token: "native-router-test".into(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };

        let handled = crate::services::discord::router::message_handler::handle_text_command(
            &ctx,
            &message,
            &data,
            fixture.channel_id,
            &message.content,
            &[],
            &mut None,
        )
        .await
        .expect("held native input still permits the clear text command");

        assert!(handled);
        assert!(
            effects.calls().iter().any(|call| call == "submit"),
            "the text command executed native clear"
        );
        assert_eq!(
            crate::db::session_transcripts::native_channel_clear_state(
                pool,
                &fixture.channel_id.get().to_string()
            )
            .await
            .unwrap(),
            NativeClearBoundary::Resolved
        );
        assert!(
            acquire(&fixture, &http, 6_577_813, &[]).await.is_some(),
            "the real router admits input after the command"
        );
        server.abort();
        let _ = server.await;
        drop(enabled);
        fixture.drop_db().await;
    });
}
