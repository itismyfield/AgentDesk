use super::*;
use crate::services::tui_o::repost::admission::OriginalGate;
use crate::services::tui_o::repost::runner::{RepostHook, Turned};
use crate::services::tui_o::store::ledger::PieceRecord;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};

#[derive(Default)]
struct Statements {
    armed: AtomicBool,
    simple: AtomicUsize,
    execute: AtomicUsize,
}

impl Statements {
    fn count(&self) -> usize {
        self.simple.load(Ordering::SeqCst) + self.execute.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.simple.store(0, Ordering::SeqCst);
        self.execute.store(0, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }
}

fn repost_sql(sql: &[u8]) -> bool {
    let sql = String::from_utf8_lossy(sql);
    ["o_piece_delivery", "o_piece_attempts", "o_piece_receipts"]
        .iter()
        .any(|table| sql.contains(table))
}

async fn observe_pg(mut client: TcpStream, mut server: TcpStream, counts: Arc<Statements>) {
    let Ok(length) = client.read_u32().await else {
        return;
    };
    let mut startup = vec![0; length as usize - 4];
    if client.read_exact(&mut startup).await.is_err()
        || server.write_u32(length).await.is_err()
        || server.write_all(&startup).await.is_err()
    {
        return;
    }
    let (mut from_client, mut to_client) = client.into_split();
    let (mut from_server, mut to_server) = server.into_split();
    let uplink = async move {
        let mut statements = BTreeMap::<Vec<u8>, bool>::new();
        let mut portals = BTreeMap::<Vec<u8>, bool>::new();
        while let Ok(kind) = from_client.read_u8().await {
            let length = from_client.read_u32().await?;
            let mut body = vec![0; length as usize - 4];
            from_client.read_exact(&mut body).await?;
            let mut fields = body.split(|byte| *byte == 0);
            let first = fields.next().unwrap_or_default();
            let second = fields.next().unwrap_or_default();
            let dedicated = match kind {
                b'P' => {
                    statements.insert(first.to_vec(), repost_sql(second));
                    false
                }
                b'B' => {
                    portals.insert(
                        first.to_vec(),
                        statements.get(second).copied().unwrap_or(false),
                    );
                    false
                }
                b'E' => portals.get(first).copied().unwrap_or(false),
                b'Q' => repost_sql(first),
                _ => false,
            };
            if dedicated && counts.armed.load(Ordering::SeqCst) {
                let count = if kind == b'Q' {
                    &counts.simple
                } else {
                    &counts.execute
                };
                count.fetch_add(1, Ordering::SeqCst);
            }
            to_server.write_u8(kind).await?;
            to_server.write_u32(length).await?;
            to_server.write_all(&body).await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let downlink = async move {
        tokio::io::copy(&mut from_server, &mut to_client)
            .await
            .map(|_| ())
    };
    let _ = tokio::try_join!(uplink, downlink);
}

async fn observed_pool(url: &str) -> (sqlx::PgPool, Arc<Statements>, tokio::task::JoinHandle<()>) {
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let upstream = (options.get_host().to_owned(), options.get_port());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counts = Arc::new(Statements::default());
    let observed = Arc::clone(&counts);
    let proxy = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = TcpStream::connect((upstream.0.as_str(), upstream.1))
                .await
                .unwrap();
            tokio::spawn(observe_pg(client, server, Arc::clone(&observed)));
        }
    });
    let options = options
        .host("127.0.0.1")
        .port(port)
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    (pool, counts, proxy)
}

struct EmptyBindings;

impl BindingEvents for EmptyBindings {
    fn binding_events_since(&self, _: u64, _: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(Vec::new())
    }
    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        watch::channel(0).1
    }
}

type RealWriter = ChannelWriter<GatewayPort, ChannelLeases, Arc<AlarmRouter>>;

struct Reached {
    inner: host::RepostTurn<GatewayHost>,
    completed: Arc<AtomicUsize>,
    notice: Arc<Notify>,
    measure: Arc<Notify>,
    measured: bool,
}

impl RepostHook<RealWriter> for Reached {
    fn turn<'a>(&'a mut self, writer: &'a mut RealWriter) -> Turned<'a, ()> {
        Box::pin(async move {
            self.inner.turn(writer).await;
            self.completed.fetch_add(1, Ordering::SeqCst);
            self.notice.notify_one();
            if !std::mem::replace(&mut self.measured, true) {
                self.measure.notified().await;
                self.inner.turn(writer).await;
                self.completed.fetch_add(1, Ordering::SeqCst);
                self.notice.notify_one();
            }
        })
    }
    fn original(&mut self, serial: u64, piece: PieceRecord) -> Turned<'_, OriginalGate> {
        self.inner.original(serial, piece)
    }
}

struct RealHost {
    gateway: GatewayHost,
    port: Arc<GatewayPort>,
    completed: Arc<AtomicUsize>,
    notice: Arc<Notify>,
    measure: Arc<Notify>,
    send: Option<(
        sqlx::PgPool,
        crate::services::tui_o::repost::identity::PieceKey,
        Arc<RetryIo>,
    )>,
}

impl HostIo for RealHost {
    type Port = GatewayPort;
    type Lease = ChannelLeases;
    type Alarms = Arc<AlarmRouter>;
    type Bindings = EmptyBindings;

    fn port(&self) -> impl Future<Output = Arc<GatewayPort>> + Send {
        std::future::ready(Arc::clone(&self.port))
    }
    fn lease(&self) -> ChannelLeases {
        self.gateway.lease()
    }
    fn alarms(&self) -> Arc<AlarmRouter> {
        self.gateway.alarms()
    }
    fn bindings(&self, _: u64, _: ShadowProvider) -> Arc<EmptyBindings> {
        Arc::new(EmptyBindings)
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.gateway.activation_facts(channel, provider)
    }
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.gateway.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.gateway.legacy()
    }
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.gateway.legacy_busy(channel)
    }
    fn relaying(&self, channel: u64) -> bool {
        self.gateway.relaying(channel)
    }
    fn repost(&self, channel: u64, port: &Arc<GatewayPort>) -> Option<host::RepostTurn<Self>> {
        if let Some((pool, key, io)) = &self.send {
            return Some(Box::new(SendOnce {
                pool: pool.clone(),
                key: key.clone(),
                io: Arc::clone(io),
                used: false,
            }));
        }
        Some(Box::new(Reached {
            inner: self.gateway.repost(channel, port)?,
            completed: Arc::clone(&self.completed),
            notice: Arc::clone(&self.notice),
            measure: Arc::clone(&self.measure),
            measured: false,
        }))
    }
}

#[tokio::test]
async fn f6_off_host_has_zero_pg_statements_with_on_positive_control_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, Candidate};
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::repost::config::RepostConfig;
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let runtime = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime.path(),
    );
    let fixture = TestPostgresDb::create().await;
    let migrated = fixture.connect_and_migrate().await;
    migrated.close().await;
    let (pool, statements, proxy) = observed_pool(&fixture.database_url).await;
    let select = "SELECT channel_id FROM o_piece_delivery LIMIT 0";
    sqlx::query(select).execute(&pool).await.unwrap();
    sqlx::raw_sql(select).execute(&pool).await.unwrap();
    statements.reset();
    sqlx::query(select).execute(&pool).await.unwrap();
    sqlx::raw_sql(select).execute(&pool).await.unwrap();
    assert!(statements.execute.load(Ordering::SeqCst) > 0);
    assert!(statements.simple.load(Ordering::SeqCst) > 0);
    statements.armed.store(false, Ordering::SeqCst);

    for enabled in [false, true] {
        let channel = 63250701 + u64::from(enabled);
        let root = runtime.path().join(channel.to_string());
        std::fs::create_dir_all(&root).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        store
            .begin_era(&[channel], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: Vec::new(),
                    initial_anchor: 100,
                    build_digest: "d4act".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let mut config = crate::config::Config::default();
        config.tui_o = Some(TuiOConfig {
            repost: RepostConfig { enabled },
            ..TuiOConfig::default()
        });
        crate::config_live_reload::install(config);
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let io = Arc::new(RealHost {
            gateway: GatewayHost {
                shared,
                alarms: Arc::new(AlarmRouter::for_process(None, None)),
                self_id_wait: Duration::ZERO,
                configured_id: None,
                turn: TurnConfig::default(),
                rest: None,
                boot: None,
            },
            port: Arc::new(GatewayPort::new(
                Arc::new(serenity::Http::new("test-token")),
                42,
            )),
            completed: Arc::new(AtomicUsize::new(0)),
            notice: Arc::new(Notify::new()),
            measure: Arc::new(Notify::new()),
            send: None,
        });
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let ready = Arc::new(host::Readiness::default());
        statements.armed.store(false, Ordering::SeqCst);
        let mut handles = host::start_managed(
            ShadowProvider::Claude,
            true,
            vec![(
                channel,
                Some(ClaudeTui),
                Some(Candidate::new(Adoption::Committed)),
            )],
            || host::HostParts {
                io: Arc::clone(&io),
                runtime_root: Some(root),
                gate,
                readiness: Arc::clone(&ready),
            },
        );
        assert_eq!(handles.len(), 1);
        tokio::time::timeout(Duration::from_secs(5), io.notice.notified())
            .await
            .expect("real host re-post turn completed");
        assert!(
            io.completed.load(Ordering::SeqCst) > 0,
            "the actor must reach the real factory hook"
        );
        let warmup_turns = io.completed.load(Ordering::SeqCst);
        // The host adopted its store and warmed the re-post turn before measurement begins.
        statements.reset();
        io.measure.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while io.completed.load(Ordering::SeqCst) == warmup_turns {
                io.notice.notified().await;
            }
        })
        .await
        .expect("a fresh real-host re-post turn completed while observed");
        let observed = statements.count();
        if enabled {
            assert!(
                observed > 0,
                "on must execute re-post statements in the same harness"
            );
        } else {
            assert_eq!(observed, 0, "off must execute zero re-post statements");
        }
        handles.pop().unwrap().stop_and_join().await.unwrap();
        statements.armed.store(false, Ordering::SeqCst);
        assert!(!ready.is_hosted(channel));
    }
    pool.close().await;
    proxy.abort();
    fixture.drop().await;
}

type JoinStop = std::pin::Pin<Box<dyn Future<Output = Result<host::WriterStopped, String>> + Send>>;
type RetryAction = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct RetryIo {
    wire: Arc<AtomicUsize>,
    waiting: Arc<Notify>,
    release: Arc<Notify>,
    finished: Arc<Notify>,
    before_retry: Arc<std::sync::Mutex<Option<RetryAction>>>,
}

impl crate::services::tui_o::repost::send::BoundedTransport for RetryIo {
    fn create(
        &self,
        envelope: &crate::services::tui_o::repost::send::RepostEnvelope,
        guard: crate::services::tui_o::repost::send::AttemptGuard,
    ) -> impl Future<Output = crate::services::tui_o::repost::send::WireOutcome> + Send + 'static
    {
        use crate::services::tui_o::repost::send::{CreatedMessage, WireOutcome};
        let body = serde_json::to_value(envelope.message()).unwrap();
        let content = body["content"].as_str().unwrap().to_owned();
        let footers = vec![
            body["embeds"][0]["footer"]["text"]
                .as_str()
                .unwrap()
                .to_owned(),
        ];
        let (wire, waiting, release, before) = (
            Arc::clone(&self.wire),
            Arc::clone(&self.waiting),
            Arc::clone(&self.release),
            Arc::clone(&self.before_retry),
        );
        async move {
            if let Err(reason) = guard.begin() {
                return WireOutcome::Unsent(reason);
            }
            wire.fetch_add(1, Ordering::SeqCst);
            guard.throttled();
            waiting.notify_one();
            release.notified().await;
            if let Some(stop_host) = before.lock().unwrap().take() {
                stop_host();
            }
            // The host stop is visible before its task can relay an actor stop.
            if let Err(reason) = guard.begin() {
                return WireOutcome::Unsent(reason);
            }
            wire.fetch_add(1, Ordering::SeqCst);
            WireOutcome::Created(CreatedMessage {
                id: 9001,
                author_id: 42,
                content,
                footers,
            })
        }
    }
}

struct SendOnce {
    pool: sqlx::PgPool,
    key: crate::services::tui_o::repost::identity::PieceKey,
    io: Arc<RetryIo>,
    used: bool,
}

fn host_run() -> crate::services::tui_o::repost::io::probe::evidence::RunScope {
    crate::services::tui_o::repost::io::probe::evidence::RunScope {
        holder: "host-node".into(),
        run: "host-run".into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    }
}

fn on_switch() -> crate::services::tui_o::repost::config::RepostSwitch {
    use crate::services::tui_o::repost::config::{RepostConfig, RepostSwitch};
    let on = TuiOConfig {
        repost: RepostConfig { enabled: true },
        ..TuiOConfig::default()
    };
    RepostSwitch::new(Some(&on), watch::channel(None).1)
}

impl RepostHook<RealWriter> for SendOnce {
    fn turn<'a>(&'a mut self, writer: &'a mut RealWriter) -> Turned<'a, ()> {
        use crate::services::tui_o::repost::dispatch::Admission;
        use crate::services::tui_o::repost::io::probe::evidence::EvidenceScope;
        use crate::services::tui_o::repost::io::probe::tests::absent_evidence;
        use crate::services::tui_o::repost::runner::{DispatchIntent, Runner, TurnWriter};
        use crate::services::tui_o::repost::{dispatch, o_piece_attempts, o_piece_delivery};

        Box::pin(async move {
            if std::mem::replace(&mut self.used, true) {
                return;
            }
            writer.set_live(true);
            writer.bind_switch(Arc::new(|| true));
            let row = o_piece_delivery::load(&self.pool, &self.key)
                .await
                .unwrap()
                .unwrap();
            let spent = o_piece_attempts::attempts(&self.pool, &self.key)
                .await
                .unwrap();
            let scope = EvidenceScope::of(&row, &spent, &host_run()).unwrap();
            let intent = DispatchIntent::Auto(Box::new(absent_evidence(scope, host_run())));
            let mut runner = Runner::new(on_switch(), self.pool.clone(), 42, host_run());
            let admission = runner
                .try_next_dispatch(writer, OriginalGate::Admitted, intent)
                .await
                .unwrap();
            let Admission::Granted { permit, lease } = admission else {
                panic!("real-host ready candidate must grant");
            };
            let started = dispatch::start(writer, permit, lease, &*self.io).await;
            started.finish(writer, &self.pool).await.unwrap();
            self.io.finished.notify_one();
        })
    }
    fn original(&mut self, _: u64, _: PieceRecord) -> Turned<'_, OriginalGate> {
        Box::pin(std::future::ready(OriginalGate::Admitted))
    }
}

#[tokio::test]
async fn d4act_host_stop_during_pending_finish_prevents_retry_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, Candidate};
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::repost::admission::{Admitter, OriginalAttempt};
    use crate::services::tui_o::repost::identity::PieceKey;
    use crate::services::tui_o::repost::o_piece_attempts::{AttemptResult, attempts};
    use crate::services::tui_o::repost::provenance::ProvenanceLog;
    use crate::services::tui_o::shadow::{UnitKey, UnitKind};
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let runtime = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime.path(),
    );
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    for stop_host in [false, true] {
        let channel = 63250711 + u64::from(stop_host);
        let root = runtime.path().join(channel.to_string());
        std::fs::create_dir_all(&root).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        store
            .begin_era(&[channel], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: Vec::new(),
                    initial_anchor: 100,
                    build_digest: "d4act".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let key = PieceKey::new(
            UnitKey {
                channel_id: channel,
                provider: ShadowProvider::Claude,
                native_key: "host-retry".into(),
                kind: UnitKind::Body,
            },
            0,
        )
        .unwrap();
        let mut provenance = ProvenanceLog::open(&root.join("seed.jsonl")).unwrap();
        let mut on = on_switch();
        let admitter = Admitter::when_on(&mut on, &pool, "host-node", 42).unwrap();
        admitter
            .adopt_uncertain(
                &mut provenance,
                &OriginalAttempt {
                    serial: 0,
                    key: key.clone(),
                    payload: "host retry".into(),
                    anchor: 100,
                },
            )
            .await
            .unwrap();
        let retry = Arc::new(RetryIo::default());
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let io = Arc::new(RealHost {
            gateway: GatewayHost {
                shared,
                alarms: Arc::new(AlarmRouter::for_process(None, None)),
                self_id_wait: Duration::ZERO,
                configured_id: None,
                turn: TurnConfig::default(),
                rest: None,
                boot: None,
            },
            port: Arc::new(GatewayPort::new(
                Arc::new(serenity::Http::new("test-token")),
                42,
            )),
            completed: Arc::new(AtomicUsize::new(0)),
            notice: Arc::new(Notify::new()),
            measure: Arc::new(Notify::new()),
            send: Some((pool.clone(), key.clone(), Arc::clone(&retry))),
        });
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let ready = Arc::new(host::Readiness::default());
        let mut handles = host::start_managed(
            ShadowProvider::Claude,
            true,
            vec![(
                channel,
                Some(ClaudeTui),
                Some(Candidate::new(Adoption::Committed)),
            )],
            || host::HostParts {
                io,
                runtime_root: Some(root),
                gate,
                readiness: Arc::clone(&ready),
            },
        );
        assert_eq!(handles.len(), 1);
        let mut handle = handles.pop();
        tokio::time::timeout(Duration::from_secs(5), retry.waiting.notified())
            .await
            .expect("first 429 reached");
        assert_eq!(retry.wire.load(Ordering::SeqCst), 1);
        let stopped = Arc::new(std::sync::Mutex::new(None::<JoinStop>));
        if stop_host {
            let handle = handle.take().unwrap();
            let saving = Arc::clone(&stopped);
            *retry.before_retry.lock().unwrap() = Some(Box::new(move || {
                *saving.lock().unwrap() = Some(Box::pin(handle.stop_and_join()));
            }));
        }
        retry.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), retry.finished.notified())
            .await
            .expect("pending finish settled");
        let stop = match handle {
            Some(handle) => Box::pin(handle.stop_and_join()) as JoinStop,
            None => stopped.lock().unwrap().take().unwrap(),
        };
        stop.await.unwrap();
        assert_eq!(
            retry.wire.load(Ordering::SeqCst),
            if stop_host { 1 } else { 2 }
        );
        let spent = attempts(&pool, &key).await.unwrap();
        assert_eq!(spent.len(), 2, "the consumed slot remains spent");
        assert_eq!(
            spent[1].result,
            Some(if stop_host {
                AttemptResult::NotSent
            } else {
                AttemptResult::Created
            })
        );
        assert!(!ready.is_hosted(channel));
    }
    pool.close().await;
    fixture.drop().await;
}
