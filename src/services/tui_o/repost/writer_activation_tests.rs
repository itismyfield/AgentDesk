//! Activation contracts exercised through the channel writer's real dispatch and ledger.

use super::*;
use crate::services::cluster::channel_home::{self, HomeGate};
use crate::services::tui_o::repost::io::probe::evidence::{FIRST_PASS_AFTER, SECOND_PASS_AFTER};
use crate::services::tui_o::writer::deliver::POST_TIMEOUT;
use sha2::Digest;

async fn start(
    runner: &mut Runner,
    writer: &mut Writer,
    pool: &PgPool,
    key: &PieceKey,
    io: &Io,
) -> dispatch::Started {
    let intent = evidence(pool, key).await;
    let admission = runner.try_next_dispatch(writer, OriginalGate::Admitted, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("the healthy writer reaches dispatch");
    };
    dispatch::start(writer, permit, lease, io).await
}

/// Both stop receivers and the effective validated config remain live after a grant.
async fn withdraw_while_429(kind: u8) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for revoke in [false, true] {
        let key = admitted(&pool, &format!("withdraw-{kind}-{revoke}"), "piece").await;
        let harness = Harness::new();
        harness.gate.acquired();
        let (configured, watched) = watch::channel(Some(config(true)));
        let (actor, actor_stop) = watch::channel(false);
        let (host, host_stop) = watch::channel(false);
        let switch = RepostSwitch::new(None, watched);
        let mut writer = harness
            .writer()
            .with_repost_stop(actor_stop)
            .with_repost_stop(host_stop);
        writer.set_live(true);
        writer.bind_switch(switch.live());
        let mut runner = Runner::new(switch, pool.clone(), BOT, run_scope());
        let io = Io::over(&harness.port);
        io.throttle_first.store(true, Ordering::SeqCst);
        let started = start(&mut runner, &mut writer, &pool, &key, &io).await;
        io.waiting.notified().await;
        if revoke {
            match kind {
                0 => {
                    configured.send_replace(Some(config(false)));
                }
                1 => {
                    actor.send_replace(true);
                }
                2 => {
                    host.send_replace(true);
                }
                _ => unreachable!(),
            }
        }
        io.release.notify_one();
        started.finish(&mut writer, &pool).await.unwrap();
        assert_eq!(io.wire(), if revoke { 1 } else { 2 }, "kind={kind}");
        let expected = if revoke {
            AttemptResult::NotSent
        } else {
            AttemptResult::Created
        };
        assert_eq!(
            results(&pool, &key).await,
            [Some(AttemptResult::Uncertain), Some(expected)]
        );
        assert!(writer.store().ledger().unresolved().is_none());
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act_reload_off_during_429_wait_prevents_retry_pg() {
    withdraw_while_429(0).await;
}

#[tokio::test]
async fn d4act_actor_stop_during_429_wait_prevents_retry_pg() {
    withdraw_while_429(1).await;
}

#[tokio::test]
async fn d4act_host_stop_during_pending_finish_prevents_retry_pg() {
    withdraw_while_429(2).await;
}

#[tokio::test]
async fn d4act_switch_withdrawal_after_first_poll_keeps_started_http_and_slot_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for revoke in [false, true] {
        let key = admitted(&pool, &format!("started-http-{revoke}"), "piece").await;
        let harness = Harness::new();
        harness.gate.acquired();
        let (configured, watched) = watch::channel(Some(config(true)));
        let switch = RepostSwitch::new(None, watched);
        let mut writer = harness.writer();
        writer.set_live(true);
        writer.bind_switch(switch.live());
        let mut runner = Runner::new(switch, pool.clone(), BOT, run_scope());
        let io = Io::over(&harness.port);
        io.hold.store(true, Ordering::SeqCst);
        let started = start(&mut runner, &mut writer, &pool, &key, &io).await;
        io.waiting.notified().await;
        assert_eq!(io.wire(), 1);
        if revoke {
            configured.send_replace(Some(config(false)));
        }
        io.release.notify_one();
        started.finish(&mut writer, &pool).await.unwrap();
        assert_eq!(
            io.wire(),
            1,
            "withdrawal does not undo the first poll's HTTP"
        );
        assert_eq!(
            results(&pool, &key).await,
            [Some(AttemptResult::Uncertain), Some(AttemptResult::Created)]
        );
        assert!(matches!(
            outcome(&mut writer, key.unit().native_key.as_str()),
            Some(PieceOutcome::Posted(_))
        ));
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act_actor_stop_receiver_reaches_a_running_repost_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for revoke in [false, true] {
        let key = admitted(&pool, &format!("actor-wire-{revoke}"), "piece").await;
        let (harness, _, _) = switched_over(b"");
        harness.gate.acquired();
        let io = Io::over(&harness.port);
        io.throttle_first.store(true, Ordering::SeqCst);
        let finished = Arc::new(Notify::new());
        let hook = ActorDispatch {
            pool: pool.clone(),
            key: key.clone(),
            io: Arc::clone(&io),
            finished: Arc::clone(&finished),
            used: false,
        };
        let (stop, task) = spawn(harness.writer().with_repost(Some(Box::new(hook))));
        tokio::time::timeout(Duration::from_secs(60), io.waiting.notified())
            .await
            .expect("the actual actor enters its re-post request");
        if revoke {
            stop.send_replace(true);
        }
        io.release.notify_one();
        if revoke {
            assert!(
                task.await.unwrap().is_none(),
                "the actor stops without a writer halt"
            );
        } else {
            finished.notified().await;
            stop.send(true).unwrap();
            assert!(
                task.await.unwrap().is_none(),
                "the healthy actor stays runnable"
            );
        }
        assert_eq!(io.wire(), if revoke { 1 } else { 2 });
        assert!(harness.port.posts().is_empty());
        assert_eq!(
            results(&pool, &key).await,
            [
                Some(AttemptResult::Uncertain),
                Some(if revoke {
                    AttemptResult::NotSent
                } else {
                    AttemptResult::Created
                })
            ]
        );
    }
    pool.close().await;
    db.drop().await;
}

struct ActorDispatch {
    pool: PgPool,
    key: PieceKey,
    io: Arc<Io>,
    finished: Arc<Notify>,
    used: bool,
}

impl RepostHook<Writer> for ActorDispatch {
    fn turn<'a>(&'a mut self, writer: &'a mut Writer) -> Turned<'a, ()> {
        Box::pin(async move {
            if std::mem::replace(&mut self.used, true) {
                return;
            }
            // Isolate the actor's receiver wiring; the seriality test covers the timed probe.
            let on = switch(true);
            writer.bind_switch(on.live());
            writer.set_live(true);
            let mut runner = Runner::new(on, self.pool.clone(), BOT, run_scope());
            let started = start(&mut runner, writer, &self.pool, &self.key, &self.io).await;
            started.finish(writer, &self.pool).await.unwrap();
            self.finished.notify_one();
        })
    }

    fn original(&mut self, _: u64, _: PieceRecord) -> Turned<'_, OriginalGate> {
        Box::pin(std::future::ready(OriginalGate::Admitted))
    }
}

fn home() -> Arc<HomeGate> {
    let home = Arc::new(HomeGate::new(&CHANNEL.to_string(), "node-a"));
    channel_home::register(Arc::clone(&home));
    home
}

/// Scope is checked again after grant and by each request, without restoring its spent slot.
#[tokio::test]
async fn d4act_scope_revoked_after_grant_and_during_429_wait_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (waiting, revoke) in [(false, false), (false, true), (true, false), (true, true)] {
        let key = admitted(&pool, &format!("scope-{waiting}-{revoke}"), "piece").await;
        let harness = Harness::new();
        harness.gate.acquired();
        let mut writer = harness.writer();
        writer.set_live(true);
        let on = switch(true);
        writer.bind_switch(on.live());
        let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
        let io = Io::over(&harness.port);
        io.throttle_first.store(waiting, Ordering::SeqCst);
        let intent = evidence(&pool, &key).await;
        let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
        let Admission::Granted { permit, lease } = admission.await.unwrap() else {
            panic!("granted")
        };
        let mut registered = None;
        if !waiting && revoke {
            registered = Some(home());
        }
        let started = dispatch::start(&mut writer, permit, lease, &*io).await;
        if waiting {
            io.waiting.notified().await;
            if revoke {
                registered = Some(home());
            }
            io.release.notify_one();
        }
        started.finish(&mut writer, &pool).await.unwrap();
        assert_eq!(
            io.wire(),
            match (waiting, revoke) {
                (false, true) => 0,
                (true, false) => 2,
                _ => 1,
            }
        );
        assert_eq!(
            writer.store().ledger().next_serial(),
            u64::from(waiting || !revoke)
        );
        assert_eq!(
            results(&pool, &key).await.len(),
            2,
            "the granted slot stays spent"
        );
        if let Some(home) = registered {
            assert!(channel_home::unregister_if_same(&home));
        }
    }
    pool.close().await;
    db.drop().await;
}

/// Fresh work whose key was admitted remains suppressed even in an unsupported home.
#[tokio::test]
async fn f6_delegated_admission_requires_sendable_home_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for delegated in [true, false] {
        let harness = Harness::new();
        harness.gate.acquired();
        let io = Io::over(&harness.port);
        let registered = delegated.then(home);
        let mut hook = turn(&harness, pool.clone(), switch(true), &io);
        let mut writer = harness.writer();
        hook.turn(&mut writer).await;
        if delegated {
            assert!(!sidecar(&harness).exists());
            assert_eq!(io.reads.load(Ordering::SeqCst), 0);
        }
        let native = format!("delegated-admitted-{delegated}");
        let key = admitted(&pool, &native, "admitted").await;
        let mut writer = writer.with_repost(Some(Box::new(hook)));
        assert_eq!(
            writer.deliver(&piece(&native, "admitted")).await,
            Step::Done
        );
        assert!(harness.port.posts().is_empty());
        let fresh = format!("delegated-fresh-{delegated}");
        let _ = writer.deliver(&piece(&fresh, "fresh")).await;
        if delegated {
            assert!(harness.port.posts().is_empty());
            assert!(!sidecar(&harness).exists());
            assert!(load(&pool, &key).await.unwrap().is_some());
        } else {
            assert_eq!(harness.port.posts(), ["fresh"]);
            assert!(
                ProvenanceLog::open(&sidecar(&harness))
                    .unwrap()
                    .state()
                    .intent(0)
                    .is_some()
            );
            let started = start(
                &mut Runner::new(switch(true), pool.clone(), BOT, run_scope()),
                &mut writer,
                &pool,
                &key,
                &io,
            )
            .await;
            started.finish(&mut writer, &pool).await.unwrap();
            assert_eq!(io.wire(), 1, "the same unregistered home can dispatch");
        }
        if let Some(home) = registered {
            assert!(channel_home::unregister_if_same(&home));
        }
    }
    pool.close().await;
    db.drop().await;
}

/// Counts the actual writer confirmation reads, rather than the re-post probe reads.
struct ObservedPort {
    port: Arc<FakePort>,
    reads: Arc<AtomicUsize>,
}
impl DiscordPort for ObservedPort {
    fn bot_id(&self) -> u64 {
        BOT
    }
    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        self.port.post(channel, content)
    }
    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.port.history_after(channel, after)
    }
    fn history_readable(&self, channel: u64) -> bool {
        self.port.history_readable(channel)
    }
}

/// Only confirmed non-sends settle directly; unknown results still consult history.
#[tokio::test]
async fn f6_unsent_and_429_do_not_raise_uncertain_alarm_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let cases = [
        Some(WireOutcome::Unsent("building failed".into())),
        Some(WireOutcome::Throttled),
        Some(WireOutcome::Uncertain("lost reply".into())),
        None,
    ];
    for (index, outcome) in cases.into_iter().enumerate() {
        let key = admitted(&pool, &format!("terminal-{index}"), "same payload").await;
        let harness = Harness::new();
        harness.gate.acquired();
        let reads = Arc::new(AtomicUsize::new(0));
        let port = Arc::new(ObservedPort {
            port: Arc::clone(&harness.port),
            reads: Arc::clone(&reads),
        });
        let mut writer = ChannelWriter::new(
            harness.channel(),
            Arc::clone(&harness.gate),
            port,
            Arc::clone(&harness.lease),
            harness.alarms.clone(),
        );
        writer.set_live(true);
        let io = Io::over(&harness.port);
        io.throttle_first.store(index == 1, Ordering::SeqCst);
        *io.outcome.lock().unwrap() = outcome;
        let on = switch(true);
        writer.bind_switch(on.live());
        let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
        let admission = runner.try_next_dispatch(
            &mut writer,
            OriginalGate::Admitted,
            evidence(&pool, &key).await,
        );
        let Admission::Granted { permit, lease } = admission.await.unwrap() else {
            panic!("granted")
        };
        dispatch::start(&mut writer, permit, lease, &*io)
            .await
            .finish(&mut writer, &pool)
            .await
            .unwrap();
        assert_eq!(io.wire(), usize::from(index != 0));
        let ledger = writer.store().ledger();
        assert!(ledger.unresolved().is_none());
        match index {
            0 | 1 => {
                assert_eq!(
                    ledger.piece(0).unwrap().outcome,
                    Some(PieceOutcome::NotSent)
                );
                assert_eq!(ledger.anchor(), 100);
                assert_eq!(reads.load(Ordering::SeqCst), 0);
                assert!(harness.alarms.taken().is_empty());
                assert_eq!(results(&pool, &key).await[1], Some(AttemptResult::NotSent));
            }
            2 => {
                assert_eq!(
                    ledger.piece(0).unwrap().outcome,
                    Some(PieceOutcome::NotFound)
                );
                assert!(reads.load(Ordering::SeqCst) > 0);
                assert_eq!(
                    harness.alarms.taken(),
                    [WriterAlarm::NotFound { serial: 0 }]
                );
            }
            3 => {
                assert_eq!(
                    io.wire(),
                    1,
                    "the identical dispatch harness posts when healthy"
                );
            }
            _ => unreachable!(),
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f6_not_sent_replay_keeps_original_suppressed_and_budget_spent_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for on in [false, true] {
        let harness = Harness::new();
        harness.gate.acquired();
        let io = Io::over(&harness.port);
        let native = format!("replay-{on}");
        let key = admitted(&pool, &native, "same payload").await;
        let mut writer = harness.writer();
        if on {
            writer.set_live(true);
            io.throttle_first.store(true, Ordering::SeqCst);
            *io.outcome.lock().unwrap() = Some(WireOutcome::Throttled);
            let on = switch(true);
            writer.bind_switch(on.live());
            let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
            start(&mut runner, &mut writer, &pool, &key, &io)
                .await
                .finish(&mut writer, &pool)
                .await
                .unwrap();
        } else {
            harness
                .port
                .replies
                .lock()
                .unwrap()
                .push_back(Reply::Unsent);
            let hook = turn(&harness, pool.clone(), switch(false), &io);
            writer = writer.with_repost(Some(Box::new(hook)));
            writer.deliver(&piece(&native, "same payload")).await;
        }
        drop(writer);
        let bytes = std::fs::read_to_string(ledger_path(&harness)).unwrap();
        assert_eq!(
            bytes.matches("\"type\":\"not_sent\"").count(),
            usize::from(on)
        );
        let mut recovered = harness.writer().with_repost(Some(Box::new(turn(
            &harness,
            pool.clone(),
            switch(true),
            &io,
        ))));
        let posted = harness.port.posts().len();
        recovered.deliver(&piece(&native, "same payload")).await;
        assert_eq!(
            harness.port.posts().len(),
            posted,
            "replay does not invent an original"
        );
        assert_eq!(results(&pool, &key).await.len(), if on { 2 } else { 1 });
        if on {
            let landed = harness.port.say(BOT, "same payload").id;
            let later = admitted(&pool, "replay-later", "same payload").await;
            recovered.set_live(true);
            io.throttle_first.store(false, Ordering::SeqCst);
            *io.outcome.lock().unwrap() = Some(WireOutcome::Uncertain("lost reply".into()));
            let on = switch(true);
            recovered.bind_switch(on.live());
            let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
            start(&mut runner, &mut recovered, &pool, &later, &io)
                .await
                .finish(&mut recovered, &pool)
                .await
                .unwrap();
            assert_eq!(
                outcome(&mut recovered, "replay-later"),
                Some(PieceOutcome::Posted(landed)),
                "a confirmed non-send is not an earlier ambiguous candidate"
            );
            assert_eq!(
                io.wire(),
                2,
                "the confirmed 429 and later uncertain dispatch both reach the wire"
            );
        } else {
            assert_eq!(
                harness.port.posts(),
                ["same payload"],
                "off retains its original send"
            );
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f6_herdr_repost_counts_as_inflight_until_request_ends_pg() {
    use crate::services::tui_o::store::fault::{self, Keep, Step as At};
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("node-a"), &[(CHANNEL, "node-a")]);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for case in 0..5 {
        let key = admitted(&pool, &format!("count-{case}"), "piece").await;
        let harness = Harness::new();
        harness.gate.acquired();
        let mut writer = harness.writer();
        writer.set_live(true);
        assert_eq!(
            deliver::posts_in_flight(CHANNEL),
            Some(0),
            "the Herdr constructor installed the actual drain counter"
        );
        let count = writer.repost_posts_counter();
        let on = switch(true);
        writer.bind_switch(on.live());
        let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
        let io = Io::over(&harness.port);
        *io.drop_count.lock().unwrap() = Some(Arc::clone(&count));
        io.throttle_first.store(case == 1, Ordering::SeqCst);
        io.hold.store(case == 2, Ordering::SeqCst);
        let admission = runner.try_next_dispatch(
            &mut writer,
            OriginalGate::Admitted,
            evidence(&pool, &key).await,
        );
        let Admission::Granted { permit, lease } = admission.await.unwrap() else {
            panic!("granted")
        };
        if case == 3 {
            harness.gate.lost();
        }
        let fault = (case == 4).then(|| {
            fault::plant(
                &ledger_path(&harness),
                At::Append(Keep::All),
                std::io::ErrorKind::StorageFull,
                None,
            )
        });
        let started = dispatch::start(&mut writer, permit, lease, &*io).await;
        drop(fault);
        match case {
            1 => {
                io.waiting.notified().await;
                assert_eq!(count.load(Ordering::SeqCst), 1);
                drop((writer, started));
                tokio::task::yield_now().await;
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    1,
                    "the live request outlasts both owners"
                );
                assert_eq!(
                    deliver::posts_in_flight(CHANNEL),
                    Some(1),
                    "the same counter keeps the drain from releasing"
                );
                io.release.notify_one();
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(count.load(Ordering::SeqCst), 0);
                assert_eq!(
                    *io.dropped_count.lock().unwrap(),
                    [1],
                    "the request destructor still sees its count"
                );
                assert_eq!(io.wire(), 1);
                continue;
            }
            2 => {
                io.waiting.notified().await;
                assert_eq!(count.load(Ordering::SeqCst), 1);
                tokio::task::yield_now().await;
                tokio::time::pause();
                tokio::time::advance(POST_TIMEOUT + Duration::from_secs(1)).await;
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    0,
                    "timeout drops the request's count"
                );
                tokio::time::resume();
            }
            _ => {
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    0,
                    "Ready and refusals own no live request"
                );
            }
        }
        started.finish(&mut writer, &pool).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(
            *io.dropped_count.lock().unwrap(),
            if case < 3 { vec![1] } else { Vec::new() },
            "count falls only after the request drops: case={case}"
        );
        assert_eq!(io.wire(), usize::from(case < 3));
    }
    pool.close().await;
    db.drop().await;
}

/// Publish a different verified source while keeping the channel permission explicitly Allowed.
/// This isolates retained-source rejection from the production verified permission hold.
fn revoke_retained_source(root: &std::path::Path) {
    use crate::services::tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{
            BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope,
            ObservedHookProcess, PreparedIncarnation,
        },
    };
    let tmux = format!("d4act-source-{}", uuid::Uuid::new_v4().simple());
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: tmux.clone(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: Some("verified".into()),
    };
    PreparedIncarnation::create(context.clone()).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &context.execution_nonce).unwrap();
    dedupe::register_tmux_channel(&tmux, CHANNEL);
    let id = "019e660d-4859-7522-9cee-8ba7c4e7c743";
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{id}.jsonl"));
    let timestamp = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let header = serde_json::json!({"type":"session_meta", "timestamp":timestamp,
        "payload":{"id":id,"timestamp":timestamp,"source":"cli","cwd":root,
            "originator":"codex_cli_rs"}});
    std::fs::write(&path, format!("{header}\n")).unwrap();
    dedupe::set_codex_delivery_permission_for_tests(
        &context,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let ingress =
        crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook(
            "codex",
            "session_start",
            Some(id),
            Some(id),
            &serde_json::json!({"session_id":id,"transcript_path":path,"source":"startup"}),
            &headers,
        );
    assert!(
        dedupe::runtime_binding_for_tmux_session(&tmux).is_some(),
        "a real Allowed source was published: {ingress:?}"
    );
    assert!(
        dedupe::codex_verified_channel_delivery_allowed(CHANNEL),
        "channel policy stays Allowed"
    );
}

async fn source_revocation(stage: u8) {
    use crate::services::tui_prompt_dedupe as dedupe;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    // Each binding case runs alone in a child process; no dedupe mutex spans PG awaits.
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for revoke in [false, true] {
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(Some(root.path()));
        let key = admitted(&pool, &format!("source-{stage}-{revoke}"), "piece").await;
        let native = "019e660d-4859-7522-9cee-8ba7c4e7c743";
        let primary_path = root
            .path()
            .canonicalize()
            .unwrap()
            .join(format!("rollout-{native}.jsonl"));
        std::fs::write(&primary_path, b"").unwrap();
        let primary = source_id_for(native, &primary_path).unwrap();
        let mut retained = None;
        let harness = Harness::build(|runtime| {
            let path = runtime.join("old.jsonl");
            std::fs::write(&path, b"").unwrap();
            let other = source_id_for("s1", &path).unwrap();
            retained = Some(other.clone());
            let prefix_hash = hex::encode(sha2::Sha256::digest(b""));
            vec![
                InitSource {
                    source_id: primary.clone(),
                    delivery_start: 0,
                    prefix_hash: prefix_hash.clone(),
                },
                InitSource {
                    source_id: other,
                    delivery_start: 0,
                    prefix_hash,
                },
            ]
        });
        let retained = retained.unwrap();
        harness.gate.acquired();
        let mut writer = harness.writer();
        writer.set_live(true);
        assert!(
            writer.delivery_allowed(false),
            "the retained source begins Legacy"
        );
        let on = switch(true);
        writer.bind_switch(on.live());
        let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
        let io = Io::over(&harness.port);
        io.throttle_first.store(stage == 2, Ordering::SeqCst);
        let intent = evidence(&pool, &key).await;
        if stage == 0 && revoke {
            revoke_retained_source(root.path());
        }
        let admission = runner
            .try_next_dispatch(&mut writer, OriginalGate::Admitted, intent)
            .await
            .unwrap();
        if stage == 0 && revoke {
            assert!(matches!(admission, Admission::GuardRejected(_)));
            assert_eq!(results(&pool, &key).await.len(), 1);
        } else {
            let Admission::Granted { permit, lease } = admission else {
                panic!("healthy source grants")
            };
            if stage == 1 && revoke {
                revoke_retained_source(root.path());
            }
            let started = dispatch::start(&mut writer, permit, lease, &*io).await;
            if stage == 2 {
                io.waiting.notified().await;
                if revoke {
                    revoke_retained_source(root.path());
                }
                io.release.notify_one();
            }
            started.finish(&mut writer, &pool).await.unwrap();
            assert_eq!(
                results(&pool, &key).await.len(),
                2,
                "a consumed grant is preserved"
            );
        }
        assert_eq!(
            io.wire(),
            match (stage, revoke) {
                (0 | 1, true) => 0,
                (2, false) => 2,
                _ => 1,
            }
        );
        assert_eq!(
            writer.store().ledger().next_serial(),
            u64::from(stage == 2 || !revoke)
        );
        if revoke {
            assert!(
                dedupe::codex_verified_o_source_allowed(CHANNEL, &primary),
                "the current retained source is allowed"
            );
            assert!(
                !dedupe::codex_verified_o_source_allowed(CHANNEL, &retained),
                "a different retained source is revoked"
            );
            assert!(!writer.delivery_allowed(false));
        }
    }
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act_source_revoked_after_tick_never_grants_pg() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::d4act_source_revoked_after_tick_never_grants_pg"
    )) {
        return;
    }
    source_revocation(0).await;
}

#[tokio::test]
async fn d4act_source_revoked_after_grant_never_wires_pg() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::d4act_source_revoked_after_grant_never_wires_pg"
    )) {
        return;
    }
    source_revocation(1).await;
}

#[tokio::test]
async fn d4act_source_revoked_during_429_wait_prevents_retry_pg() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::d4act_source_revoked_during_429_wait_prevents_retry_pg"
    )) {
        return;
    }
    source_revocation(2).await;
}

#[tokio::test]
async fn d4act_unbound_writer_blocks_grant_prepared_and_wire_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "unbound-switch", "piece").await;
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    writer.set_live(true);
    let on = switch(true);
    let effective = on.live();
    let mut runner = Runner::new(on, pool.clone(), BOT, run_scope());
    let io = Io::over(&harness.port);
    let admission = runner
        .try_next_dispatch(
            &mut writer,
            OriginalGate::Admitted,
            evidence(&pool, &key).await,
        )
        .await
        .unwrap();
    assert!(
        matches!(
            admission,
            Admission::GuardRejected(dispatch::GuardReason::Writer(_))
        ),
        "an unbound writer cannot consume a grant"
    );
    assert_eq!(results(&pool, &key).await, [Some(AttemptResult::Uncertain)]);
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert_eq!(
        std::fs::read_to_string(ledger_path(&harness))
            .unwrap()
            .matches("\"type\":\"prepared\"")
            .count(),
        0,
        "an unbound writer appends no Prepared"
    );
    assert_eq!(io.wire(), 0);
    writer.bind_switch(effective);
    start(&mut runner, &mut writer, &pool, &key, &io)
        .await
        .finish(&mut writer, &pool)
        .await
        .unwrap();
    assert_eq!(io.wire(), 1, "the same writer dispatches when bound ON");
    assert_eq!(writer.store().ledger().next_serial(), 1);
    let bytes = std::fs::read_to_string(ledger_path(&harness)).unwrap();
    assert_eq!(bytes.matches("\"type\":\"prepared\"").count(), 1);
    assert!(matches!(
        outcome(&mut writer, "unbound-switch"),
        Some(PieceOutcome::Posted(_))
    ));
    assert_eq!(
        results(&pool, &key).await,
        [Some(AttemptResult::Uncertain), Some(AttemptResult::Created)]
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act_channel_turn_binds_switch_before_real_repost_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "channel-turn-binding", "absent piece").await;
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    // A real Posted and the same bot's history give both probe passes permission evidence.
    assert_eq!(
        writer
            .deliver(&piece("binding-proof", "permission proof"))
            .await,
        Step::Done
    );
    let io = Io::over(&harness.port);
    let mut hook = turn(&harness, pool.clone(), switch(true), &io);
    hook.turn(&mut writer).await;
    assert!(
        writer.guard().is_ok(),
        "ChannelTurn binds the writer before admission"
    );
    assert_eq!(io.wire(), 0);
    tokio::time::sleep(FIRST_PASS_AFTER).await;
    hook.turn(&mut writer).await;
    assert!(io.reads.load(Ordering::SeqCst) > 0);
    assert_eq!(io.wire(), 0, "one pass grants nothing");
    assert_eq!(results(&pool, &key).await, [Some(AttemptResult::Uncertain)]);
    tokio::time::sleep(SECOND_PASS_AFTER).await;
    hook.turn(&mut writer).await;
    assert_eq!(
        io.wire(),
        1,
        "the actual ChannelTurn dispatches after two passes"
    );
    assert_eq!(harness.port.posts(), ["permission proof"]);
    assert_eq!(writer.store().ledger().next_serial(), 2);
    assert!(matches!(
        outcome(&mut writer, "channel-turn-binding"),
        Some(PieceOutcome::Posted(_))
    ));
    assert_eq!(
        results(&pool, &key).await,
        [Some(AttemptResult::Uncertain), Some(AttemptResult::Created)]
    );
    assert!(writer.store().ledger().unresolved().is_none());
    pool.close().await;
    db.drop().await;
}
