use super::*;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::services::discord::health::{
    HealthRegistry, InjectPane, claim_kinded, register_inject_runtime,
};
use crate::services::discord::router;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::turn_orchestrator::{InterventionMode, SourceMessageQueuedGeneration};
use poise::serenity_prelude::{self as serenity, UserId};

const TEXT: &str = "status?\nline two\nline three";

#[derive(Clone, Default)]
struct RestState {
    markers: Arc<Mutex<HashSet<String>>>,
    notices: Arc<AtomicUsize>,
}

async fn rest(State(state): State<RestState>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method();
    if let Some((_, marker)) = path.split_once("/reactions/") {
        let marker = marker.trim_end_matches("/@me").to_string();
        if method == Method::PUT {
            state.markers.lock().unwrap().insert(marker);
        } else if method == Method::DELETE {
            state.markers.lock().unwrap().remove(&marker);
        }
        return StatusCode::NO_CONTENT.into_response();
    }
    let user = json!({"id": "200", "username": "writer", "discriminator": "0"});
    if path == "/api/v10/guilds/1/members/200" {
        return axum::Json(
            json!({"user": user, "nick": "writer", "roles": [], "deaf": false,
            "mute": false, "flags": 0}),
        )
        .into_response();
    }
    if let Some(tail) = path.strip_prefix("/api/v10/channels/") {
        if let Some((channel, message)) = tail.split_once("/messages/") {
            return axum::Json(json!({"id": message, "channel_id": channel, "author": user,
                "content": "status?", "timestamp": "2026-10-20T11:00:00Z", "edited_timestamp": null,
                "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                "attachments": [], "embeds": [], "pinned": false, "type": 0}))
            .into_response();
        }
        if tail.ends_with("/messages") {
            if method == Method::POST {
                state.notices.fetch_add(1, Ordering::SeqCst);
                return axum::Json(json!({"id": "940720200000999", "channel_id": tail.trim_end_matches("/messages"),
                    "author": user, "content": "queue notice", "timestamp": "2026-10-20T11:00:00Z",
                    "edited_timestamp": null, "tts": false, "mention_everyone": false,
                    "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
                    "pinned": false, "type": 0})).into_response();
            }
            return axum::Json(json!([])).into_response();
        }
        return axum::Json(json!({"id": tail, "type": 0, "guild_id": "1",
            "name": format!("inject-{tail}"), "position": 0, "permission_overwrites": [],
            "nsfw": false, "parent_id": null}))
        .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"message": "unexpected test endpoint", "code": 0})),
    )
        .into_response()
}

struct Transport {
    ctx: serenity::Context,
    state: RestState,
    _rest: crate::services::discord::shared_state::test_rest::Guard,
    http: tokio::task::JoinHandle<()>,
    gateway: tokio::task::JoinHandle<()>,
}

impl Transport {
    async fn new() -> Self {
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let state = RestState::default();
        let app = axum::Router::new().fallback(rest).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
        let _rest = crate::services::discord::shared_state::test_rest::install(ctx.http.clone());
        Self {
            ctx,
            state,
            _rest,
            http,
            gateway,
        }
    }

    fn pending_present(&self) -> bool {
        self.state
            .markers
            .lock()
            .unwrap()
            .iter()
            .any(|marker| marker.eq_ignore_ascii_case("%F0%9F%93%AC") || marker == "📬")
    }

    fn notice_count(&self) -> usize {
        self.state.notices.load(Ordering::SeqCst)
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.http.abort();
        self.gateway.abort();
    }
}

fn queued(message: MessageId, generation: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(200),
        author_is_bot: false,
        message_id: message,
        queued_generation: generation,
        source_message_ids: vec![message],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            message, generation,
        )],
        source_text_segments: Vec::new(),
        text: TEXT.into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn catch_up_sole_head_live_entry_injects_once_and_has_no_pending_rest_marker_pg() {
    catch_up_live_entry(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn catch_up_reserved_head_live_offer_keeps_one_pending_entry_without_a_notice_pg() {
    catch_up_live_entry(true).await;
}

async fn catch_up_live_entry(reserved: bool) {
    use crate::services::turn_orchestrator::QueuedHeadClaim;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let root_path = crate::config::runtime_root().unwrap();
    let channel = ChannelId::new(940_720_200_000_001 + u64::from(reserved));
    let generation = crate::services::discord::runtime_store::process_generation();
    let timestamp = chrono::Utc::now().timestamp_millis() - 1_420_070_400_000 - 2_000;
    let message_id = MessageId::new((timestamp as u64) << 22 | (7202 + u64::from(reserved)));
    let config = crate::runtime_layout::config_file_path(&root_path);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(root_path.join("workspace")).unwrap();
    std::fs::write(&config, json!({"server": {}, "data": {"dir": root_path.join("data")},
        "agents": [{"id": "catch-up-fixture", "name": "Catch-up fixture", "voice_enabled": false,
            "provider": "claude", "channels": {"claude": {"id": channel.to_string(),
                "name": format!("inject-{channel}"), "workspace": root_path.join("workspace"), "runtime": "tui"}}}]
    }).to_string()).unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        register_inject_runtime(&HealthRegistry::new(), &[channel.get()], Some(pool)).await;
    shared
        .restart
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Relaxed);
    claim_kinded(&shared, channel.get(), ActiveTurnKind::Background).await;
    let pane = InjectPane::new(channel.get(), "all");
    pane.fold_paste(TEXT.matches('\n').count() + 1);
    let _gate = router::busy_inject_support::open_gate(channel.get());
    test_hook::set_queued_head(channel.get(), true);
    let transport = Transport::new().await;
    shared
        .http
        .cached_serenity_ctx
        .set(transport.ctx.clone())
        .unwrap();
    shared.turn_view_reconciler.enable_test_http_reactions();
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        channel,
        &format!("inject-{channel}"),
    )
    .await;
    shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .unwrap()
        .current_path = Some(root_path.join("workspace").display().to_string());
    assert_eq!(
        crate::services::discord::resolve_runtime_channel_binding_status(
            &transport.ctx.http,
            channel,
        )
        .await,
        crate::services::discord::RuntimeChannelBindingStatus::Owned,
        "fixture has a real configured GuildText binding"
    );
    assert!(
        crate::services::discord::provider_handles_channel(
            &transport.ctx,
            &ProviderKind::Claude,
            &shared.settings.read().await.clone(),
            channel,
        )
        .await,
        "fixture passes live provider routing"
    );
    assert!(
        crate::services::discord::check_auth(UserId::new(200), "queue-user", &shared, "test-token")
            .await,
        "fixture passes fresh live authorization"
    );
    let enqueued =
        crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
            crate::services::discord::mailbox_enqueue_intervention(
                &shared,
                &ProviderKind::Claude,
                channel,
                queued(message_id, generation),
            ),
        )
        .await;
    assert!(
        enqueued.enqueued && enqueued.persistence_error.is_none(),
        "catch-up enqueue committed"
    );
    let original = crate::services::discord::mailbox_snapshot(&shared, channel)
        .await
        .intervention_queue;
    let _reservation = if reserved {
        let persistence = crate::services::discord::queue_persistence_context(
            &shared,
            &ProviderKind::Claude,
            channel,
        );
        let QueuedHeadClaim::Reserved(ticket) = shared
            .mailbox(channel)
            .claim_queued_head(QueueIdentity::of(&original[0]), persistence)
            .await
        else {
            panic!("fixture holds the queued head reservation before the live arrival");
        };
        Some(ticket)
    } else {
        None
    };
    crate::services::discord::queue_marker::note_added_queued_generation(
        &shared,
        &transport.ctx.http,
        channel,
        message_id,
        '📬',
        generation,
        "catch_up_fixture",
    )
    .await;
    assert!(
        transport.pending_present(),
        "fixture starts with a real REST pending marker"
    );
    assert!(
        pane.keys().is_empty(),
        "fixture has not injected before the live event"
    );
    let voice_config = crate::voice::VoiceConfig::default();
    let data = crate::services::discord::Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    let mut message = discord_mock::user_message(message_id.get(), TEXT);
    message.channel_id = channel;
    message.guild_id = Some(serenity::GuildId::new(1));
    message.author.id = UserId::new(200);
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    Box::pin(router::handle_event(&transport.ctx, &event, &data))
        .await
        .unwrap();
    let seen = router::busy_inject_support::seen(message_id.get());
    assert_eq!(
        seen.offers, 1,
        "fixture must enter the production live busy-inject boundary"
    );
    if reserved {
        assert_eq!(
            transport.notice_count(),
            0,
            "reserved catch-up head must not emit a queue-error notice"
        );
        assert!(
            matches!(seen.outcomes.as_slice(), [outcome] if outcome.starts_with("NotSent") && outcome.contains("source_owned")),
            "the actor reservation keeps custody of the live source: {seen:?}"
        );
        assert!(
            pane.keys().is_empty(),
            "a reserved head is not pasted again"
        );
        let after = crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue;
        assert_eq!(after.len(), 1, "reserved source keeps one queued entry");
        assert_eq!(format!("{after:?}"), format!("{original:?}"));
        assert!(
            transport.pending_present(),
            "a reserved source keeps its pending marker"
        );
        return;
    }
    assert!(
        matches!(seen.outcomes.as_slice(), [outcome] if outcome.starts_with("Injected")),
        "live arrival must inject its own catch-up head before fallback intake: {seen:?}"
    );
    assert_eq!(
        pane.keys(),
        ["paste-buffer", "send-keys"],
        "live entry takes its own catch-up head once"
    );
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue
            .is_empty()
    );
    assert!(
        !transport.pending_present(),
        "confirmed injection removes the existing REST pending marker"
    );
    crate::services::discord::queue_marker::note_added_queued_generation(
        &shared,
        &transport.ctx.http,
        channel,
        message_id,
        '📬',
        generation,
        "intake_gate_queue_pending",
    )
    .await;
    assert!(
        !transport.pending_present(),
        "a late queue transaction cannot restore the REST pending marker"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn catch_up_reuse_keeps_foreign_merged_rich_and_disabled_heads_queued_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (offset, case) in [
        "foreign", "merged", "upload", "reply", "command", "backlog", "disabled",
    ]
    .into_iter()
    .enumerate()
    {
        let channel = ChannelId::new(940_720_200_000_100 + offset as u64);
        let message = MessageId::new(channel.get() + 100);
        let generation = crate::services::discord::runtime_store::process_generation();
        let shared =
            register_inject_runtime(&HealthRegistry::new(), &[channel.get()], Some(pool.clone()))
                .await;
        claim_kinded(&shared, channel.get(), ActiveTurnKind::Background).await;
        let pane = InjectPane::new(channel.get(), "all");
        test_hook::set_queued_head(channel.get(), case != "disabled");
        let mut head = queued(message, generation);
        match case {
            "foreign" => head = queued(MessageId::new(message.get() + 1), generation),
            "merged" => {
                let earlier = MessageId::new(message.get() - 1);
                head.source_message_ids.insert(0, earlier);
                head.source_message_queued_generations.insert(
                    0,
                    SourceMessageQueuedGeneration::user_instruction(earlier, generation),
                );
            }
            "upload" => head.pending_uploads.push("[File uploaded] fixture".into()),
            "reply" => {
                head.reply_context = Some("reply context".into());
                head.has_reply_boundary = true;
            }
            "command" => head.text = "!unregistered".into(),
            _ => {}
        }
        let text = if case == "command" {
            "!unregistered"
        } else {
            TEXT
        };
        let enqueue =
            crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                crate::services::discord::mailbox_enqueue_intervention(
                    &shared,
                    &ProviderKind::Claude,
                    channel,
                    head,
                ),
            )
            .await;
        assert!(
            enqueue.enqueued && enqueue.persistence_error.is_none(),
            "{case}"
        );
        if case == "backlog" {
            let mut tail = queued(MessageId::new(message.get() + 1), generation);
            tail.text = "distinct queued tail".into();
            let enqueue =
                crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                    crate::services::discord::mailbox_enqueue_intervention(
                        &shared,
                        &ProviderKind::Claude,
                        channel,
                        tail,
                    ),
                )
                .await;
            assert!(enqueue.enqueued && enqueue.persistence_error.is_none());
        }
        let before = crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue;
        let guard = inject_disposition::claim_source(
            &ProviderKind::Claude,
            message,
            std::time::Instant::now(),
        )
        .unwrap();
        let request = HumanInputRequest {
            channel_id: channel,
            provider: ProviderKind::Claude,
            text: text.into(),
            author_id: 200,
            source: format!("discord:{channel}:{message}"),
            metadata: None,
            channel_name_hint: None,
        };
        let origin = Origin::Discord {
            message,
            handback: Box::new((queued(message, generation), "turn".into())),
            guard,
            name: "writer".into(),
        };
        let outcome = attempt(&shared, &request, origin).await;
        assert!(
            matches!(outcome, InjectAttempt::NotSent(QUEUE_NONEMPTY)),
            "{case}: {outcome:?}"
        );
        assert!(
            pane.keys().is_empty(),
            "{case}: no paste may overtake the queued source"
        );
        let after = crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue;
        assert_eq!(
            format!("{before:?}"),
            format!("{after:?}"),
            "{case}: retain original queue custody"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn catch_up_reused_head_veto_keeps_one_original_queue_entry_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(940_720_200_000_200);
    let message = MessageId::new(channel.get() + 100);
    let generation = crate::services::discord::runtime_store::process_generation();
    let shared =
        register_inject_runtime(&HealthRegistry::new(), &[channel.get()], Some(pool)).await;
    claim_kinded(&shared, channel.get(), ActiveTurnKind::Background).await;
    let pane = InjectPane::new(channel.get(), "all");
    pane.draft();
    test_hook::set_queued_head(channel.get(), true);
    let head = queued(message, generation);
    let enqueue = crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
        crate::services::discord::mailbox_enqueue_intervention(
            &shared,
            &ProviderKind::Claude,
            channel,
            head,
        ),
    )
    .await;
    assert!(enqueue.enqueued && enqueue.persistence_error.is_none());
    let before = crate::services::discord::mailbox_snapshot(&shared, channel)
        .await
        .intervention_queue;
    let guard =
        inject_disposition::claim_source(&ProviderKind::Claude, message, std::time::Instant::now())
            .unwrap();
    let request = HumanInputRequest {
        channel_id: channel,
        provider: ProviderKind::Claude,
        text: TEXT.into(),
        author_id: 200,
        source: format!("discord:{channel}:{message}"),
        metadata: None,
        channel_name_hint: None,
    };
    let origin = Origin::Discord {
        message,
        handback: Box::new((queued(message, generation), "turn".into())),
        guard,
        name: "writer".into(),
    };
    let outcome = attempt(&shared, &request, origin).await;
    assert!(
        matches!(outcome, InjectAttempt::HandedBack { veto: "draft", .. }),
        "{outcome:?}"
    );
    assert!(pane.keys().is_empty());
    let after = crate::services::discord::mailbox_snapshot(&shared, channel)
        .await
        .intervention_queue;
    assert_eq!(after.len(), 1);
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
}

#[tokio::test(flavor = "current_thread")]
async fn catch_up_mended_head_clears_pending_marker_without_another_paste_pg() {
    use crate::services::turn_orchestrator::{HeadOutcome, HeadSettle, QueuedHeadClaim};
    use std::io::Write;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(940_720_200_000_300);
    let message = MessageId::new(channel.get() + 100);
    let generation = crate::services::discord::runtime_store::process_generation();
    let shared =
        register_inject_runtime(&HealthRegistry::new(), &[channel.get()], Some(pool)).await;
    claim_kinded(&shared, channel.get(), ActiveTurnKind::Background).await;
    let pane = InjectPane::new(channel.get(), "all");
    test_hook::set_queued_head(channel.get(), true);
    let transport = Transport::new().await;
    shared
        .http
        .cached_serenity_ctx
        .set(transport.ctx.clone())
        .unwrap();
    shared.turn_view_reconciler.enable_test_http_reactions();
    let head = queued(message, generation);
    let identity = QueueIdentity::of(&head);
    let enqueue = crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
        crate::services::discord::mailbox_enqueue_intervention(
            &shared,
            &ProviderKind::Claude,
            channel,
            head,
        ),
    )
    .await;
    assert!(enqueue.enqueued && enqueue.persistence_error.is_none());
    let persistence = crate::services::discord::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel,
    );
    let mailbox = shared.mailbox(channel);
    let QueuedHeadClaim::Reserved(mut ticket) = mailbox
        .claim_queued_head(identity, persistence.clone())
        .await
    else {
        panic!("fixture reserves the queued head");
    };
    let nonce = "7202abcd";
    let path = pane.path(InjectPane::TRANSCRIPT);
    ticket
        .name_paste(nonce, &path, Some(InjectPane::SESSION.into()))
        .unwrap();
    let HeadSettle::Committed(Some(ticket)) = mailbox
        .settle_queued_head(ticket, HeadOutcome::MayHaveSent, persistence.clone())
        .await
    else {
        panic!("fixture records the paste attempt");
    };
    assert!(matches!(
        mailbox
            .settle_queued_head(ticket, HeadOutcome::Unconfirmed, persistence)
            .await,
        HeadSettle::Unknown
    ));
    crate::services::discord::queue_marker::note_added_queued_generation(
        &shared,
        &transport.ctx.http,
        channel,
        message,
        '📬',
        generation,
        "catch_up_fixture",
    )
    .await;
    assert!(transport.pending_present());
    let frame = crate::services::tui_input::human_frame::legacy_inject(
        &format!("discord:{channel}:{message}"),
        "200",
        nonce,
        TEXT,
    );
    let stamp = chrono::Utc::now().to_rfc3339();
    let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in [
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": stamp,
            "sessionId": InjectPane::SESSION, "content": frame}),
        json!({"type": "queue-operation", "operation": "remove", "timestamp": stamp,
            "sessionId": InjectPane::SESSION, "content": frame, "reason": "absorbed_mid_turn",
            "commandUuid": "catch-up-command", "deliveryId": "catch-up-delivery"}),
        json!({"type": "attachment", "timestamp": stamp, "sessionId": InjectPane::SESSION,
            "attachment": {"type": "queued_command", "prompt": frame, "source_uuid": "catch-up-command",
                "delivery_id": "catch-up-delivery", "commandMode": "prompt", "origin": {"kind": "human"}, "humanTurn": true}}),
    ] {
        writeln!(transcript, "{record}").unwrap();
    }
    let request = HumanInputRequest {
        channel_id: channel,
        provider: ProviderKind::Claude,
        text: TEXT.into(),
        author_id: 200,
        source: "queued".into(),
        metadata: None,
        channel_name_hint: None,
    };
    let mended = super::queued_reconcile::mend(&shared, &request, None, |_, _| true).await;
    assert!(matches!(
        mended,
        Some(InjectAttempt::NotSent(super::queued_support::RECONCILED))
    ));
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue
            .is_empty()
    );
    assert!(
        pane.keys().is_empty(),
        "transcript proof must never type the input again"
    );
    assert!(
        !transport.pending_present(),
        "mend confirmation also clears the existing pending marker"
    );
    crate::services::discord::queue_marker::note_added_queued_generation(
        &shared,
        &transport.ctx.http,
        channel,
        message,
        '📬',
        generation,
        "intake_gate_queue_pending",
    )
    .await;
    assert!(
        !transport.pending_present(),
        "mended input rejects a late pending marker too"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn confirmed_source_blocks_late_pending_rest_marker() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let transport = Transport::new().await;
    shared.turn_view_reconciler.enable_test_http_reactions();
    let channel = ChannelId::new(940_720_200_000_400);
    let message = MessageId::new(channel.get() + 100);
    // Exercise the REST transport on another target, leaving this target unfinalized.
    let control = MessageId::new(message.get() + 1);
    let generation = shared.restart.current_generation;
    crate::services::discord::queue_marker::note_added_queued_generation(
        &shared,
        &transport.ctx.http,
        channel,
        control,
        '📬',
        generation,
        "catch_up_fixture",
    )
    .await;
    assert!(
        transport.pending_present(),
        "the REST fixture observes a real marker add"
    );
    crate::services::discord::queue_marker::note_removed_queued_generation(
        &shared,
        &transport.ctx.http,
        channel,
        control,
        '📬',
        generation,
        "catch_up_fixture",
    )
    .await;
    assert!(!transport.pending_present());
    inject_disposition::note_terminal(
        &ProviderKind::Claude,
        channel,
        Some(message),
        InjectionOutcome::Observed,
        std::time::Instant::now(),
    );
    assert!(
        crate::services::discord::queue_marker::note_added_queued_generation(
            &shared,
            &transport.ctx.http,
            channel,
            message,
            '📬',
            generation,
            "intake_gate_queue_pending"
        )
        .await
    );
    assert!(
        !transport.pending_present(),
        "confirmed input must reject a late REST pending marker"
    );
}
