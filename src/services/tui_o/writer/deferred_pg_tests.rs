//! Actual Deferred adoption holds intake writes and waits for durable Legacy body delivery.

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{FailedPreAcceptSweepOutcome, sweep_failed_pre_accept_once};
use crate::db::o_channel_activation::{activation_rows, fenced_activation_rows};
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::health::HealthRegistry;
use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg_returning_id_with_ttl};
use crate::services::tui_o::writer::host::FencedFacts;
use sqlx::PgPool;

/// The real DB fence over the existing channel host fixture, including every observed blocker.
struct PgIo {
    base: Arc<TestIo>,
    pool: PgPool,
    fenced: Mutex<Vec<(i64, i64)>>,
}

impl HostIo for PgIo {
    type Port = FakePort;
    type Lease = Arc<FakeLease>;
    type Alarms = Raised;
    type Bindings = ChannelBindingLog;

    fn port(&self) -> impl Future<Output = Arc<FakePort>> + Send {
        self.base.port()
    }

    fn lease(&self) -> Self::Lease {
        self.base.lease()
    }

    fn alarms(&self) -> Raised {
        self.base.alarms()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        self.base.bindings(channel, provider)
    }

    async fn activation_facts(
        &self,
        channel: u64,
        _: ShadowProvider,
    ) -> Result<ActivationFacts, String> {
        let rows = activation_rows(&self.pool, &channel.to_string(), "gateway")
            .await
            .map_err(|error| error.to_string())?;
        Ok(ActivationFacts {
            open_intake: rows.open_intake,
            runner_sessions: rows.foreign_sessions,
            node_override: None,
        })
    }

    async fn intake_fence(&self, channel: u64, _: ShadowProvider) -> Result<FencedFacts, String> {
        let (hold, rows, queued_bodies) =
            fenced_activation_rows(&self.pool, &channel.to_string(), "gateway")
                .await
                .map_err(|error| error.to_string())?;
        self.fenced
            .lock()
            .unwrap()
            .push((rows.open_intake, queued_bodies));
        Ok(FencedFacts {
            hold,
            facts: ActivationFacts {
                open_intake: rows.open_intake,
                runner_sessions: rows.foreign_sessions,
                node_override: None,
            },
            queued_bodies,
        })
    }

    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.base.local_custody(channel, provider)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.base.legacy()
    }

    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.base.legacy_busy(channel)
    }

    fn relaying(&self, channel: u64) -> bool {
        self.base.relaying(channel)
    }

    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        self.base.adopted(channel, provider);
    }
}

/// Keeps SQL and blocking transcript reads from implicitly advancing the paused clock.
struct Clock(tokio::task::JoinHandle<()>);

impl Clock {
    fn paused() -> Self {
        tokio::time::pause();
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }

    async fn advance(&self, by: Duration) {
        // SQL setup pauses at a fractional timer tick; cross Tokio's rounded sleep deadline.
        let by = by + Duration::from_millis(1);
        if adoption(CHANNEL) != Adoption::Deferred {
            tokio::time::advance(by).await;
            wall_yield(Duration::from_millis(40)).await;
            return;
        }
        let completed = hook_reached(Step::DeferredTick, || {});
        tokio::time::advance(by).await;
        wait_until("real Deferred iteration finished", || {
            completed.load(Ordering::SeqCst) || adoption(CHANNEL) != Adoption::Deferred
        })
        .await;
    }

    async fn tick(&self) {
        self.advance(Duration::from_secs(5)).await;
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.0.abort();
        tokio::time::resume();
    }
}

async fn wall_yield(by: Duration) {
    let since = std::time::Instant::now();
    while since.elapsed() < by {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

async fn wait_until(why: &str, done: impl Fn() -> bool) {
    let since = std::time::Instant::now();
    while !done() {
        assert!(since.elapsed() < Duration::from_secs(10), "{why}");
        wall_yield(Duration::from_millis(1)).await;
    }
}

struct PgCase {
    open: Open,
    io: Arc<PgIo>,
    expired: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl PgCase {
    fn new(pool: PgPool, expired: bool) -> Self {
        let open = Open::new();
        if expired {
            open.close();
            open.legacy.frontier.store(0, Ordering::SeqCst);
            *open.io.custody.lock().unwrap() = Ok(Custody::Row);
        }
        let io = Arc::new(PgIo {
            base: Arc::clone(&open.io),
            pool,
            fenced: Mutex::default(),
        });
        Self {
            open,
            io,
            expired,
            tasks: Vec::new(),
        }
    }

    async fn start(&mut self) {
        let reached = hook_reached(Step::DeferredStarted, || {});
        p5::set_test_root(Some(self.open.harness._runtime.path()));
        let parts = || HostParts {
            io: Arc::clone(&self.io),
            runtime_root: Some(root(&self.open.harness)),
            gate: Arc::clone(&self.open.harness.gate),
            readiness: Arc::clone(&self.open.ready),
        };
        self.tasks = start(ShadowProvider::Claude, true, parts);
        wait_until("actual Deferred retry entered", || {
            reached.load(Ordering::SeqCst)
        })
        .await;
        self.assert_waiting();
    }

    async fn eligible(&self, clock: &Clock) {
        if !self.expired {
            self.open.close();
        }
        for _ in 0..4 {
            clock.tick().await;
        }
        if self.expired {
            clock.advance(Duration::from_secs(40 * 60)).await;
        }
    }

    fn assert_waiting(&self) {
        assert_eq!(adoption(CHANNEL), Adoption::Deferred);
        assert!(!self.open.harness.store.has_channel_dir(CHANNEL));
        assert!(!self.open.ready.is_ready(CHANNEL));
        assert_eq!(self.open.alarms(), []);
    }

    fn init_path(&self) -> PathBuf {
        init_path(&self.open.harness, CHANNEL)
    }

    fn samples(&self) -> Vec<(i64, i64)> {
        self.io.fenced.lock().unwrap().clone()
    }

    async fn adopted(&self, clock: &Clock) {
        // Readiness follows fence rollback and handoff alarms; Committed alone is earlier.
        wait_until("actual Deferred host published readiness", || {
            adoption(CHANNEL) == Adoption::Committed && self.open.ready.is_ready(CHANNEL)
        })
        .await;
        let init = self.open.harness.store.read_init(CHANNEL).unwrap().unwrap();
        let starts: Vec<_> = init
            .sources
            .iter()
            .map(|source| source.delivery_start)
            .collect();
        assert_eq!(starts, [self.open.len()]);
        assert!(
            self.samples().contains(&(0, 0)),
            "fresh fence allowed commit"
        );
        let abandoned = self
            .open
            .alarms()
            .iter()
            .filter(|(_, alarm)| matches!(alarm, WriterAlarm::Abandoned { .. }))
            .count();
        assert_eq!(abandoned, usize::from(self.expired));
        append(&self.open.path, &row("pg-next", "next"));
        for _ in 0..3 {
            clock.tick().await;
        }
        assert!(self.open.ready.is_ready(CHANNEL));
        assert_eq!(self.open.harness.port.posts(), ["next"]);
    }

    async fn stop(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

async fn retryable_parent(pool: &PgPool) -> i64 {
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, labels, capabilities, last_heartbeat_at)
         VALUES ('worker', 'online', '[]',
            '{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"]}}', NOW())",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status,
            admission_kind, completed_at
         ) VALUES ('worker', 'leader', $1, 'deferred-fence-parent', 'user', 'hi',
            'standard', 'agent', 'claude', 'failed_pre_accept', 'local', NOW()) RETURNING id",
    )
    .bind(CHANNEL.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn independent_pool(url: &str) -> PgPool {
    crate::db::postgres::connect_test_pool_with_max_connections(url, "Deferred fence tests", 1)
        .await
        .unwrap()
}

async fn child_count(pool: &PgPool, parent: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id = $1")
        .bind(parent)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn sweep_before(url: String, parent: i64) {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let pool = independent_pool(&url).await;
                let outcome = sweep_failed_pre_accept_once(&pool, "leader", 5, 60, None)
                    .await
                    .unwrap();
                assert!(matches!(outcome,
                    FailedPreAcceptSweepOutcome::Retried { source_id, .. } if source_id == parent));
                assert_eq!(child_count(&pool, parent).await, 1);
                pool.close().await;
            });
    })
    .join()
    .unwrap();
}

struct Sweep {
    task: std::thread::JoinHandle<bool>,
    backend: i32,
}

type SweepSlot = Arc<Mutex<Option<Sweep>>>;

/// Reports the actual sweep backend's lock and init ordering from its independent runtime.
fn sweep_behind_fence(url: String, parent: i64, init: PathBuf, slot: SweepSlot) {
    let (proof_tx, proof_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let observer = independent_pool(&url).await;
                let sweep_pool = independent_pool(&url).await;
                let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&sweep_pool)
                    .await
                    .unwrap();
                let actual_pool = sweep_pool.clone();
                let sweep = tokio::spawn(async move {
                    let actual: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(&actual_pool)
                        .await
                        .unwrap();
                    assert_eq!(actual, backend, "actual sweep uses its independent backend");
                    sweep_failed_pre_accept_once(&actual_pool, "leader", 5, 60, None).await
                });
                let since = std::time::Instant::now();
                let mut lock_wait = false;
                while since.elapsed() < Duration::from_secs(2) && !sweep.is_finished() {
                    lock_wait = sqlx::query_scalar(
                        "SELECT EXISTS (
                            SELECT 1 FROM pg_locks
                             WHERE pid = $1 AND relation = 'intake_outbox'::regclass
                               AND mode = 'RowExclusiveLock' AND NOT granted)",
                    )
                    .bind(backend)
                    .fetch_one(&observer)
                    .await
                    .unwrap();
                    if lock_wait {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                let unfinished = !sweep.is_finished();
                let children = child_count(&observer, parent).await;
                let init_absent = !init.exists();
                proof_tx
                    .send((backend, lock_wait, unfinished, children, init_absent))
                    .unwrap();
                let outcome = tokio::time::timeout(Duration::from_secs(10), sweep)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert!(matches!(outcome,
                    FailedPreAcceptSweepOutcome::Retried { source_id, .. } if source_id == parent));
                assert_eq!(child_count(&observer, parent).await, 1);
                let init_before_child = init.exists();
                sweep_pool.close().await;
                observer.close().await;
                init_before_child
            })
    });
    let (backend, blocked, unfinished, children, init_absent) =
        proof_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    *slot.lock().unwrap() = Some(Sweep {
        task: thread,
        backend,
    });
    assert!(
        blocked,
        "actual sweep backend waits on intake RowExclusiveLock"
    );
    assert!(unfinished, "actual sweep remains unfinished for 300ms");
    assert_eq!(children, 0, "no child commit while fence is held");
    assert!(init_absent, "hook runs before init publication");
}

async fn sweep_first(expired: bool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let parent = retryable_parent(&pool).await;
    let mut case = PgCase::new(pool.clone(), expired);
    let clock = Clock::paused();
    case.start().await;
    assert_eq!(
        activation_rows(&pool, &CHANNEL.to_string(), "gateway")
            .await
            .unwrap()
            .open_intake,
        0
    );
    let url = db.database_url.clone();
    let reached = hook_reached(Step::BeforeFence, move || sweep_before(url, parent));
    case.eligible(&clock).await;
    wait_until("BeforeFence actual sweep finished", || {
        reached.load(Ordering::SeqCst)
    })
    .await;
    wait_until("fresh fence observed the committed retry child", || {
        !case.samples().is_empty()
    })
    .await;
    assert!(
        case.samples().iter().all(|sample| *sample == (1, 0)),
        "fresh facts must replace pre-fence facts: {:?}",
        case.samples()
    );
    assert_eq!(child_count(&pool, parent).await, 1);
    case.assert_waiting();
    case.stop().await;
    drop(clock);
    pool.close().await;
    db.drop().await;
}

async fn fence_first(expired: bool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let parent = retryable_parent(&pool).await;
    let mut case = PgCase::new(pool.clone(), expired);
    let clock = Clock::paused();
    case.start().await;
    let url = db.database_url.clone();
    let at_write_url = url.clone();
    let init = case.init_path();
    let at_write_init = init.clone();
    let sweep: SweepSlot = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&sweep);
    let at_write_slot = Arc::clone(&sweep);
    let reached = hook_reached(Step::AfterFence, move || {
        sweep_behind_fence(url, parent, init, slot);
    });
    let published = hook_reached(Step::AfterWrite, move || {
        let sweep = at_write_slot.lock().unwrap();
        let sweep = sweep.as_ref().unwrap();
        assert!(
            !sweep.task.is_finished(),
            "actual sweep remains blocked through init publication"
        );
        let backend = sweep.backend;
        assert!(
            at_write_init.exists(),
            "init has been published while fence remains held"
        );
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let observer = independent_pool(&at_write_url).await;
                    let waiting: bool = sqlx::query_scalar(
                        "SELECT EXISTS (
                            SELECT 1 FROM pg_locks
                             WHERE pid = $1 AND relation = 'intake_outbox'::regclass
                               AND mode = 'RowExclusiveLock' AND NOT granted)",
                    )
                    .bind(backend)
                    .fetch_one(&observer)
                    .await
                    .unwrap();
                    assert!(
                        waiting,
                        "sweep lock remains ungranted after actual init write"
                    );
                    assert_eq!(
                        child_count(&observer, parent).await,
                        0,
                        "actual child must not commit before init publication finishes"
                    );
                    observer.close().await;
                });
        })
        .join()
        .unwrap();
    });
    case.eligible(&clock).await;
    wait_until("AfterFence actual sweep lock proof completed", || {
        reached.load(Ordering::SeqCst)
    })
    .await;
    assert!(
        published.load(Ordering::SeqCst),
        "AfterWrite checked actual init under the PG fence"
    );
    case.adopted(&clock).await;
    let sweep = sweep.lock().unwrap().take().unwrap();
    assert!(
        sweep.task.join().unwrap(),
        "actual child only committed after init publication"
    );
    assert_eq!(child_count(&pool, parent).await, 1);
    case.stop().await;
    drop(clock);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn sweep_before_normal_deferred_fence_observes_fresh_child_pg() {
    if isolated(concat!(
        module_path!(),
        "::sweep_before_normal_deferred_fence_observes_fresh_child_pg"
    )) {
        sweep_first(false).await;
    }
}

#[tokio::test]
async fn sweep_before_expired_handoff_fence_observes_fresh_child_pg() {
    if isolated(concat!(
        module_path!(),
        "::sweep_before_expired_handoff_fence_observes_fresh_child_pg"
    )) {
        sweep_first(true).await;
    }
}

#[tokio::test]
async fn normal_deferred_fence_blocks_actual_sweep_until_init_published_pg() {
    if isolated(concat!(
        module_path!(),
        "::normal_deferred_fence_blocks_actual_sweep_until_init_published_pg"
    )) {
        fence_first(false).await;
    }
}

#[tokio::test]
async fn expired_handoff_fence_blocks_actual_sweep_until_init_published_pg() {
    if isolated(concat!(
        module_path!(),
        "::expired_handoff_fence_blocks_actual_sweep_until_init_published_pg"
    )) {
        fence_first(true).await;
    }
}

type Posts = Arc<Mutex<Vec<(String, String, serde_json::Value)>>>;

async fn mock_headless_post() -> (
    Arc<poise::serenity_prelude::Http>,
    Posts,
    Arc<AtomicBool>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::response::IntoResponse;
    let posts: Posts = Arc::default();
    let reached = Arc::new(AtomicBool::new(false));
    let (release, released) = tokio::sync::oneshot::channel();
    let held = Arc::new(Mutex::new(Some(released)));
    let record = Arc::clone(&posts);
    let at_post = Arc::clone(&reached);
    let app = axum::Router::new().fallback(move |request: Request<Body>| {
        let record = Arc::clone(&record);
        let at_post = Arc::clone(&at_post);
        let held = Arc::clone(&held);
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_string();
            let auth = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let channel = format!("/api/v10/channels/{CHANNEL}");
            if method == Method::GET && path == channel {
                axum::Json(serde_json::json!({
                    "id": CHANNEL.to_string(), "type": 0, "name": "deferred-headless",
                    "guild_id": "6737000000", "position": 0, "permission_overwrites": [],
                    "nsfw": false, "parent_id": null
                }))
                .into_response()
            } else if method == Method::POST && path == format!("{channel}/messages") {
                record.lock().unwrap().push((path, auth, body.clone()));
                let released = held.lock().unwrap().take().unwrap();
                at_post.store(true, Ordering::SeqCst);
                released.await.unwrap();
                axum::Json(message_json(body["content"].as_str().unwrap())).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let http = poise::serenity_prelude::HttpBuilder::new("deferred-headless-token")
        .proxy(proxy)
        .ratelimiter_disabled(true)
        .build();
    (Arc::new(http), posts, reached, release, server)
}

fn message_json(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "6737000099", "channel_id": CHANNEL.to_string(),
        "author": {
            "id": "6737000098", "username": "deferred-headless", "discriminator": "0001",
            "global_name": null, "avatar": null, "bot": true, "system": false, "public_flags": 0
        },
        "content": content, "timestamp": "2026-10-10T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "attachments": [], "embeds": [], "nonce": null, "pinned": false,
        "type": 0, "flags": 0
    })
}

async fn outbox_status(pool: &PgPool, id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM message_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn normal_deferred_rejects_headless_enqueue_completed_after_fence_pg() {
    if !isolated(concat!(
        module_path!(),
        "::normal_deferred_rejects_headless_enqueue_completed_after_fence_pg"
    )) {
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut case = PgCase::new(pool.clone(), false);
    let clock = Clock::paused();
    case.start().await;
    let candidate =
        test_override::with_channels(|boot| boot.unwrap().candidate(CHANNEL).unwrap().clone());
    assert_eq!(candidate.sends(), (0, 0));
    let shared = test_override::shared_channels();
    let url = db.database_url.clone();
    let enqueued_id = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&enqueued_id);
    let reached = hook_reached(Step::AfterFence, move || {
        let id = std::thread::spawn(move || {
            let _selected = shared();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let independent = independent_pool(&url).await;
                    let target = format!("channel:{CHANNEL}");
                    let sent =
                        claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
                            enqueue_outbox_pg_returning_id_with_ttl(
                                &independent,
                                OutboxMessage {
                                    target: &target,
                                    content: "headless enqueued after fence",
                                    bot: "notify",
                                    source: "headless_turn",
                                    reason_code: Some("headless.delivery"),
                                    session_key: None,
                                },
                                0,
                            )
                            .await
                        })
                        .await;
                    let id = match sent {
                        Ok(BodySend::Sent(Ok(Some(id)))) => id,
                        other => panic!("post-fence Legacy enqueue must complete: {other:?}"),
                    };
                    independent.close().await;
                    id
                })
        })
        .join()
        .unwrap();
        *recorded.lock().unwrap() = Some(id);
    });
    case.eligible(&clock).await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the actual Deferred retry reached AfterFence"
    );
    let id = enqueued_id
        .lock()
        .unwrap()
        .expect("actual producer returned its row id");
    assert_eq!(
        case.samples(),
        [(0, 0)],
        "fence saw no queued body before the completed enqueue"
    );
    assert_eq!(
        candidate.sends(),
        (1, 1),
        "actual Legacy reservation started and finished"
    );
    assert_eq!(outbox_status(&pool, id).await, "pending");
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox
         WHERE id = $1 AND target = $2 AND source = 'headless_turn' AND status = 'pending'",
    )
    .bind(id)
    .bind(format!("channel:{CHANNEL}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending, 1, "one validated headless body remains queued");
    assert_eq!(
        crate::services::tui_o::cutover::claims_judged(CHANNEL),
        [false]
    );
    case.assert_waiting();
    case.stop().await;
    drop(clock);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn queued_headless_pending_and_processing_block_expired_handoff_until_worker_sent_pg() {
    if !isolated(concat!(
        module_path!(),
        "::queued_headless_pending_and_processing_block_expired_handoff_until_worker_sent_pg"
    )) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut case = PgCase::new(pool.clone(), true);
    let clock = Clock::paused();
    case.start().await;
    let target = format!("channel:{CHANNEL}");
    let enqueued = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        enqueue_outbox_pg_returning_id_with_ttl(
            &pool,
            OutboxMessage {
                target: &target,
                content: "legacy headless",
                bot: "notify",
                source: "headless_turn",
                reason_code: Some("headless.delivery"),
                session_key: None,
            },
            0,
        )
        .await
    })
    .await;
    let id = match enqueued {
        Ok(BodySend::Sent(Ok(Some(id)))) => id,
        other => panic!("actual Legacy reservation must enqueue one validated row: {other:?}"),
    };
    assert_eq!(outbox_status(&pool, id).await, "pending");
    case.eligible(&clock).await;
    wait_until("pending headless row reached fresh fence", || {
        !case.samples().is_empty()
    })
    .await;
    assert_eq!(case.samples().last(), Some(&(0, 1)));
    case.assert_waiting();

    let (http, posts, at_post, release, server) = mock_headless_post().await;
    let registry = Arc::new(HealthRegistry::new());
    registry
        .set_utility_bot_http_for_tests(UtilityBotRole::Notify, http)
        .await;
    let worker_pool = Arc::new(pool.clone());
    let worker = tokio::spawn(async move {
        crate::server::outbox_worker::drain_message_outbox_once(
            &worker_pool,
            &registry,
            "deferred-headless-worker",
        )
        .await
    });
    wait_until("actual worker reached its POST barrier", || {
        at_post.load(Ordering::SeqCst)
    })
    .await;
    assert_eq!(outbox_status(&pool, id).await, "processing");
    let before = case.samples().len();
    clock.tick().await;
    wait_until("processing headless row reached a new fresh fence", || {
        case.samples().len() > before
    })
    .await;
    assert_eq!(case.samples().last(), Some(&(0, 1)));
    case.assert_waiting();
    assert!(
        !worker.is_finished(),
        "actual worker stays inside its pending POST"
    );

    release.send(()).unwrap();
    wait_until("actual worker settled its POST and sent CAS", || {
        worker.is_finished()
    })
    .await;
    assert_eq!(worker.await.unwrap(), 1, "actual worker claimed one row");
    assert_eq!(outbox_status(&pool, id).await, "sent");
    clock.tick().await;
    case.adopted(&clock).await;
    let posts = posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "one actual Legacy HTTP POST");
    assert_eq!(posts[0].0, format!("/api/v10/channels/{CHANNEL}/messages"));
    assert_eq!(posts[0].1, "Bot deferred-headless-token");
    assert_eq!(posts[0].2["content"], "legacy headless");
    let second_enqueue = AtomicBool::new(false);
    let judged = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        second_enqueue.store(true, Ordering::SeqCst);
    })
    .await;
    assert!(matches!(judged, Ok(BodySend::OwnedByO)));
    assert!(!second_enqueue.load(Ordering::SeqCst));
    case.stop().await;
    server.abort();
    let _ = server.await;
    drop(clock);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn normal_deferred_rejects_headless_enqueue_completed_after_fence_from_a_preexisting_reservation_pg()
 {
    if !isolated(concat!(
        module_path!(),
        "::normal_deferred_rejects_headless_enqueue_completed_after_fence_from_a_preexisting_reservation_pg"
    )) {
        return;
    }
    struct Producer {
        release: std::sync::mpsc::Sender<()>,
        completed: std::sync::mpsc::Receiver<i64>,
        thread: std::thread::JoinHandle<()>,
    }

    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut case = PgCase::new(pool.clone(), false);
    let clock = Clock::paused();
    case.start().await;
    let candidate =
        test_override::with_channels(|boot| boot.unwrap().candidate(CHANNEL).unwrap().clone());
    assert_eq!(candidate.sends(), (0, 0));
    let before_candidate = candidate.clone();
    let after_candidate = candidate.clone();
    let shared = test_override::shared_channels();
    let url = db.database_url.clone();
    let producer: Arc<Mutex<Option<Producer>>> = Arc::new(Mutex::new(None));
    let held = Arc::clone(&producer);
    let before = hook_reached(Step::BeforeFence, move || {
        let (entered, at_transport) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (completed, result) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _selected = shared();
            let id = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let independent = independent_pool(&url).await;
                    let target = format!("channel:{CHANNEL}");
                    let sent =
                        claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
                            entered.send(()).unwrap();
                            released.recv_timeout(Duration::from_secs(10)).unwrap();
                            enqueue_outbox_pg_returning_id_with_ttl(
                                &independent,
                                OutboxMessage {
                                    target: &target,
                                    content: "preexisting reservation enqueued after fence",
                                    bot: "notify",
                                    source: "headless_turn",
                                    reason_code: Some("headless.delivery"),
                                    session_key: None,
                                },
                                0,
                            )
                            .await
                        })
                        .await;
                    let id = match sent {
                        Ok(BodySend::Sent(Ok(Some(id)))) => id,
                        other => panic!("preexisting Legacy enqueue must complete: {other:?}"),
                    };
                    independent.close().await;
                    id
                });
            completed.send(id).unwrap();
        });
        *held.lock().unwrap() = Some(Producer {
            release,
            completed: result,
            thread,
        });
        at_transport.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            before_candidate.sends(),
            (1, 0),
            "actual reservation is held before the fence samples sends"
        );
    });
    let completed_id = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&completed_id);
    let after = hook_reached(Step::AfterFence, move || {
        assert_eq!(after_candidate.sends(), (1, 0));
        let producer = producer.lock().unwrap().take().unwrap();
        producer.release.send(()).unwrap();
        let id = producer
            .completed
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        producer.thread.join().unwrap();
        assert_eq!(
            after_candidate.sends(),
            (1, 1),
            "the same reservation finished its canonical enqueue after the fence"
        );
        *recorded.lock().unwrap() = Some(id);
    });

    case.eligible(&clock).await;
    assert!(
        before.load(Ordering::SeqCst),
        "actual BeforeFence transport barrier reached"
    );
    assert!(
        after.load(Ordering::SeqCst),
        "actual AfterFence released and joined the producer"
    );
    let id = completed_id
        .lock()
        .unwrap()
        .expect("actual producer returned its row id");
    assert_eq!(
        case.samples(),
        [(0, 0)],
        "fence read queued count before the enqueue committed"
    );
    assert_eq!(
        candidate.sends(),
        (1, 1),
        "started is unchanged while finished advanced"
    );
    assert_eq!(outbox_status(&pool, id).await, "pending");
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox
         WHERE id = $1 AND target = $2 AND source = 'headless_turn' AND status = 'pending'",
    )
    .bind(id)
    .bind(format!("channel:{CHANNEL}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pending, 1,
        "one actual queued headless body survives the completed reservation"
    );
    assert_eq!(
        crate::services::tui_o::cutover::claims_judged(CHANNEL),
        [false]
    );
    case.assert_waiting();
    case.stop().await;
    drop(clock);
    pool.close().await;
    db.drop().await;
}
