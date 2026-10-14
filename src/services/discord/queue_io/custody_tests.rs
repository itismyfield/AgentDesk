use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::turn_orchestrator::{
    DispatchLease, SourceMessageQueuedGeneration, SourceMessageTextSegment,
    load_channel_pending_dispatch_marker_checked, load_channel_pending_queue_checked,
};
use serde_json::{Value, json};
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub(in crate::services::discord::queue_io) fn mutate_loan(
    owner: &mut Option<Box<router::QueuedLedgerOutcome>>,
) {
    if crate::services::discord::input_runtime::supervisor::mutant("c2c2_root_loan_forgets_owner") {
        drop(owner.take());
        println!("C2C2_ROOT_MUTATED=loan_forgets_owner");
    }
}

pub(in crate::services::discord::queue_io) fn mutate_worker_drop() -> Option<bool> {
    use crate::services::discord::input_runtime::supervisor::mutant;
    if mutant("c2c2_root_guard_removes_owned_slot") {
        println!("C2C2_ROOT_MUTATED=guard_removes_owned_slot");
        Some(true)
    } else if mutant("c2c2_root_worker_remains_active") {
        println!("C2C2_ROOT_MUTATED=worker_remains_active");
        Some(false)
    } else {
        None
    }
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
        self.shared
            .restart
            .deferred_hook_channels
            .remove(&self.channel);
        self.shared.mailboxes.remove_fixture_for_test(self.channel);
    }
}

fn original() -> Intervention {
    let generation = runtime_store::process_generation();
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(10),
        queued_generation: generation,
        source_message_ids: vec![MessageId::new(11), MessageId::new(10)],
        source_message_queued_generations: vec![
            SourceMessageQueuedGeneration::user_instruction(MessageId::new(11), generation),
            SourceMessageQueuedGeneration::new(MessageId::new(10), generation),
        ],
        source_text_segments: vec![
            SourceMessageTextSegment::new(MessageId::new(11), "absorbed original"),
            SourceMessageTextSegment::new(MessageId::new(10), "primary original"),
        ],
        text: "absorbed original\nprimary original".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: Some("complete original reply".into()),
        has_reply_boundary: true,
        merge_consecutive: false,
        pending_uploads: vec!["Q1".into(), "Q2".into()],
        voice_announcement: Some(crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: "spoken original".into(),
            user_id: "7".into(),
            utterance_id: "root-custody-10".into(),
            language: "ko".into(),
            verbose_progress: true,
            started_at: Some("2026-10-14T00:00:00Z".into()),
            completed_at: Some("2026-10-14T00:00:01Z".into()),
            samples_written: Some(240),
            control_channel_id: None,
            stt_mode: Some("fixture".into()),
            stt_latency_ms: Some(17),
        }),
    }
}

fn second_original() -> Intervention {
    let mut item = original();
    item.author_id = UserId::new(8);
    item.message_id = MessageId::new(12);
    item.queued_generation += 1;
    item.source_message_ids = vec![MessageId::new(13), MessageId::new(12)];
    item.source_message_queued_generations = vec![
        SourceMessageQueuedGeneration::new(MessageId::new(13), item.queued_generation),
        SourceMessageQueuedGeneration::user_instruction(MessageId::new(12), item.queued_generation),
    ];
    item.source_text_segments = vec![
        SourceMessageTextSegment::new(MessageId::new(13), "second absorbed"),
        SourceMessageTextSegment::new(MessageId::new(12), "second primary"),
    ];
    item.text = "second absorbed\nsecond primary".into();
    item.reply_context = Some("second original reply".into());
    item.merge_consecutive = true;
    item.pending_uploads = vec!["B1".into(), "B2".into(), "B3".into()];
    let voice = item.voice_announcement.as_mut().unwrap();
    voice.transcript = "second spoken original".into();
    voice.user_id = "8".into();
    voice.utterance_id = "root-custody-12".into();
    voice.language = "en".into();
    voice.verbose_progress = false;
    voice.stt_latency_ms = None;
    item
}

fn raw_snapshot(item: &Intervention) -> Value {
    json!({
        "author": item.author_id.get(), "author_is_bot": item.author_is_bot,
        "primary": item.message_id.get(), "generation": item.queued_generation,
        "sources": item.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
        "owners": item.source_message_queued_generations.iter().map(|owner|
            (owner.message_id.get(), owner.queued_generation,
             owner.enqueued_at_epoch_us, owner.preserve_on_cancel)).collect::<Vec<_>>(),
        "segments": item.source_text_segments.iter().map(|part|
            (part.message_id.get(), &part.text)).collect::<Vec<_>>(),
        "text": item.text, "mode": format!("{:?}", item.mode),
        "reply": item.reply_context, "reply_boundary": item.has_reply_boundary,
        "merge": item.merge_consecutive, "uploads": item.pending_uploads,
        "voice": item.voice_announcement,
    })
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
        ];
        let channel = ChannelId::new(channel);
        let app = axum::Router::new()
            .route(
                "/gateway",
                axum::routing::get(|ws: axum::extract::ws::WebSocketUpgrade| async {
                    ws.on_upgrade(
                        |mut socket| async move { while socket.recv().await.is_some() {} },
                    )
                }),
            )
            .fallback(move |method: axum::http::Method| async move {
                use axum::response::IntoResponse;
                if method == axum::http::Method::GET {
                    axum::Json(
                        json!({"id":channel.get().to_string(),"type":0,"name":"custody-test-cdx",
                    "guild_id":"8000000","position":0,"permission_overwrites":[],"nsfw":false,
                    "parent_id":null}),
                    )
                    .into_response()
                } else {
                    axum::http::StatusCode::NO_CONTENT.into_response()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = format!("http://127.0.0.1:{port}");
        let http = Arc::new(
            serenity::HttpBuilder::new("custody-test-token")
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
            token: "custody-test-token",
        }
    }

    async fn enable_transport(&self) {
        let ctx = discord_mock::serenity_context(
            self.proxy.clone(),
            format!("ws://127.0.0.1:{}/gateway", self.port),
        )
        .await;
        assert!(self.shared.http.cached_serenity_ctx.set(ctx).is_ok());
        assert!(
            self.shared
                .http
                .cached_bot_token
                .set("custody-test-token".into())
                .is_ok()
        );
        assert!(QueueTransport::from_runtime(&self.shared).is_some());
    }

    async fn take(&self) -> (Intervention, Arc<DispatchLease>) {
        self.take_original(original()).await
    }

    async fn take_original(&self, original: Intervention) -> (Intervention, Arc<DispatchLease>) {
        self.shared
            .mailbox(self.channel)
            .replace_queue(
                vec![original],
                queue_dispatch::persistence_context(
                    &self.shared,
                    &ProviderKind::Codex,
                    self.channel,
                ),
            )
            .await;
        assert_eq!(
            self.disk_queue().len(),
            1,
            "seed must persist before actual dequeue"
        );
        let taken = self
            .shared
            .mailbox(self.channel)
            .take_soft_matching_or_refused(
                queue_dispatch::persistence_context(
                    &self.shared,
                    &ProviderKind::Codex,
                    self.channel,
                ),
                None,
                crate::services::turn_orchestrator::TakeHeadCondition::Unchecked,
            )
            .await
            .unwrap();
        assert!(taken.persistence_error.is_none());
        let lease = taken
            .dispatch_lease
            .expect("actual actor provides its Arc lease");
        let raw = taken
            .intervention
            .expect("actual actor dequeues the rich original");
        assert!(self.disk_queue().is_empty());
        assert!(self.marker_present());
        (raw, lease)
    }

    async fn carrier(
        &self,
        raw: &Intervention,
        lease: Arc<DispatchLease>,
        received: bool,
    ) -> Box<router::QueuedLedgerOutcome> {
        router::QueuedAdmissionDisposition::inject_ledger_for_original_test(
            self.channel.get(),
            raw,
            41,
            received,
        );
        self.admit_carrier(raw, lease).await
    }

    async fn bound_carrier(
        &self,
        raw: &Intervention,
        lease: Arc<DispatchLease>,
        received_seq: u64,
    ) -> Box<router::QueuedLedgerOutcome> {
        router::QueuedAdmissionDisposition::inject_ledger_for_original_test(
            self.channel.get(),
            raw,
            received_seq,
            true,
        );
        self.admit_carrier(raw, lease).await
    }

    async fn admit_carrier(
        &self,
        raw: &Intervention,
        lease: Arc<DispatchLease>,
    ) -> Box<router::QueuedLedgerOutcome> {
        let admitted = router::admit_queued_intake(
            &self.deps(),
            ProviderKind::Codex,
            self.channel,
            raw,
            UserId::new(99),
            "execution owner".into(),
            false,
            false,
            "custody fixture",
            Some(lease),
        )
        .await;
        let router::QueuedAdmissionDisposition::Admitted(admitted) = admitted else {
            panic!("C carrier must preserve the already acquired proof");
        };
        let error = router::finish_admitted_queued_intake(&self.deps(), admitted)
            .await
            .unwrap_err();
        assert!(error.is::<router::QueuedLedgerOutcome>());
        let outcome = error.downcast::<router::QueuedLedgerOutcome>().unwrap();
        assert!(
            outcome.proof_for_test().is_some(),
            "complete immutable proof fixture is present"
        );
        outcome
    }

    fn disk_queue(&self) -> Vec<Intervention> {
        load_channel_pending_queue_checked(
            &ProviderKind::Codex,
            &self.shared.token_hash,
            self.channel,
        )
        .expect("checked durable queue readback")
        .0
    }

    fn marker_present(&self) -> bool {
        load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Codex,
            &self.shared.token_hash,
            self.channel,
        )
        .expect("checked marker readback")
        .is_some()
    }

    fn slot(&self) -> Arc<BackstopSlot> {
        self.shared
            .restart
            .deferred_hook_channels
            .get(&self.channel)
            .expect("custody has a synchronous retained slot")
            .clone()
    }

    async fn release_actor(&self, raw: &Intervention, lease: Arc<DispatchLease>) {
        assert!(
            self.shared
                .mailbox(self.channel)
                .abandon_pending_dispatch_if_lease_matches(
                    raw.message_id,
                    lease,
                    queue_dispatch::persistence_context(
                        &self.shared,
                        &ProviderKind::Codex,
                        self.channel
                    ),
                )
                .await
        );
        self.assert_no_raw_backlog().await;
    }

    async fn assert_no_raw_backlog(&self) {
        let snapshot = self
            .shared
            .mailbox(self.channel)
            .try_snapshot()
            .await
            .unwrap();
        assert!(snapshot.intervention_queue.is_empty());
        assert!(snapshot.pending_user_dispatch.is_none());
        assert!(self.disk_queue().is_empty());
        assert!(!self.marker_present());
    }
}

fn assert_retained(
    slot: &BackstopSlot,
    raw: &Intervention,
    lease: &Weak<DispatchLease>,
    proof: Option<&str>,
) {
    let held = slot.custody.lock().unwrap();
    assert_eq!(
        held.len(),
        1,
        "the sole raw/proof owner remains in the slot"
    );
    let owner = held.front().unwrap();
    assert_eq!(raw_snapshot(owner.original()), raw_snapshot(raw));
    assert_eq!(owner.original().created_at, raw.created_at);
    assert!(
        Weak::ptr_eq(&Arc::downgrade(owner.dispatch_lease().unwrap()), lease),
        "the same actual actor Arc survives"
    );
    assert_eq!(owner.proof_for_test().as_deref(), proof);
    drop(held);
    assert!(slot.has_custody());
}

async fn wait_for_worker(channel: ChannelId) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !cancel_backstop_test_support::backstop_waiting(channel) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual slow worker reaches its waiting boundary");
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_root_unpolled_consume_drop_retains_complete_proof_and_actual_lease() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_135_000).await;
    let (raw, lease) = f.take().await;
    let weak = Arc::downgrade(&lease);
    let outcome = f.carrier(&raw, lease, true).await;
    let proof = outcome.proof_for_test().unwrap();
    let deps = f.deps();
    let attempt = consume_queued_custody(&deps, outcome, None);
    let slot = f.slot();
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 1);
    assert!(slot.custody.lock().unwrap().is_empty());
    drop(attempt);
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 0);
    assert_retained(&slot, &raw, &weak, Some(&proof));
    let snapshot = f.shared.mailbox(f.channel).try_snapshot().await.unwrap();
    assert_eq!(snapshot.pending_user_dispatch, Some(raw.message_id));
    assert_eq!(
        idle_queue_backstop_backlog_units(&f.shared, &ProviderKind::Codex, f.channel, &snapshot),
        1
    );
    assert!(f.marker_present());
    println!(
        "C2C2_ROOT_UNPOLLED_CANCEL actual_dequeue=true first_poll=0 full_proof=true same_actual_arc=true"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_root_provisional_drop_is_backlog_without_queue_or_marker() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_136_000).await;
    let (raw, lease) = f.take().await;
    let weak = Arc::downgrade(&lease);
    f.release_actor(&raw, lease.clone()).await;
    let owner = QueuedAdmissionOwner::new(
        &f.shared,
        &ProviderKind::Codex,
        f.channel,
        &raw,
        Some(lease),
    );
    let slot = f.slot();
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 1);
    drop(owner);
    assert_retained(&slot, &raw, &weak, None);
    f.assert_no_raw_backlog().await;
    let snapshot = f.shared.mailbox(f.channel).try_snapshot().await.unwrap();
    assert_eq!(
        idle_queue_backstop_backlog_units(&f.shared, &ProviderKind::Codex, f.channel, &snapshot),
        1,
        "provisional custody itself is recovery backlog"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_root_worker_abort_keeps_same_slot_and_rearms_without_raw_backlog() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_137_000).await;
    let (raw, lease) = f.take().await;
    let weak = Arc::downgrade(&lease);
    let outcome = f.carrier(&raw, lease.clone(), false).await;
    let proof = outcome.proof_for_test().unwrap();
    f.release_actor(&raw, lease).await;
    let slot = Arc::new(BackstopSlot::new());
    slot.custody.lock().unwrap().push_back(outcome);
    slot.worker_active.store(true, Ordering::Release);
    assert!(
        f.shared
            .restart
            .deferred_hook_channels
            .insert(f.channel, slot.clone())
            .is_none()
    );
    f.shared
        .restart
        .deferred_hook_backlog
        .fetch_add(1, Ordering::Relaxed);
    let shared = f.shared.clone();
    let channel = f.channel;
    let worker_slot = slot.clone();
    let worker = tokio::spawn(async move {
        let mut guard = DeferredHookBacklogGuard {
            shared: shared.clone(),
            channel_id: channel,
            slot: worker_slot.clone(),
            active: true,
        };
        run_slow_idle_queue_backstop_loop(
            &shared,
            &ProviderKind::Codex,
            channel,
            "custody abort fixture",
            &worker_slot,
            &mut guard,
        )
        .await;
    });
    wait_for_worker(f.channel).await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(Arc::ptr_eq(&f.slot(), &slot));
    assert!(!slot.worker_active.load(Ordering::Acquire));
    assert_retained(&slot, &raw, &weak, Some(&proof));
    f.assert_no_raw_backlog().await;
    assert!(
        schedule_single_slow_idle_queue_backstop(
            f.shared.clone(),
            ProviderKind::Codex,
            f.channel,
            "custody rearm fixture",
            1
        ),
        "cancelled worker can be claimed again"
    );
    assert!(Arc::ptr_eq(&f.slot(), &slot));
    assert!(slot.worker_active.load(Ordering::Acquire));
    assert_retained(&slot, &raw, &weak, Some(&proof));
    let snapshot = f.shared.mailbox(f.channel).try_snapshot().await.unwrap();
    assert_eq!(
        idle_queue_backstop_backlog_units(&f.shared, &ProviderKind::Codex, f.channel, &snapshot),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_root_two_arrivals_keep_fifo_proofs_and_distinct_actual_leases() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_139_000).await;
    let (first, first_lease) = f.take().await;
    let first_weak = Arc::downgrade(&first_lease);
    let first_carrier = f.bound_carrier(&first, first_lease.clone(), 41).await;
    let first_proof = first_carrier.proof_for_test().unwrap();
    let deps = f.deps();
    let first_attempt = consume_queued_custody(&deps, first_carrier, None);
    let slot = f.slot();
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 1);
    f.release_actor(&first, first_lease).await;

    let (second, second_lease) = f.take_original(second_original()).await;
    let second_weak = Arc::downgrade(&second_lease);
    assert!(!Weak::ptr_eq(&first_weak, &second_weak));
    let second_carrier = f.bound_carrier(&second, second_lease, 42).await;
    let second_proof = second_carrier.proof_for_test().unwrap();
    assert_ne!(
        first_proof, second_proof,
        "each source has its own bound canonical proof"
    );
    assert_ne!(raw_snapshot(&first), raw_snapshot(&second));
    let second_attempt = consume_queued_custody(&deps, second_carrier, None);
    assert!(Arc::ptr_eq(&slot, &f.slot()));
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 2);
    assert!(slot.custody.lock().unwrap().is_empty());

    drop(first_attempt);
    assert!(Arc::ptr_eq(&slot, &f.slot()));
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 1);
    assert_retained(&slot, &first, &first_weak, Some(&first_proof));
    assert_eq!(
        first_weak.strong_count(),
        1,
        "the first owner is the only holder of the old Arc"
    );
    assert_eq!(
        second_weak.strong_count(),
        2,
        "the actor and second inflight owner hold the new Arc"
    );
    let snapshot = f.shared.mailbox(f.channel).try_snapshot().await.unwrap();
    assert_eq!(snapshot.pending_user_dispatch, Some(second.message_id));
    assert_eq!(
        idle_queue_backstop_backlog_units(&f.shared, &ProviderKind::Codex, f.channel, &snapshot),
        1,
        "dropping one loan cannot remove the other inflight owner slot"
    );

    drop(second_attempt);
    assert!(Arc::ptr_eq(&slot, &f.slot()));
    assert_eq!(slot.custody_inflight.load(Ordering::Acquire), 0);
    let held = slot.custody.lock().unwrap();
    assert_eq!(
        held.len(),
        2,
        "both arrivals survive without overwriting either owner"
    );
    for (owner, original, weak, proof) in [
        (&held[0], &first, &first_weak, &first_proof),
        (&held[1], &second, &second_weak, &second_proof),
    ] {
        assert_eq!(raw_snapshot(owner.original()), raw_snapshot(original));
        assert_eq!(owner.original().created_at, original.created_at);
        assert!(Weak::ptr_eq(
            &Arc::downgrade(owner.dispatch_lease().unwrap()),
            weak
        ));
        assert_eq!(owner.proof_for_test().as_ref(), Some(proof));
    }
    drop(held);
    assert!(slot.has_custody());
    println!(
        "C2C2_ROOT_MULTIARRIVAL actual_dequeues=2 fifo_owners=2 independent_bound_proofs=true distinct_actual_arcs=true inflight_drop_preserved_slot=true"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn c2c2_root_real_sixty_second_retry_keeps_lease_past_orphan_boundary() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let f = Fixture::new(8_138_000).await;
    f.enable_transport().await;
    let (raw, lease) = f.take().await;
    let weak = Arc::downgrade(&lease);
    let outcome = f.carrier(&raw, lease, false).await;
    let proof = outcome.proof_for_test().unwrap();
    let deps = f.deps();
    let attempt = consume_queued_custody(&deps, outcome, None);
    let slot = f.slot();
    drop(attempt);
    let waiting_at = Instant::now();
    wait_for_worker(f.channel).await;
    assert_retained(&slot, &raw, &weak, Some(&proof));
    {
        let owner = slot.custody.lock().unwrap();
        assert_eq!(
            Arc::strong_count(owner.front().unwrap().dispatch_lease().unwrap()),
            2,
            "only actor and retained owner hold the actual lease"
        );
    }
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert!(
        waiting_at.elapsed() >= Duration::from_secs(11),
        "orphan boundary uses real std Instant"
    );
    let withheld = f
        .shared
        .mailbox(f.channel)
        .take_soft_matching_or_refused(
            queue_dispatch::persistence_context(&f.shared, &ProviderKind::Codex, f.channel),
            None,
            crate::services::turn_orchestrator::TakeHeadCondition::Unchecked,
        )
        .await
        .unwrap();
    assert!(
        withheld.intervention.is_none(),
        "owner prevents actual orphan recovery past ten seconds"
    );
    assert!(f.disk_queue().is_empty());
    assert!(f.marker_present());
    assert_retained(&slot, &raw, &weak, Some(&proof));
    tokio::time::sleep(Duration::from_secs(50).saturating_sub(waiting_at.elapsed())).await;
    assert!(
        waiting_at.elapsed() < Duration::from_secs(60),
        "pre-deadline observation must precede real retry"
    );
    assert!(
        f.disk_queue().is_empty(),
        "actual slow custody retry does not restore before sixty seconds"
    );
    assert_retained(&slot, &raw, &weak, Some(&proof));
    let restored = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let items = f.disk_queue();
            if !items.is_empty() {
                break items;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("one actual slow retry restores within a bounded real window");
    assert!(
        waiting_at.elapsed() >= Duration::from_secs(60),
        "retry follows real sixty-second boundary"
    );
    assert_eq!(restored.len(), 1);
    assert_eq!(raw_snapshot(&restored[0]), raw_snapshot(&raw));
    assert!(
        (restored[0].created_at.elapsed().as_secs_f64() - raw.created_at.elapsed().as_secs_f64())
            .abs()
            < 0.1,
        "reopened queue retains original age within serialized precision"
    );
    let snapshot = f.shared.mailbox(f.channel).try_snapshot().await.unwrap();
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(
        raw_snapshot(&snapshot.intervention_queue[0]),
        raw_snapshot(&raw)
    );
    assert!(snapshot.pending_user_dispatch.is_none());
    assert!(!f.marker_present());
    tokio::time::timeout(Duration::from_secs(2), async {
        while slot.has_custody() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successful checked restore releases its custody owner");
    println!(
        "C2C2_ROOT_REAL_BOUNDARIES elapsed_ms={} orphan_probe_after_10s=true restore_after_60s=true actual_same_arc=true",
        waiting_at.elapsed().as_millis()
    );
}
