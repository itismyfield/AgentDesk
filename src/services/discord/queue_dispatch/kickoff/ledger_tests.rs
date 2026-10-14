use super::*;
use crate::services::discord::gateway::{DiscordGateway, LiveDiscordTurnContext, TurnGateway};
use crate::services::turn_orchestrator::{
    SourceMessageQueuedGeneration, SourceMessageTextSegment, load_channel_pending_dispatch_marker,
    load_channel_pending_queue_for_tests,
};
use std::cell::RefCell;

pub(in crate::services::discord) fn gateway_custody_result(
    result: Result<router::QueuedCustody, queue_io::QueuedCustodyError>,
) -> Result<router::QueuedCustody, Error> {
    result.map_err(|error| {
        if input_runtime::supervisor::mutant("c2c2_gateway_custody_error_stringified") {
            error.to_string().into()
        } else {
            Box::new(error) as Error
        }
    })
}

thread_local! {
    static AFTER_ADMISSION: RefCell<Option<(ChannelId, std::path::PathBuf)>> = const { RefCell::new(None) };
}

pub(super) fn after_admission(channel: ChannelId) {
    let failure = AFTER_ADMISSION.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| *target == channel)
        {
            slot.borrow_mut().take()
        } else {
            None
        }
    });
    if let Some((_, path)) = failure {
        block_restore(&path);
    }
}

fn block_restore(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove fixture queue after actual dequeue: {error}"),
    }
    std::fs::create_dir(path).expect("block only restore persistence");
}

struct Fixture {
    shared: Arc<SharedData>,
    channel: ChannelId,
    http: Arc<serenity::Http>,
    proxy: String,
    port: u16,
    server: tokio::task::JoinHandle<()>,
    _guards: Vec<crate::config::TestEnvVarGuard>,
    _root: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        AFTER_ADMISSION.with(|slot| *slot.borrow_mut() = None);
    }
}

fn rich_item(id: u64) -> Intervention {
    let generation = runtime_store::process_generation();
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: true,
        message_id: MessageId::new(id),
        queued_generation: generation,
        source_message_ids: vec![MessageId::new(id - 1), MessageId::new(id)],
        source_message_queued_generations: vec![
            SourceMessageQueuedGeneration::new(MessageId::new(id - 1), generation),
            SourceMessageQueuedGeneration::new(MessageId::new(id), generation),
        ],
        source_text_segments: vec![
            SourceMessageTextSegment {
                message_id: MessageId::new(id - 1),
                text: "absorbed".into(),
            },
            SourceMessageTextSegment {
                message_id: MessageId::new(id),
                text: "primary".into(),
            },
        ],
        text: "absorbed\nprimary".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: Some("original reply".into()),
        has_reply_boundary: true,
        merge_consecutive: false,
        pending_uploads: vec!["Q1".into(), "Q2".into()],
        voice_announcement: Some(crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: "voice original".into(),
            user_id: "43".into(),
            utterance_id: format!("c2c2-{id}"),
            language: "ko".into(),
            verbose_progress: true,
            started_at: Some("2026-10-14T03:00:00+09:00".into()),
            completed_at: Some("2026-10-14T03:00:01+09:00".into()),
            samples_written: Some(48000),
            control_channel_id: Some(id + 100),
            stt_mode: Some("file".into()),
            stt_latency_ms: Some(120),
        }),
    }
}

impl Fixture {
    async fn new(channel: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fixture.yml");
        std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
        let guards = vec![
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                root.path(),
            ),
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_CONFIG",
                &config,
            ),
            crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                "ADK_INTAKE_ROUTING_MODE",
                "disabled".as_ref(),
            ),
            crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                "ADK_TEST_CODEX_PROMOTE_READY",
                "1".as_ref(),
            ),
        ];
        let channel = ChannelId::new(channel);
        let app = axum::Router::new().route("/gateway", axum::routing::get(
            |ws: axum::extract::ws::WebSocketUpgrade| async {
                ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
            }
        )).fallback(move |method: axum::http::Method| async move {
            use axum::response::IntoResponse;
            if method == axum::http::Method::GET {
                axum::Json(serde_json::json!({"id":channel.get().to_string(),"type":0,"name":"upper-test-cdx",
                    "guild_id":"8000000","position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null})).into_response()
            } else {
                axum::http::StatusCode::NO_CONTENT.into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = format!("http://127.0.0.1:{port}");
        let http = Arc::new(
            serenity::HttpBuilder::new("upper-test-token")
                .proxy(proxy.clone())
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let shared = make_shared_data_for_tests();
        shared.settings.write().await.provider = ProviderKind::Codex;
        Self {
            shared,
            channel,
            http,
            proxy,
            port,
            server,
            _guards: guards,
            _root: root,
        }
    }

    fn deps(&self) -> router::IntakeDeps<'_> {
        router::IntakeDeps {
            http: &self.http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &self.shared,
            token: "upper-test-token",
        }
    }

    fn queue_path(&self) -> std::path::PathBuf {
        runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("codex")
            .join(&self.shared.token_hash)
            .join(format!("{}.json", self.channel.get()))
    }

    async fn seed(&self) -> Intervention {
        let mut original = rich_item(self.channel.get() + 2);
        let real_id = |id: MessageId| MessageId::new(100_000_000_000_000 + id.get());
        original.message_id = real_id(original.message_id);
        for id in &mut original.source_message_ids {
            *id = real_id(*id);
        }
        for source in &mut original.source_message_queued_generations {
            source.message_id = real_id(source.message_id);
        }
        for source in &mut original.source_text_segments {
            source.message_id = real_id(source.message_id);
        }
        self.shared
            .mailbox(self.channel)
            .replace_queue(
                vec![original.clone()],
                queue_dispatch::persistence_context(
                    &self.shared,
                    &ProviderKind::Codex,
                    self.channel,
                ),
            )
            .await;
        let sources = original.source_message_queued_generations();
        assert!(!sources.is_empty(), "actual source markers must be seeded");
        let before = self.shared.turn_view_reconciler.ops().len();
        for source in &sources {
            assert!(reaction_lifecycle::is_real_discord_message_id(
                source.message_id
            ));
            assert!(
                queue_marker::note_added_queued_generation(
                    &self.shared,
                    &self.http,
                    self.channel,
                    source.message_id,
                    queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
                    source.queued_generation,
                    "upper_fixture",
                )
                .await,
                "queue marker seed must be accepted"
            );
        }
        let ops = self.shared.turn_view_reconciler.ops();
        let seeded = &ops[before..];
        assert_eq!(
            seeded.len(),
            sources.len(),
            "each real source must emit its queued UI operation"
        );
        assert!(
            seeded.iter().all(|op| op.add
                && op.emoji == queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
                && op.target.kind == turn_view_reconciler::TurnViewTargetKind::IntakeUserMessage
                && op.target.channel_id == self.channel
                && sources
                    .iter()
                    .any(|source| op.target.message_id == source.message_id)),
            "seeded UI must be the actual queued source target"
        );
        original
    }

    async fn gateway(&self) -> DiscordGateway {
        let ctx = tui_prompt_relay::relay_e2e::discord_mock::serenity_context(
            self.proxy.clone(),
            format!("ws://127.0.0.1:{}/gateway", self.port),
        )
        .await;
        DiscordGateway::new(
            self.http.clone(),
            self.shared.clone(),
            ProviderKind::Codex,
            Some(LiveDiscordTurnContext {
                ctx,
                token: "upper-test-token".into(),
                request_owner: UserId::new(1),
            }),
        )
    }
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_ct10_kickoff_received_never_starts_or_restores() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_125_000).await;
    f.seed().await;
    router::QueuedAdmissionDisposition::inject_ledger_for_test(f.channel.get(), true);
    let outcome = kickoff_idle_queue_channel(&f.deps(), &ProviderKind::Codex, f.channel).await;
    assert!(
        !outcome.expect("received custody is consumed").started,
        "receipt is never provider start"
    );
    assert!(
        mailbox_snapshot(&f.shared, f.channel)
            .await
            .intervention_queue
            .is_empty(),
        "actual received custody must not restore the original"
    );
    assert!(
        load_channel_pending_dispatch_marker(&ProviderKind::Codex, &f.shared.token_hash, f.channel)
            .is_none(),
        "received custody settles its actual pending dispatch lease"
    );
    assert!(
        !f.shared
            .turn_view_reconciler
            .ops()
            .iter()
            .any(|op| op.add && op.emoji == '⏳'),
        "received custody must not publish provider-start UI"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_ct10_kickoff_deferred_restores_before_ui() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_126_000).await;
    let original = f.seed().await;
    let ops = f.shared.turn_view_reconciler.ops().len();
    router::QueuedAdmissionDisposition::inject_ledger_for_test(f.channel.get(), false);
    let outcome = kickoff_idle_queue_channel(&f.deps(), &ProviderKind::Codex, f.channel).await;
    assert!(!outcome.expect("durable restore is safe no-start").started);
    let (disk, _) =
        load_channel_pending_queue_for_tests(&ProviderKind::Codex, &f.shared.token_hash, f.channel);
    assert_eq!(disk.len(), 1, "durable restore readback required");
    assert_eq!(disk[0].pending_uploads, original.pending_uploads);
    assert_eq!(disk[0].voice_announcement, original.voice_announcement);
    assert_eq!(disk[0].reply_context, original.reply_context);
    assert_eq!(disk[0].source_message_ids, original.source_message_ids);
    assert_eq!(
        f.shared.turn_view_reconciler.ops().len(),
        ops,
        "deferred custody preserves queued UI"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_ct10_kickoff_restore_failure_is_caller_error() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_127_000).await;
    f.seed().await;
    let ops = f.shared.turn_view_reconciler.ops().len();
    AFTER_ADMISSION.with(|slot| *slot.borrow_mut() = Some((f.channel, f.queue_path())));
    router::QueuedAdmissionDisposition::inject_ledger_for_test(f.channel.get(), false);
    let outcome = kickoff_idle_queue_channel(&f.deps(), &ProviderKind::Codex, f.channel).await;
    let error = outcome.expect_err("restore failure must escape no-start result");
    assert!(
        error.is::<queue_io::QueuedCustodyError>(),
        "typed error keeps the actual owner"
    );
    assert_eq!(
        f.shared.turn_view_reconciler.ops().len(),
        ops,
        "failure keeps queued UI"
    );
    assert!(
        load_channel_pending_dispatch_marker(&ProviderKind::Codex, &f.shared.token_hash, f.channel)
            .is_some()
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_ct10_gateway_received_consumes_custody() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_128_000).await;
    f.seed().await;
    let taken =
        idle_queue_take_next_soft_if_ready(&f.shared, &ProviderKind::Codex, f.channel).await;
    let original = taken
        .intervention
        .as_ref()
        .expect("actor dequeued before C injection");
    assert!(taken.dispatch_lease.is_some(), "actual actor lease");
    let gateway = f.gateway().await;
    router::QueuedAdmissionDisposition::inject_ledger_for_test(f.channel.get(), true);
    let outcome = gateway
        .dispatch_queued_turn_checked(
            f.channel,
            original,
            "owner",
            false,
            taken.dispatch_lease.clone(),
        )
        .await;
    assert_eq!(
        outcome.expect("gateway centrally consumes C custody"),
        gateway::QueuedDispatchSuccess::Ledger
    );
    assert!(
        load_channel_pending_dispatch_marker(&ProviderKind::Codex, &f.shared.token_hash, f.channel)
            .is_none()
    );
    assert!(
        mailbox_snapshot(&f.shared, f.channel)
            .await
            .intervention_queue
            .is_empty()
    );
    assert!(
        !f.shared
            .turn_view_reconciler
            .ops()
            .iter()
            .any(|op| op.add && op.emoji == '⏳'),
        "received custody must not publish provider-start UI"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_ct10_gateway_deferred_restores_before_ui() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_129_000).await;
    let original = f.seed().await;
    let ops = f.shared.turn_view_reconciler.ops().len();
    let taken =
        idle_queue_take_next_soft_if_ready(&f.shared, &ProviderKind::Codex, f.channel).await;
    let gateway = f.gateway().await;
    router::QueuedAdmissionDisposition::inject_ledger_for_test(f.channel.get(), false);
    let outcome = gateway
        .dispatch_queued_turn_checked(
            f.channel,
            taken.intervention.as_ref().expect("actual actor dequeue"),
            "owner",
            false,
            taken.dispatch_lease.clone(),
        )
        .await;
    assert_eq!(
        outcome.expect("actual C restore"),
        gateway::QueuedDispatchSuccess::Ledger
    );
    let (disk, _) =
        load_channel_pending_queue_for_tests(&ProviderKind::Codex, &f.shared.token_hash, f.channel);
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].pending_uploads, original.pending_uploads);
    assert_eq!(disk[0].reply_context, original.reply_context);
    assert_eq!(disk[0].voice_announcement, original.voice_announcement);
    assert_eq!(disk[0].source_message_ids, original.source_message_ids);
    assert_eq!(
        f.shared.turn_view_reconciler.ops().len(),
        ops,
        "restored custody keeps queued UI"
    );
    assert!(
        load_channel_pending_dispatch_marker(&ProviderKind::Codex, &f.shared.token_hash, f.channel)
            .is_none()
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_ct10_gateway_restore_failure_retains_typed_owner() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_130_000).await;
    f.seed().await;
    let ops = f.shared.turn_view_reconciler.ops().len();
    let taken =
        idle_queue_take_next_soft_if_ready(&f.shared, &ProviderKind::Codex, f.channel).await;
    let gateway = f.gateway().await;
    block_restore(&f.queue_path());
    router::QueuedAdmissionDisposition::inject_ledger_for_test(f.channel.get(), false);
    let outcome = gateway
        .dispatch_queued_turn_checked(
            f.channel,
            taken.intervention.as_ref().expect("actual actor dequeue"),
            "owner",
            false,
            taken.dispatch_lease.clone(),
        )
        .await;
    let error = outcome.expect_err("failed actual restore must be a caller error");
    assert!(error.is::<queue_io::QueuedCustodyError>());
    assert!(
        f.shared
            .restart
            .deferred_hook_channels
            .contains_key(&f.channel),
        "existing owner slot retains custody"
    );
    assert!(
        error.to_string().contains("original custody retained=true"),
        "actual typed slot still holds the owner"
    );
    assert_eq!(
        f.shared.turn_view_reconciler.ops().len(),
        ops,
        "failed custody keeps queued UI"
    );
    assert!(
        load_channel_pending_dispatch_marker(&ProviderKind::Codex, &f.shared.token_hash, f.channel)
            .is_some()
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}
