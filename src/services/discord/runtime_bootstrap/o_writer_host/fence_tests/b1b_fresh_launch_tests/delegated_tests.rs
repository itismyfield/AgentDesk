//! The actual home-routed worker launches Codex before its delegated writer restores.

use super::*;
use crate::db::o_channel_homes::{self, HomeError, HomeState, HomeWrite};
use crate::services::cluster::channel_home::{self, HomeGate, HomeIntake, HomeOwnership};
use crate::services::cluster::intake_worker::{self, TickOutcome, test_executor};
use crate::services::turn_orchestrator::{QueueIdentity, QueuePersistenceContext};
use std::sync::atomic::AtomicBool;

const HOLDER: &str = "b1-absent-test-host";
const GATEWAY: &str = "b1b-other-gateway";

struct Home(Arc<HomeGate>);
impl Drop for Home {
    fn drop(&mut self) {
        channel_home::unregister_if_same(&self.0);
    }
}

fn applied<T>(write: Result<HomeWrite<T>, HomeError>) -> T {
    match write.unwrap() {
        HomeWrite::Applied(row) => row,
        HomeWrite::Stale => panic!("the fixture's exact home transition must apply"),
    }
}

async fn renew(pool: &sqlx::PgPool, home: &HomeGate, epoch: i64) {
    let sent = tokio::time::Instant::now();
    let held =
        applied(o_channel_homes::renew(pool, &CANARY_CHANNEL.to_string(), HOLDER, epoch).await);
    assert!(
        matches!(home.confirm(&held, sent).unwrap(), HomeOwnership::Owned {
        home_epoch, intake: HomeIntake::Open, ..
    } if home_epoch == epoch)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn delegated_home_actual_worker_launches_fresh_and_writer_posts_fifo_pg() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        module_path!(),
        "::delegated_home_actual_worker_launches_fresh_and_writer_posts_fifo_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some(HOLDER)).await;
    let api = b1::recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let _mode = fixture.scoped_test_env("ADK_INTAKE_ROUTING_MODE", "enforce".as_ref());
    let _writer = fixture.scoped_test_env(test_override::CHILD_ENV, "1".as_ref());
    let channels = serde_json::to_string(&[(CANARY_CHANNEL, CodexTui)]).unwrap();
    let _channels = fixture.scoped_test_env(test_override::CHANNELS_ENV, channels.as_ref());
    let _selected = test_override::force_channels(&[(CANARY_CHANNEL, CodexTui)]);
    let mut config = (*crate::config_live_reload::current().unwrap()).clone();
    config.cluster.enabled = true;
    config.cluster.instance_id = Some(HOLDER.into());
    config.cluster.gateway_preferred_instance_id = Some(GATEWAY.into());
    config.runtime.channel_home_delegation_enabled = Some(true);
    crate::config_live_reload::install(config);
    b1::target_missing(&fixture, true);
    crate::services::platform::tmux::invalidate_availability_cache();
    assert!(!dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));

    let pool = fixture.shared.pg_pool.as_ref().unwrap();
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('b1b-home', 'B1b home', 'codex', $1)")
        .bind(CANARY_CHANNEL.to_string()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO worker_nodes (instance_id, status, role, effective_role, labels, capabilities, last_heartbeat_at, started_at, updated_at) VALUES ($1, 'online', 'worker', 'worker', '[]', $2, NOW(), NOW(), NOW())")
        .bind(HOLDER).bind(json!({"intake_worker":{"enabled":true,"providers":["codex"],"features":["preserve_on_cancel_v1"]}}))
        .execute(pool).await.unwrap();
    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &fixture.shared.token_hash,
        &ProviderKind::Codex,
        CANARY_TMUX,
    );
    assert_eq!(sqlx::query("UPDATE sessions SET instance_id=$1 WHERE session_key=$2 AND provider='codex' AND channel_id=$3")
        .bind(HOLDER).bind(session_key).bind(CANARY_CHANNEL.to_string()).execute(pool).await.unwrap().rows_affected(), 1);
    let row = applied(
        o_channel_homes::delegate(pool, &CANARY_CHANNEL.to_string(), "codex", GATEWAY, HOLDER)
            .await,
    );
    let row = applied(
        o_channel_homes::finish_release(pool, &CANARY_CHANNEL.to_string(), GATEWAY, row.epoch)
            .await,
    );
    let row =
        applied(o_channel_homes::adopt(pool, &CANARY_CHANNEL.to_string(), HOLDER, row.epoch).await);
    let epoch = row.epoch;
    assert_eq!(row.state, HomeState::Worker);
    assert_eq!(row.holder.as_deref(), Some(HOLDER));
    let home = Home(Arc::new(HomeGate::new(&CANARY_CHANNEL.to_string(), HOLDER)));
    channel_home::register(Arc::clone(&home.0));
    renew(pool, &home.0, epoch).await;

    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[CANARY_CHANNEL], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: Vec::new(),
                initial_anchor: 0,
                build_digest: "b1b-delegated-worker".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let mut old_store = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    let old_cursor = old_store.attach_source(&fixture.old_source).unwrap();
    let proof = dedupe::binding_events::codex::read_ownership(&fixture.context)
        .unwrap()
        .verified
        .unwrap();
    old_store.set_binding_checkpoint(proof.seq).unwrap();
    drop(old_store);
    let old_bytes = fs::read(&fixture.old_source.path).unwrap();
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(
        &fixture.old_source.path,
    );
    let output = Discord::start().await;
    let _rest = crate::services::discord::shared_state::test_rest::install(Arc::clone(&api.http));
    fixture
        .shared
        .http
        .cached_bot_token
        .set("e51-isolated-test-bot".into())
        .unwrap();
    fixture
        .shared
        .settings
        .write()
        .await
        .allowed_channel_ids
        .push(CANARY_CHANNEL);
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    registry
        .register("codex".into(), Arc::clone(&fixture.shared))
        .await;
    let mut claimant = crate::services::discord::make_shared_data_for_tests();
    {
        let shared = Arc::get_mut(&mut claimant).unwrap();
        shared.provider = ProviderKind::Codex;
        shared.health_registry = Arc::downgrade(&registry);
    }
    let actual = crate::services::discord::health::resolve_intake_worker_runtime(
        &claimant,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&actual.shared, &fixture.shared));
    assert!(Arc::ptr_eq(&actual.http, &api.http));
    let mut prepared = parts(
        &fixture.shared,
        &ProviderKind::Codex,
        None,
        Some(Arc::clone(&output.http)),
    );
    Arc::get_mut(&mut prepared.io).unwrap().configured_id = Some(HOLDER.into());
    let readiness = host::process_readiness();
    let delegated =
        crate::services::cluster::channel_home_boot::delegated_boot_ownership(|channel| {
            channel == CANARY_CHANNEL
        });
    assert_eq!(delegated.len(), 1);
    let mut handles = host::start_managed(ShadowProvider::Codex, false, delegated, || prepared);
    assert_eq!(
        handles.len(),
        1,
        "the real home must host with pg_gateway=false"
    );
    let run = std::panic::AssertUnwindSafe(async {
        eventually(|| readiness.submission(CANARY_CHANNEL).is_some(), "delegated false-resumed actor").await;
        assert!(!readiness.accepts(CANARY_CHANNEL));
        let deps = IntakeDeps { http: &api.http, cache: None, ctx_for_chained_dispatch: None,
            shared: &fixture.shared, token: "e51-isolated-test-bot" };
        let admission = admit_text_intake(&deps, &input()).await;
        let (target_instance_id, outbox_id) = admission.forwarded_for_tests()
            .unwrap_or_else(|| panic!("the actual home route must retain Q1 in its worker delivery: {admission:?}"));
        assert_eq!(target_instance_id, HOLDER);
        let routed: (String, Option<i64>, String) = sqlx::query_as("SELECT target_instance_id, home_epoch, user_text FROM intake_outbox WHERE id=$1")
            .bind(outbox_id).fetch_one(pool).await.unwrap();
        assert_eq!(routed, (HOLDER.into(), Some(epoch), b1::Q1.into()));
        fixture.fake.take_calls();
        let (_observe, observed) = observe_launch();
        let done = Arc::new(AtomicBool::new(false));
        let worker_done = Arc::clone(&done);
        let checkpoints = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&checkpoints);
        let _checkpoints = test_executor::hook(Box::new(move |point| {
            seen.lock().unwrap().push(point);
            Box::pin(async {})
        }));
        let worker = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
            let mut ticks = Vec::new();
            while !worker_done.load(Ordering::SeqCst) {
                assert!(tokio::time::Instant::now() < deadline, "actual delegated worker did not finish its FIFO");
                renew(pool, &home.0, epoch).await;
                let tick = intake_worker::run_intake_worker_tick(pool, &claimant, HOLDER, "codex", "b1b-home-claim", &|| false).await.unwrap();
                ticks.push(tick);
                let failed: Vec<(String, Option<String>)> = sqlx::query_as(
                    "SELECT status, last_error FROM intake_outbox WHERE channel_id=$1 AND status IN ('failed_pre_accept', 'failed_post_accept')",
                ).bind(CANARY_CHANNEL.to_string()).fetch_all(pool).await.unwrap();
                assert!(failed.is_empty(), "the actual delegated executor failed: {failed:?}");
                let snapshot = fixture.shared.mailbox(serenity::ChannelId::new(CANARY_CHANNEL)).snapshot().await;
                if snapshot.cancel_token.is_none() && !snapshot.intervention_queue.is_empty() {
                    b1::kickoff(&fixture, &api).await;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            ticks
        };
        let drive = drive_fifo(&fixture, &output, &readiness, &old_io, done, &observed);
        let (ticks, ()) = tokio::join!(worker, drive);
        assert!(ticks.iter().any(|tick| *tick == TickOutcome::Processed));
        for point in [test_executor::Checkpoint::BeforeFinalO, test_executor::Checkpoint::PreAccept,
            test_executor::Checkpoint::Spawned, test_executor::Checkpoint::FinalDbDone] {
            assert!(checkpoints.lock().unwrap().contains(&point), "missing actual worker checkpoint {point:?}");
        }
        let rows: Vec<(Option<i64>, bool, bool, String)> = sqlx::query_as(
            "SELECT home_epoch, accepted_at IS NOT NULL, spawned_at IS NOT NULL, user_text FROM intake_outbox WHERE channel_id=$1 ORDER BY id",
        ).bind(CANARY_CHANNEL.to_string()).fetch_all(pool).await.unwrap();
        assert_eq!(rows, [(Some(epoch), true, true, b1::Q1.into()), (Some(epoch), true, true, b1::Q2.into())]);
    }).catch_unwind().await;
    if let Some(token) = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
    {
        token.cancelled.store(true, Ordering::SeqCst);
        b1::wait_idle(&fixture).await;
    }
    let stopped = tokio::time::timeout(
        Duration::from_secs(20),
        handles.pop().unwrap().stop_and_join(),
    )
    .await;
    if let Err(panic) = run {
        std::panic::resume_unwind(panic);
    }
    stopped.expect("the delegated writer must stop").unwrap();
    let recovered = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    assert_eq!(recovered.cursor(&fixture.old_source), Some(&old_cursor));
    assert_eq!(fs::read(&fixture.old_source.path).unwrap(), old_bytes);
    assert_eq!(old_io.stats(), Default::default());
    assert_eq!(
        output
            .texts()
            .iter()
            .filter(|text| text.contains(FIRST_REPLY))
            .count(),
        1
    );
    assert_eq!(
        output
            .texts()
            .iter()
            .filter(|text| text.contains(SECOND_REPLY))
            .count(),
        1
    );
    assert!(recovered.ledger().unresolved().is_none());
}

async fn drive_fifo(
    fixture: &CanaryFixture,
    output: &Discord,
    readiness: &Arc<host::Readiness>,
    old_io: &crate::services::codex::actual_submit_tests::source_io::Watch,
    done: Arc<AtomicBool>,
    observed: &mpsc::Receiver<Arc<crate::services::provider::CancelToken>>,
) {
    let first_token = receive(observed, "worker Q1 actual provider").await;
    let first = fixture.turn_failure_guard().await;
    let fresh = b1::wait_context_after(fixture, &fixture.context.execution_nonce).await;
    assert_eq!(fresh.launch_mode, "fresh");
    assert_eq!(fresh.source_policy.as_deref(), Some("verified"));
    assert!(fresh.expected_native_session_id.is_none());
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert_eq!(argv.iter().filter(|arg| arg.contains(b1::Q1)).count(), 1);
    assert!(
        argv.iter()
            .all(|arg| !arg.contains(b1::Q2) && arg != "resume")
    );
    let pending = b1::instruction(CANARY_CHANNEL + 102, b1::Q2);
    fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .replace_queue(
            vec![pending],
            QueuePersistenceContext::new(&ProviderKind::Codex, &fixture.shared.token_hash, None),
        )
        .await;
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .unwrap();
    assert_eq!(disk.len(), 1);
    assert_eq!(
        b1::queue_identities(fixture).await,
        disk.iter().map(QueueIdentity::of).collect::<Vec<_>>()
    );
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let input_calls = Arc::new(Mutex::new(Vec::new()));
    let (_warm, entered, release, boundary) =
        native_output::warm_probe(&fresh, Arc::clone(&accepted), Arc::clone(&input_calls));
    let path = b1::publish_named_source(fixture, &fresh, NEW_NATIVE_ID);
    let new_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&path);
    native_output::terminal(&path, "new-clear-turn", FIRST_REPLY);
    b1::wait_completion(&first_token).await;
    eventually(
        || readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL),
        "delegated scoped FullResume",
    )
    .await;
    eventually(
        || output.texts().iter().any(|text| text.contains(FIRST_REPLY)),
        "delegated O Q1 POST",
    )
    .await;
    first.finish();
    crate::services::discord::turn_presence::activity::tests::companion_attach_log_watcher(
        &fixture.shared,
        CANARY_CHANNEL,
        CANARY_TMUX,
        &path,
    );
    let activity =
        crate::services::discord::turn_presence::activity::tests::companion_settled_activity(
            &fixture.shared,
            CANARY_CHANNEL,
        )
        .await;
    assert_eq!(
        activity.activity,
        crate::services::discord::turn_presence::activity::Activity::Idle
    );
    let prompt = receive(&entered, "delegated Q2 actual warm worker").await;
    assert!(prompt.contains(b1::Q2) && !prompt.contains(b1::Q1));
    let second = fixture.turn_failure_guard().await;
    let second_token = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
        .unwrap();
    assert!(!Arc::ptr_eq(&first_token, &second_token));
    release.send(()).unwrap();
    eventually(
        || {
            if let Ok(stage) = boundary.try_recv() {
                panic!("actual delegated warm provider held: {stage}");
            }
            accepted.lock().unwrap().len() == 1
        },
        "actual delegated warm Enter",
    )
    .await;
    b1::wait_completion(&second_token).await;
    b1::wait_idle(fixture).await;
    eventually(
        || {
            output
                .texts()
                .iter()
                .any(|text| text.contains(SECOND_REPLY))
        },
        "delegated O Q2 POST",
    )
    .await;
    second.finish();
    assert!(b1::queue_identities(fixture).await.is_empty());
    assert_eq!(
        input_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "keys:Enter")
            .count(),
        1
    );
    let calls = fixture.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("kill-session")),
        "{calls:?}"
    );
    assert_eq!(old_io.stats(), Default::default());
    assert!(new_io.stats().read_bytes > 0);
    done.store(true, Ordering::SeqCst);
}

fn observe_launch() -> (
    crate::services::discord::codex_absence::test_setup::ResetGuard,
    mpsc::Receiver<Arc<crate::services::provider::CancelToken>>,
) {
    let (observed_tx, observed_rx) = mpsc::channel();
    let guard = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            crate::services::codex::actual_submit_tests::observe_absence_admission(Box::new(
                move |observation, token| {
                    let token = token.expect("actual delegated worker provider token");
                    let observed = observation.expect("the worker launch must carry ExactSelf");
                    assert!(
                        observed.matches_token(token)
                            && observed.recheck(CANARY_CHANNEL, CANARY_TMUX)
                    );
                    observed_tx.send(Arc::clone(token)).unwrap();
                },
            ));
            Box::new(())
        }),
    );
    (guard, observed_rx)
}
