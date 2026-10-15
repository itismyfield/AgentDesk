use super::*;
use crate::services::discord::router::{IntakeExecution, message_handler::IntakeDeps};
use crate::services::discord::{self as discord, DiscordSession, SharedData, queue_dispatch};
use crate::services::turn_orchestrator::{
    InterventionMode, SourceMessageQueuedGeneration, SourceMessageTextSegment,
};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};
use std::{
    cell::RefCell,
    sync::Arc,
    time::{Duration, Instant},
};

struct UploadHook {
    channel: ChannelId,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
thread_local! { static UPLOAD_TAKE: RefCell<Option<UploadHook>> = const { RefCell::new(None) }; }
pub(in crate::services::discord::router) async fn after_upload_take(channel: ChannelId) {
    let hook = UPLOAD_TAKE.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|hook| hook.channel == channel)
        {
            slot.borrow_mut().take()
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        hook.entered.send(()).unwrap();
        hook.release.await.unwrap();
    }
}
fn pause_upload_take(
    channel: ChannelId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (resume, release) = tokio::sync::oneshot::channel();
    UPLOAD_TAKE.with(|slot| {
        *slot.borrow_mut() = Some(UploadHook {
            channel,
            entered,
            release,
        })
    });
    (observed, resume)
}
fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 7,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration {
            message_id: MessageId::new(id),
            queued_generation: 7,
            enqueued_at_epoch_us: Some(123456),
            preserve_on_cancel: true,
        }],
        source_text_segments: vec![SourceMessageTextSegment {
            message_id: MessageId::new(id),
            text: format!("body {id}"),
        }],
        text: format!("body {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}
fn session(path: &std::path::Path) -> DiscordSession {
    DiscordSession {
        session_id: Some("r1-session".into()),
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: Some(path.display().to_string()),
        history: Vec::new(),
        pending_uploads: vec!["U".into()],
        cleared: false,
        remote_profile_name: None,
        channel_id: None,
        channel_name: Some("r1-test".into()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    }
}
struct Fixture {
    shared: Arc<SharedData>,
    channel: ChannelId,
    provider: ProviderKind,
    http: Arc<serenity::Http>,
    proxy: String,
    port: u16,
    redirect: Arc<std::sync::Mutex<Option<ChannelId>>>,
    task: tokio::task::JoinHandle<()>,
    _env: crate::config::TestEnvVarGuard,
    _routing: crate::config::TestEnvVarGuard,
    _config: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(channel: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            "disabled".as_ref(),
        );
        let config = super::local_admission_config_after_shared_test_env_lock(root.path());
        let channel = ChannelId::new(channel);
        let redirect = Arc::new(std::sync::Mutex::new(None::<ChannelId>));
        let target = redirect.clone();
        let app = axum::Router::new().route("/gateway", axum::routing::get(
            |ws: axum::extract::ws::WebSocketUpgrade| async {
                ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
            }
        )).fallback(move |uri: axum::http::Uri| {
            let target = target.clone();
            async move {
                let target = *target.lock().unwrap();
                let id = uri.path().rsplit('/').next().and_then(|id| id.parse::<u64>().ok()).unwrap_or(channel.get());
                let is_thread = target.is_some_and(|target| target.get() == id);
                let body = if uri.path() == "/api/internal/card-thread" {
                    serde_json::json!({"active_thread_id":target.map(|id|id.get().to_string()),"dispatch_type":"implementation"})
                } else {
                    serde_json::json!({"id": id.to_string(), "type": if is_thread {11} else {0}, "name": "r1-test-cdx",
                        "guild_id": "8000000", "position": 0, "permission_overwrites": [], "nsfw": false,
                        "parent_id": if is_thread {Some(channel.get().to_string())} else {None},
                        "thread_metadata":{"archived":false,"locked":false,"auto_archive_duration":1440,"archive_timestamp":"2026-10-12T00:00:00Z"}})
                };
                axum::Json(body)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = format!("http://127.0.0.1:{port}");
        let http = Arc::new(
            serenity::HttpBuilder::new("r1-test-token")
                .proxy(proxy.clone())
                .ratelimiter_disabled(true)
                .build(),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let shared = discord::make_shared_data_for_tests();
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(channel, session(root.path()));
        shared.settings.write().await.provider = ProviderKind::Codex;
        Self {
            shared,
            channel,
            provider: ProviderKind::Codex,
            http,
            proxy,
            port,
            redirect,
            task,
            _env: env,
            _routing: routing,
            _config: config,
            _root: root,
        }
    }
    fn deps(&self) -> IntakeDeps<'_> {
        IntakeDeps {
            http: &self.http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &self.shared,
            token: "r1-test-token",
        }
    }
    async fn enqueue(&self, intervention: Intervention) {
        let result = discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
            discord::mailbox_enqueue_intervention(
                &self.shared,
                &self.provider,
                self.channel,
                intervention,
            ),
        )
        .await;
        assert!(result.enqueued && result.persistence_error.is_none());
    }
    async fn take(&self) -> queue_dispatch::MailboxTakeNextSoftOutcome {
        discord::idle_queue_take_next_soft_if_ready(&self.shared, &self.provider, self.channel)
            .await
    }
    async fn admit(
        &self,
        taken: &queue_dispatch::MailboxTakeNextSoftOutcome,
    ) -> AdmittedQueuedIntake {
        let intervention = taken.intervention.as_ref().unwrap();
        match admit_queued_intake(
            &self.deps(),
            self.provider.clone(),
            self.channel,
            intervention,
            intervention.author_id,
            "owner".into(),
            false,
            false,
            "r1-test",
            taken.dispatch_lease.clone(),
        )
        .await
        {
            QueuedAdmissionDisposition::Admitted(admitted) => admitted,
            _ => panic!("real queued local admission required"),
        }
    }
    async fn ids(&self) -> Vec<u64> {
        discord::mailbox_snapshot(&self.shared, self.channel)
            .await
            .intervention_queue
            .iter()
            .map(|i| i.message_id.get())
            .collect()
    }
}

#[tokio::test]
async fn t1_a_busy_restores_original_uploads_and_new_lease() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Fixture::new(8_001_000).await;
    let registry = Arc::new(discord::health::HealthRegistry::new());
    registry
        .set_utility_bot_user_id_for_tests(discord::bot_role::UtilityBotRole::Announce, 1)
        .await;
    Arc::get_mut(&mut f.shared).unwrap().health_registry = Arc::downgrade(&registry);
    let mut original = item(8_001_002);
    original.author_is_bot = true;
    original
        .source_message_ids
        .insert(0, MessageId::new(8_001_001));
    let mut source = SourceMessageQueuedGeneration::new(MessageId::new(8_001_001), 4);
    source.enqueued_at_epoch_us = Some(111222);
    original.source_message_queued_generations.insert(0, source);
    original.source_text_segments.insert(
        0,
        SourceMessageTextSegment {
            message_id: MessageId::new(8_001_001),
            text: "source one".into(),
        },
    );
    original.text = original
        .source_text_segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    original.voice_announcement = Some(crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "voice body".into(),
        user_id: "42".into(),
        utterance_id: "r1-original-voice".into(),
        language: "ko".into(),
        verbose_progress: true,
        started_at: Some("2026-10-12T20:00:00+09:00".into()),
        completed_at: Some("2026-10-12T20:00:01+09:00".into()),
        samples_written: Some(48000),
        control_channel_id: Some(f.channel.get()),
        stt_mode: Some("file".into()),
        stt_latency_ms: Some(120),
    });
    original.pending_uploads = vec!["Q".into()];
    original.reply_context = Some("original reply".into());
    original.has_reply_boundary = true;
    f.enqueue(original.clone()).await;
    f.enqueue(item(8_001_003)).await;
    let taken = f.take().await;
    let admitted = f.admit(&taken).await;
    let held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    let (entered, release) = pause_upload_take(f.channel);
    let deps = f.deps();
    let finish = finish_admitted_queued_intake(&deps, admitted);
    let arrivals = async {
        entered.await.unwrap();
        f.shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&f.channel)
            .unwrap()
            .pending_uploads
            .push("U2".into());
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(finish, arrivals)
    })
    .await
    .expect("busy must finish");
    assert_eq!(
        f.ids().await,
        [8_001_002, 8_001_003],
        "restore preserves the original front"
    );
    assert!(
        matches!(result.unwrap(), IntakeExecution::Deferred),
        "busy must return literal Deferred"
    );
    let snapshot = discord::mailbox_snapshot(&f.shared, f.channel).await;
    let restored = &snapshot.intervention_queue[0];
    assert_eq!(restored.pending_uploads, ["U", "Q"]);
    assert_eq!(restored.source_message_ids, original.source_message_ids);
    assert_eq!(
        restored
            .source_text_segments
            .iter()
            .map(|s| (s.message_id, &s.text))
            .collect::<Vec<_>>(),
        original
            .source_text_segments
            .iter()
            .map(|s| (s.message_id, &s.text))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        restored
            .source_message_queued_generations
            .iter()
            .map(|g| (
                g.message_id,
                g.queued_generation,
                g.enqueued_at_epoch_us,
                g.preserve_on_cancel
            ))
            .collect::<Vec<_>>(),
        original
            .source_message_queued_generations
            .iter()
            .map(|g| (
                g.message_id,
                g.queued_generation,
                g.enqueued_at_epoch_us,
                g.preserve_on_cancel
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(restored.author_id, original.author_id);
    assert_eq!(restored.author_is_bot, original.author_is_bot);
    assert_eq!(restored.text, original.text);
    assert_eq!(restored.reply_context, original.reply_context);
    assert_eq!(restored.has_reply_boundary, original.has_reply_boundary);
    assert_eq!(restored.merge_consecutive, original.merge_consecutive);
    assert_eq!(restored.voice_announcement, original.voice_announcement);
    assert_eq!(restored.queued_generation, original.queued_generation);
    assert_eq!(restored.created_at, original.created_at);
    assert!(snapshot.pending_user_dispatch.is_none());
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &f.provider,
            &f.shared.token_hash,
            f.channel
        )
        .is_none()
    );
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &f.provider,
        &f.shared.token_hash,
        f.channel,
    );
    assert_eq!(
        disk.iter().map(|i| i.message_id.get()).collect::<Vec<_>>(),
        [8_001_002, 8_001_003]
    );
    assert_eq!(disk[0].pending_uploads, ["U", "Q"]);
    assert_eq!(disk[0].source_message_ids, original.source_message_ids);
    assert_eq!(
        disk[0]
            .source_text_segments
            .iter()
            .map(|s| (s.message_id, &s.text))
            .collect::<Vec<_>>(),
        original
            .source_text_segments
            .iter()
            .map(|s| (s.message_id, &s.text))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        disk[0]
            .source_message_queued_generations
            .iter()
            .map(|g| (
                g.message_id,
                g.queued_generation,
                g.enqueued_at_epoch_us,
                g.preserve_on_cancel
            ))
            .collect::<Vec<_>>(),
        original
            .source_message_queued_generations
            .iter()
            .map(|g| (
                g.message_id,
                g.queued_generation,
                g.enqueued_at_epoch_us,
                g.preserve_on_cancel
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(disk[0].author_id, original.author_id);
    assert_eq!(disk[0].author_is_bot, original.author_is_bot);
    assert_eq!(disk[0].text, original.text);
    assert_eq!(disk[0].reply_context, original.reply_context);
    assert_eq!(disk[0].has_reply_boundary, original.has_reply_boundary);
    assert_eq!(disk[0].merge_consecutive, original.merge_consecutive);
    assert_eq!(disk[0].voice_announcement, original.voice_announcement);
    assert!(
        disk[0]
            .created_at
            .elapsed()
            .abs_diff(original.created_at.elapsed())
            < Duration::from_secs(1)
    );
    assert_eq!(
        f.shared.core.lock().await.sessions[&f.channel].pending_uploads,
        ["U2"]
    );
    drop(held);
    let next = f.take().await;
    assert_eq!(next.intervention.unwrap().message_id, original.message_id);
    assert!(!Arc::ptr_eq(
        taken.dispatch_lease.as_ref().unwrap(),
        next.dispatch_lease.as_ref().unwrap()
    ));
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t1_b_selection_before_restore_must_not_take_tail() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_002_000).await;
    f.enqueue(item(8_002_002)).await;
    f.enqueue(item(8_002_003)).await;
    let taken = f.take().await;
    let admitted = f.admit(&taken).await;
    let (entered, release) = queue_dispatch::input_effect_tests::pause_before_take(f.channel);
    let select = f.take();
    let restore = async {
        entered.await.unwrap();
        let result = finish_admitted_queued_intake(&f.deps(), admitted)
            .await
            .unwrap();
        release.send(()).unwrap();
        result
    };
    let (result, busy) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(select, restore)
    })
    .await
    .unwrap();
    assert!(
        result.intervention.is_none(),
        "old M3 selection must finish without a Take"
    );
    assert!(matches!(busy, IntakeExecution::Deferred));
    assert!(result.selection_stale, "the observed physical head changed");
    assert!(result.dispatch_lease.is_none());
    assert_eq!(f.ids().await, [8_002_002, 8_002_003]);
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &f.provider,
        &f.shared.token_hash,
        f.channel,
    );
    assert_eq!(
        disk.iter().map(|i| i.message_id.get()).collect::<Vec<_>>(),
        [8_002_002, 8_002_003]
    );
    assert!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .pending_user_dispatch
            .is_none()
    );
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &f.provider,
            &f.shared.token_hash,
            f.channel
        )
        .is_none()
    );
    assert_eq!(
        f.take().await.intervention.unwrap().message_id.get(),
        8_002_002
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t1_c_restore_arms_only_sixty_second_wake_with_real_take() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_003_000).await;
    f.enqueue(item(8_003_002)).await;
    f.enqueue(item(8_003_003)).await;
    let taken = f.take().await;
    let admitted = f.admit(&taken).await;
    let held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    let kicks = Arc::new(AtomicUsize::new(0));
    let actual = Arc::new(std::sync::Mutex::new(Vec::new()));
    let channel = f.channel;
    let counted = kicks.clone();
    let observed = actual.clone();
    let _hook = discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, ch, _| {
            let counted = counted.clone();
            let observed = observed.clone();
            Box::pin(async move {
                if ch != channel {
                    return None;
                }
                counted.fetch_add(1, SeqCst);
                let take =
                    discord::idle_queue_take_next_soft_if_ready(&shared, &provider, ch).await;
                if let Some(item) = take.intervention {
                    observed
                        .lock()
                        .unwrap()
                        .push((item.message_id.get(), take.dispatch_lease));
                }
                Some(discord::IdleQueueKickoffChannelOutcome { started: false })
            })
        },
    ));
    let result = finish_admitted_queued_intake(&f.deps(), admitted)
        .await
        .unwrap();
    assert!(matches!(result, IntakeExecution::Deferred));
    tokio::time::pause();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(kicks.load(SeqCst), 0);
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(kicks.load(SeqCst), 0, "no two-second kickoff");
    drop(held);
    assert!(
        f.shared
            .session_transition_lock(channel)
            .try_lock_owned()
            .is_ok(),
        "busy intake adds no transition waiter"
    );
    tokio::time::advance(Duration::from_millis(57_999)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(kicks.load(SeqCst), 0, "no wake before sixty seconds");
    tokio::time::advance(Duration::from_millis(2)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        if !actual.lock().unwrap().is_empty() {
            break;
        }
    }
    assert_eq!(kicks.load(SeqCst), 1);
    assert_eq!(
        actual
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [8_003_002],
        "backstop must really Take M2"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t4_failed_restore_holds_payload_until_sixty_second_retry() {
    use futures::FutureExt;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_004_000).await;
    let mut original = item(8_004_002);
    original.pending_uploads = vec!["Q".into()];
    f.enqueue(original).await;
    f.enqueue(item(8_004_003)).await;
    let taken = f.take().await;
    let admitted = f.admit(&taken).await;
    drop(taken);
    let _held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    let (entered, release) = pause_upload_take(f.channel);
    let deps = f.deps();
    let mut finish = Box::pin(finish_admitted_queued_intake(&deps, admitted));
    tokio::select! {
        result = &mut finish => panic!("finish before upload barrier: {result:?}"),
        _ = entered => {}
    }
    f.shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&f.channel)
        .unwrap()
        .pending_uploads
        .push("U2".into());
    crate::services::turn_orchestrator::test_support::fail_queue_saves(f.channel, 1);
    tokio::time::pause();
    release.send(()).unwrap();
    let consumed = async {
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if crate::services::turn_orchestrator::test_support::queue_save_faults(f.channel, false)
                == 0
            {
                return;
            }
        }
        panic!("restore must attempt its first write");
    };
    tokio::select! { result = &mut finish => panic!("failed restore cannot finish: {result:?}"), _ = consumed => {} }
    assert!(
        finish.as_mut().now_or_never().is_none(),
        "restore owner must enter its retry wait before advancing time"
    );
    assert_eq!(f.ids().await, [8_004_003]);
    assert_eq!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .pending_user_dispatch,
        Some(MessageId::new(8_004_002))
    );
    std::thread::sleep(Duration::from_secs(11));
    let withheld = f
        .shared
        .mailbox(f.channel)
        .take_soft_matching_or_refused(
            queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel),
            None,
            crate::services::turn_orchestrator::TakeHeadCondition::Unchecked,
        )
        .await
        .unwrap();
    assert!(
        withheld.intervention.is_none(),
        "only the owner lease prevents real-time orphan recovery"
    );
    assert_eq!(
        f.shared.core.lock().await.sessions[&f.channel].pending_uploads,
        ["U2"]
    );
    assert_eq!(
        f.shared
            .restart
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "failed restore cannot arm kickoff"
    );
    tokio::time::advance(Duration::from_millis(59_999)).await;
    assert!(finish.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_millis(1)).await;
    let result = tokio::time::timeout(Duration::from_secs(1), finish)
        .await
        .expect("restore retry must finish within the bound")
        .unwrap();
    assert!(matches!(result, IntakeExecution::Deferred));
    assert_eq!(f.ids().await, [8_004_002, 8_004_003]);
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &f.provider,
        &f.shared.token_hash,
        f.channel,
    );
    assert_eq!(disk[0].pending_uploads, ["U", "Q"]);
    assert_eq!(
        f.shared.core.lock().await.sessions[&f.channel].pending_uploads,
        ["U2"]
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t2_fresh_busy_keeps_tail_order() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_005_000).await;
    f.enqueue(item(8_005_003)).await;
    let _held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    let submission = IntakeSubmission {
        provider: f.provider.clone(),
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: true,
        has_nonportable_uploads: false,
        attachments: Vec::new(),
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
        request: super::super::super::message_handler::IntakeRequest {
            intake_outbox_id: None,
            channel_id: f.channel,
            user_msg_id: MessageId::new(8_005_004),
            source_message_ids: vec![MessageId::new(8_005_004)],
            busy_followup_retry_user_msg_id: MessageId::new(8_005_004),
            request_owner: UserId::new(1),
            request_owner_name: "fresh-owner".into(),
            user_text: "fresh M4".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: crate::services::discord::router::TurnKind::Foreground,
            preserve_on_cancel: true,
        },
    };
    let deps = f.deps();
    let IntakeAdmission::Local(permit) = admit_text_intake(&deps, &submission).await else {
        panic!("fresh local admission");
    };
    finish_admitted_local(&deps, permit, submission)
        .await
        .unwrap();
    assert_eq!(f.ids().await, [8_005_003, 8_005_004]);
    let snapshot = discord::mailbox_snapshot(&f.shared, f.channel).await;
    assert_eq!(snapshot.intervention_queue[1].pending_uploads, ["U"]);
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t3_capped_physical_head_allows_eligible_tail() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_006_000).await;
    f.enqueue(item(8_006_002)).await;
    f.enqueue(item(8_006_003)).await;
    for _ in 0..discord::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
        discord::busy_followup_retry_store::record_busy_retry(
            &f.provider,
            f.channel.get(),
            8_006_002,
            8_006_010,
        )
        .unwrap();
    }
    let taken = f.take().await;
    assert!(!taken.selection_stale);
    assert_eq!(
        taken.intervention.unwrap().message_id.get(),
        8_006_003,
        "the condition is the physical head, not selected ID"
    );
    assert_eq!(f.ids().await, [8_006_002]);
    discord::mailbox_abandon_unclaimed_dispatch_after_success(
        &f.shared,
        &f.provider,
        f.channel,
        MessageId::new(8_006_003),
        taken.dispatch_lease.unwrap(),
    )
    .await;
    let capped = f.take().await;
    assert!(capped.intervention.is_none());
    assert_eq!(f.ids().await, [8_006_002]);
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn p2_a_full_identity_changes_refuse_but_matching_identity_takes() {
    use crate::services::turn_orchestrator::{QueueIdentity, TakeHeadCondition};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_007_000).await;
    let original = item(8_007_002);
    let expected = QueueIdentity::of(&original);
    let mut primary_generation = original.clone();
    primary_generation.queued_generation += 1;
    let mut source_generation = original.clone();
    source_generation.source_message_queued_generations[0].queued_generation += 1;
    let mut source_composition = original.clone();
    source_composition
        .source_message_ids
        .push(MessageId::new(8_007_001));
    source_composition.source_message_queued_generations.push(
        SourceMessageQueuedGeneration::user_instruction(MessageId::new(8_007_001), 4),
    );
    let mut source_order = source_composition.clone();
    source_order.source_message_ids.reverse();
    for (head, observed) in [
        (primary_generation, expected.clone()),
        (source_generation, expected.clone()),
        (source_composition.clone(), expected.clone()),
        (source_order, QueueIdentity::of(&source_composition)),
    ] {
        f.shared
            .mailbox(f.channel)
            .replace_queue(
                vec![head.clone()],
                queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel),
            )
            .await;
        let result = f
            .shared
            .mailbox(f.channel)
            .take_soft_matching_or_refused(
                queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel),
                Some(original.message_id),
                TakeHeadCondition::Observed(Some(observed)),
            )
            .await
            .unwrap();
        assert!(
            result.selection_stale,
            "any generation/source/order change invalidates selection"
        );
        assert!(result.intervention.is_none() && result.dispatch_lease.is_none());
        assert_eq!(result.queue_len_after, 1);
        assert!(result.queue_exit_events.is_empty() && result.persistence_error.is_none());
        assert_eq!(f.ids().await, [8_007_002]);
    }
    f.shared
        .mailbox(f.channel)
        .replace_queue(
            vec![original.clone()],
            queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel),
        )
        .await;
    let normal = f
        .shared
        .mailbox(f.channel)
        .take_soft_matching_or_refused(
            queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel),
            Some(original.message_id),
            TakeHeadCondition::Observed(Some(expected)),
        )
        .await
        .unwrap();
    assert!(!normal.selection_stale);
    assert_eq!(normal.intervention.unwrap().message_id, original.message_id);
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t3_observed_empty_is_stale_after_marker_recovery() {
    use crate::services::turn_orchestrator::TakeHeadCondition;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_008_000).await;
    let head = item(8_008_002);
    crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
        &f.provider,
        &f.shared.token_hash,
        f.channel,
        &head,
        None,
    )
    .unwrap();
    let recovered = f
        .shared
        .mailbox(f.channel)
        .take_soft_matching_or_refused(
            queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel),
            None,
            TakeHeadCondition::Observed(None),
        )
        .await
        .unwrap();
    assert!(
        recovered.selection_stale,
        "empty observation is not unchecked"
    );
    assert!(recovered.intervention.is_none() && recovered.dispatch_lease.is_none());
    assert_eq!(f.ids().await, [8_008_002]);
    assert_eq!(
        f.take().await.intervention.unwrap().message_id,
        head.message_id
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn t5_old_lease_cleanup_preserves_new_reservation_and_marker() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_009_000).await;
    f.enqueue(item(8_009_002)).await;
    f.enqueue(item(8_009_003)).await;
    let old = f.take().await;
    let admitted = f.admit(&old).await;
    let held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    let result = finish_admitted_queued_intake(&f.deps(), admitted)
        .await
        .unwrap();
    assert!(matches!(result, IntakeExecution::Deferred));
    assert!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .pending_user_dispatch
            .is_none()
    );
    drop(held);
    let new = f.take().await;
    assert!(!Arc::ptr_eq(
        old.dispatch_lease.as_ref().unwrap(),
        new.dispatch_lease.as_ref().unwrap()
    ));
    discord::mailbox_abandon_unclaimed_dispatch_after_success(
        &f.shared,
        &f.provider,
        f.channel,
        MessageId::new(8_009_002),
        old.dispatch_lease.unwrap(),
    )
    .await;
    assert_eq!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .pending_user_dispatch,
        Some(MessageId::new(8_009_002))
    );
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &f.provider,
            &f.shared.token_hash,
            f.channel
        )
        .is_some()
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn missing_lease_refuses_local_intake_without_consuming_session() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_010_000).await;
    let original = item(8_010_002);
    let result = admit_queued_intake(
        &f.deps(),
        f.provider.clone(),
        f.channel,
        &original,
        original.author_id,
        "owner".into(),
        false,
        false,
        "missing-lease",
        None,
    )
    .await;
    assert!(matches!(
        result,
        QueuedAdmissionDisposition::RejectedRestore
    ));
    assert_eq!(
        f.shared.core.lock().await.sessions[&f.channel].pending_uploads,
        ["U"]
    );
    assert!(f.ids().await.is_empty());
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[cfg(unix)]
#[tokio::test]
async fn s2b_on_bidirectional_claims_and_reserved_front_protection() {
    use crate::services::turn_orchestrator::{QueueIdentity, QueuedHeadClaim};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let mut f = Fixture::new(8_011_000).await;
    f.provider = ProviderKind::Claude;
    f.shared.settings.write().await.provider = f.provider.clone();
    let pane = discord::health::InjectPane::new(f.channel.get(), "all");
    discord::health::inject_hook::set_queued_head(f.channel.get(), true);
    let _gate = crate::services::discord::router::busy_inject_support::open_gate(f.channel.get());
    assert!(crate::services::discord::router::queued_head_inject_covers(
        f.channel, None
    ));
    let head = item(8_011_002);
    let tail = item(8_011_003);
    f.enqueue(head.clone()).await;
    f.enqueue(tail.clone()).await;
    let taken = f.take().await;
    let context = || queue_dispatch::persistence_context(&f.shared, &f.provider, f.channel);
    let claim = f
        .shared
        .mailbox(f.channel)
        .claim_queued_head_if_enabled(
            QueueIdentity::of(&tail),
            context(),
            f.channel,
            None,
            crate::services::discord::router::queued_head_inject_covers,
        )
        .await;
    assert!(
        matches!(claim, QueuedHeadClaim::Contended),
        "DispatchLease+marker, not ineligibility, blocks a real eligible S2b claim"
    );
    let admitted = f.admit(&taken).await;
    let held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    // No session attachment may make the eligible original ineligible on restore.
    f.shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&f.channel)
        .unwrap()
        .pending_uploads
        .clear();
    assert!(matches!(
        finish_admitted_queued_intake(&f.deps(), admitted)
            .await
            .unwrap(),
        IntakeExecution::Deferred
    ));
    assert!(
        matches!(
            f.shared
                .mailbox(f.channel)
                .claim_queued_head_if_enabled(
                    QueueIdentity::of(&tail),
                    context(),
                    f.channel,
                    None,
                    crate::services::discord::router::queued_head_inject_covers
                )
                .await,
            QueuedHeadClaim::Contended
        ),
        "the old M3 identity loses after M2 restoration"
    );
    let claim = f
        .shared
        .mailbox(f.channel)
        .claim_queued_head_if_enabled(
            QueueIdentity::of(&head),
            context(),
            f.channel,
            None,
            crate::services::discord::router::queued_head_inject_covers,
        )
        .await;
    let QueuedHeadClaim::Reserved(ticket) = claim else {
        panic!("fixture must reach a real S2b reservation");
    };
    drop(held);
    assert!(
        f.take().await.intervention.is_none(),
        "S2b reservation withholds general automatic Take"
    );
    let intrusion = discord::mailbox_restore_dequeued_head(
        &f.shared,
        &f.provider,
        f.channel,
        item(8_011_001),
        taken.dispatch_lease.unwrap(),
    )
    .await;
    assert!(
        !intrusion.enqueued && intrusion.persistence_error.is_some(),
        "queue write cannot move a reserved head"
    );
    assert_eq!(f.ids().await, [8_011_002, 8_011_003]);
    assert_eq!(
        pane.tmux_calls(),
        0,
        "claims alone cannot submit to the pane"
    );
    drop(ticket);
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[cfg(unix)]
#[tokio::test]
async fn p2_b_actual_redirect_busy_restores_origin_channel() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::intake_dispatch::queued::transition_busy_tests::",
        "p2_b_actual_redirect_busy_restores_origin_channel"
    )) {
        return;
    }
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_012_000).await;
    let target = ChannelId::new(8_012_100);
    *f.redirect.lock().unwrap() = Some(target);
    discord::internal_api::init(f.port, None);
    f.shared
        .core
        .lock()
        .await
        .sessions
        .insert(target, session(f._root.path()));
    let mut original = item(8_012_002);
    original.text = "DISPATCH:r1-redirect - body".into();
    original.source_text_segments[0].text = original.text.clone();
    original.pending_uploads = vec!["Q".into()];
    f.enqueue(original).await;
    f.enqueue(item(8_012_003)).await;
    let taken = f.take().await;
    let admitted = f.admit(&taken).await;
    let _held = f.shared.session_transition_lock(target).lock_owned().await;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        finish_admitted_queued_intake(&f.deps(), admitted),
    )
    .await
    .unwrap()
    .unwrap();
    // Only the target's transition lock is held, so deferral proves the redirect resolved it.
    assert!(matches!(result, IntakeExecution::Deferred));
    assert!(
        !f.shared.dispatch.thread_parents.contains_key(&f.channel),
        "a route without a claim is never published"
    );
    assert_eq!(f.ids().await, [8_012_002, 8_012_003]);
    let original_queue = discord::mailbox_snapshot(&f.shared, f.channel).await;
    assert_eq!(
        original_queue.intervention_queue[0].pending_uploads,
        ["U", "Q"]
    );
    assert!(original_queue.pending_user_dispatch.is_none());
    let redirected = discord::mailbox_snapshot(&f.shared, target).await;
    assert!(redirected.intervention_queue.is_empty() && redirected.pending_user_dispatch.is_none());
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &f.provider,
            &f.shared.token_hash,
            target
        )
        .is_none()
    );
    assert!(
        f.shared
            .restart
            .deferred_hook_channels
            .contains_key(&f.channel)
    );
    assert!(
        !f.shared
            .restart
            .deferred_hook_channels
            .contains_key(&target)
    );
    assert_eq!(
        f.shared.core.lock().await.sessions[&target].pending_uploads,
        ["U"],
        "redirect target session uploads are not retaken"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
    f.shared.mailboxes.remove_fixture_for_test(target);
}

#[tokio::test]
async fn t5_kickoff_busy_reports_no_start_after_durable_restore() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_TEST_CODEX_PROMOTE_READY",
        "1".as_ref(),
    );
    let f = Fixture::new(8_013_000).await;
    let settings = f.shared.settings.read().await.clone();
    discord::session_runtime::validate_rest_channel_routing(
        &f.http,
        None,
        &f.provider,
        &settings,
        f.channel,
        None,
    )
    .await
    .expect("fixture provider routing must be accepted");
    let mut original = item(8_013_002);
    original.pending_uploads = vec!["Q".into()];
    f.enqueue(original).await;
    f.enqueue(item(8_013_003)).await;
    let (entered, release) = pause_upload_take(f.channel);
    let deps = f.deps();
    let kickoff =
        queue_dispatch::kickoff::kickoff_idle_queue_channel(&deps, &f.provider, f.channel);
    let lock = async {
        entered.await.unwrap();
        let held = f
            .shared
            .session_transition_lock(f.channel)
            .lock_owned()
            .await;
        release.send(()).unwrap();
        held
    };
    let (outcome, _held) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(kickoff, lock)
    })
    .await
    .expect("KICKOFF must reach real intake");
    assert!(!outcome.started);
    assert_eq!(f.ids().await, [8_013_002, 8_013_003]);
    assert_eq!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .intervention_queue[0]
            .pending_uploads,
        ["U", "Q"]
    );
    assert!(
        f.shared.core.lock().await.sessions[&f.channel]
            .pending_uploads
            .is_empty(),
        "real session take is mandatory"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[cfg(unix)]
#[tokio::test]
async fn t5_gateway_busy_returns_ok_after_durable_restore() {
    use discord::gateway::{DiscordGateway, LiveDiscordTurnContext, TurnGateway};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_TEST_CODEX_PROMOTE_READY",
        "1".as_ref(),
    );
    let f = Fixture::new(8_014_000).await;
    let mut original = item(8_014_002);
    original.pending_uploads = vec!["Q".into()];
    f.enqueue(original).await;
    f.enqueue(item(8_014_003)).await;
    let taken = f.take().await;
    let ctx = discord::tui_prompt_relay::relay_e2e::discord_mock::serenity_context(
        f.proxy.clone(),
        format!("ws://127.0.0.1:{}/gateway", f.port),
    )
    .await;
    let gateway = DiscordGateway::new(
        f.http.clone(),
        f.shared.clone(),
        f.provider.clone(),
        Some(LiveDiscordTurnContext {
            ctx,
            token: "r1-test-token".into(),
            request_owner: UserId::new(1),
        }),
    );
    let _held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    gateway
        .dispatch_queued_turn(
            f.channel,
            taken.intervention.as_ref().unwrap(),
            "owner",
            false,
            taken.dispatch_lease.clone(),
        )
        .await
        .expect("queued busy must not escape as Err");
    assert_eq!(f.ids().await, [8_014_002, 8_014_003]);
    assert_eq!(
        discord::mailbox_snapshot(&f.shared, f.channel)
            .await
            .intervention_queue[0]
            .pending_uploads,
        ["U", "Q"]
    );
    assert!(
        f.shared.core.lock().await.sessions[&f.channel]
            .pending_uploads
            .is_empty()
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

#[tokio::test]
async fn legacy_array_bot_busy_preserves_restored_original() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_015_000).await;
    let mut head = item(8_015_002);
    head.author_is_bot = true;
    head.pending_uploads = vec!["Q".into()];
    f.enqueue(head).await;
    f.enqueue(item(8_015_003)).await;
    let path = discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(f.provider.as_str())
        .join(&f.shared.token_hash)
        .join(format!("{}.json", f.channel.get()));
    let mut legacy: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for item in legacy.as_array_mut().unwrap() {
        for key in [
            "queued_generation",
            "source_message_ids",
            "source_message_queued_generations",
            "source_text_segments",
            "created_at_wall_time_ms",
        ] {
            item.as_object_mut().unwrap().remove(key);
        }
    }
    std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
    let hydration = f
        .shared
        .mailbox(f.channel)
        .hydrate_pending_queue_from_disk_or_refused(queue_dispatch::persistence_context(
            &f.shared,
            &f.provider,
            f.channel,
        ))
        .await
        .unwrap();
    assert!(hydration.persistence_error.is_none());
    let taken = f.take().await;
    let original = taken.intervention.as_ref().unwrap().clone();
    assert!(original.author_is_bot);
    let admitted = f.admit(&taken).await;
    let _held = f
        .shared
        .session_transition_lock(f.channel)
        .lock_owned()
        .await;
    assert!(matches!(
        finish_admitted_queued_intake(&f.deps(), admitted)
            .await
            .unwrap(),
        IntakeExecution::Deferred
    ));
    assert_eq!(f.ids().await, [8_015_002, 8_015_003]);
    let snapshot = discord::mailbox_snapshot(&f.shared, f.channel).await;
    let restored = &snapshot.intervention_queue[0];
    assert_eq!(restored.pending_uploads, ["U", "Q"]);
    assert_eq!(restored.queued_generation, original.queued_generation);
    assert_eq!(restored.source_message_ids, original.source_message_ids);
    assert_eq!(restored.author_is_bot, original.author_is_bot);
    assert_eq!(restored.created_at, original.created_at);
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(
        persisted.is_array(),
        "restore keeps the legacy array contract without S2b metadata"
    );
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &f.provider,
        &f.shared.token_hash,
        f.channel,
    );
    assert_eq!(disk[0].pending_uploads, ["U", "Q"]);
    assert_eq!(disk[0].author_is_bot, original.author_is_bot);
    assert!(
        disk[0]
            .created_at
            .elapsed()
            .abs_diff(original.created_at.elapsed())
            < Duration::from_secs(1),
        "disk timestamps preserve age rather than identical reconstructed Instants"
    );
    f.shared.mailboxes.remove_fixture_for_test(f.channel);
}

mod retry_tests;
