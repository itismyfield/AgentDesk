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

#[path = "rollback_rehearsal_tests.rs"]
mod rollback_rehearsal_tests;
use rollback_rehearsal_tests::scan_activated_ledgers;

fn fixture_not_sent_tags(path: &std::path::Path) -> std::io::Result<usize> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|line| line["entry"]["type"] == "not_sent")
        .count())
}

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

    fn receipts(
        &mut self,
    ) -> Turned<'_, crate::services::tui_o::repost::projection::ReceiptOverlay> {
        self.inner.receipts()
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
    wire_turns: Option<Arc<WireTurns>>,
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
        if let Some(turns) = &self.wire_turns {
            return Some(Box::new(WireTurn {
                inner: self.gateway.repost(channel, port)?,
                turns: Arc::clone(turns),
                used: false,
            }));
        }
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
            repost: RepostConfig {
                enabled,
                channels: [channel].into_iter().collect(),
            },
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
            wire_turns: None,
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

fn on_switch(channel: u64) -> crate::services::tui_o::repost::config::RepostSwitch {
    use crate::services::tui_o::repost::config::{RepostConfig, RepostSwitch};
    let on = TuiOConfig {
        repost: RepostConfig {
            enabled: true,
            channels: [channel].into_iter().collect(),
        },
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
            let mut runner = Runner::new(
                on_switch(self.key.unit().channel_id),
                self.pool.clone(),
                42,
                host_run(),
            );
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

    fn receipts(
        &mut self,
    ) -> Turned<'_, crate::services::tui_o::repost::projection::ReceiptOverlay> {
        Box::pin(std::future::ready(
            crate::services::tui_o::repost::projection::ReceiptOverlay::Disabled,
        ))
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
        let mut on = on_switch(channel);
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
            wire_turns: None,
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

#[derive(Default)]
struct WireTurns {
    completed: AtomicUsize,
    proceed: Notify,
    notice: Notify,
}

impl WireTurns {
    async fn wait(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.completed.load(Ordering::SeqCst) < count {
                self.notice.notified().await;
            }
        })
        .await
        .expect("the real channel turn completed");
    }
}

struct WireTurn {
    inner: host::RepostTurn<GatewayHost>,
    turns: Arc<WireTurns>,
    used: bool,
}

impl RepostHook<RealWriter> for WireTurn {
    fn turn<'a>(&'a mut self, writer: &'a mut RealWriter) -> Turned<'a, ()> {
        Box::pin(async move {
            if std::mem::replace(&mut self.used, true) {
                return;
            }
            for count in 1..=3 {
                self.inner.turn(writer).await;
                self.turns.completed.store(count, Ordering::SeqCst);
                self.turns.notice.notify_one();
                if count < 3 {
                    self.turns.proceed.notified().await;
                }
            }
        })
    }
    fn original(&mut self, serial: u64, piece: PieceRecord) -> Turned<'_, OriginalGate> {
        self.inner.original(serial, piece)
    }

    fn receipts(
        &mut self,
    ) -> Turned<'_, crate::services::tui_o::repost::projection::ReceiptOverlay> {
        self.inner.receipts()
    }
}

struct WireServer {
    base: String,
    posts: Arc<AtomicUsize>,
    valid_posts: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    probe_reads: Arc<AtomicUsize>,
    first_429: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl WireServer {
    async fn start(channel: u64, key: &crate::services::tui_o::repost::identity::PieceKey) -> Self {
        use serde_json::json;
        let marker = crate::services::tui_o::repost::identity::marker(key);
        let nonce = crate::services::tui_o::repost::send::RepostIds::for_piece(&marker)
            .unwrap()
            .nonce()
            .to_owned();
        let footer = format!("재확인 후 추가 전달 {marker}");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (posts, valid_posts, reads, first_429) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Notify::new()),
        );
        let (sent, valid, read, first) = (
            posts.clone(),
            valid_posts.clone(),
            reads.clone(),
            first_429.clone(),
        );
        let probe_reads = Arc::new(AtomicUsize::new(0));
        let probed = Arc::clone(&probe_reads);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let head_end = loop {
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break None;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(at) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break Some(at + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < head_end + length {
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let mut request = head.lines().next().unwrap().split_whitespace();
                let (method, path) = (request.next().unwrap(), request.next().unwrap());
                let route = format!("/api/v10/channels/{channel}/messages");
                let proof = json!({
                    "id": "900", "channel_id": channel.to_string(),
                    "author": {"id": "42"}, "content": "earlier permission", "embeds": []
                });
                let (status, headers, body, throttled) = if method == "GET" {
                    read.fetch_add(1, Ordering::SeqCst);
                    // Message-route reads exclude unrelated root requests on the fixture listener.
                    if path.starts_with(&route) {
                        probed.fetch_add(1, Ordering::SeqCst);
                    }
                    if path == format!("{route}/900") {
                        ("200 OK", "", proof.to_string(), false)
                    } else if path.starts_with(&format!("{route}?")) {
                        ("200 OK", "", json!([proof]).to_string(), false)
                    } else {
                        ("404 Not Found", "", "{}".into(), false)
                    }
                } else if method == "POST" && path == route {
                    let count = sent.fetch_add(1, Ordering::SeqCst);
                    let message: serde_json::Value =
                        serde_json::from_slice(&bytes[head_end..head_end + length])
                            .unwrap_or_default();
                    if message["content"] == "absent target"
                        && message["nonce"] == nonce
                        && message["enforce_nonce"] == true
                        && message["embeds"][0]["footer"]["text"] == footer
                    {
                        valid.fetch_add(1, Ordering::SeqCst);
                    }
                    if count == 0 {
                        (
                            "429 Too Many Requests",
                            "retry-after: 2\r\nx-ratelimit-limit: 17\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset-after: 2\r\n",
                            r#"{"retry_after":2,"global":false}"#.into(),
                            true,
                        )
                    } else {
                        (
                            "200 OK",
                            "",
                            json!({
                                "id": "9001", "author": {"id": "42"},
                                "content": message["content"], "embeds": message["embeds"]
                            })
                            .to_string(),
                            false,
                        )
                    }
                } else {
                    ("400 Bad Request", "", "{}".into(), false)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                if throttled {
                    first.notify_one();
                }
            }
        });
        Self {
            base,
            posts,
            valid_posts,
            reads,
            probe_reads,
            first_429,
            task,
        }
    }
}

#[derive(Clone, Copy)]
enum RollbackStep {
    Off,
    Wait,
    Scan,
    Decide,
}

fn rollback_steps() -> Vec<RollbackStep> {
    let runbook = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/tui-o-writer-canary-runbook.md"),
    )
    .unwrap();
    let Some((_, section)) = runbook.split_once("## 8. Roll back after re-post activation") else {
        return Vec::new();
    };
    section
        .lines()
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| {
            let (number, action) = line.split_once(". ")?;
            number.parse::<usize>().ok()?;
            match action {
                "Confirm effective OFF." => Some(RollbackStep::Off),
                "Wait for in-flight requests and writer quiescence." => Some(RollbackStep::Wait),
                "Inspect all activated channel ledgers." => Some(RollbackStep::Scan),
                "Decide whether the older binary may run." => Some(RollbackStep::Decide),
                _ => None,
            }
        })
        .collect()
}

fn validated_repost(path: &std::path::Path, enabled: bool, channel: u64) -> bool {
    std::fs::write(
        path,
        format!("server:\n  port: 8791\ntui_o:\n  repost:\n    enabled: {enabled}\n    channels: [{channel}]\n"),
    )
    .unwrap();
    matches!(
        crate::config_live_reload::reload_from_path(path),
        crate::config_live_reload::ReloadOutcome::Applied { .. }
    ) && crate::config_live_reload::current().is_some_and(|config| {
        config
            .tui_o
            .as_ref()
            .is_some_and(|o| o.repost.enabled == enabled)
    })
}

struct WireCase {
    first_posts: usize,
    posts: usize,
    valid_posts: usize,
    reads: usize,
    early_tags: usize,
    late_tags: usize,
    allow_old: Option<bool>,
    queue_pending: bool,
    rate_headers: bool,
    quiescent: bool,
    old_bytes: bool,
    spent: Vec<(
        u8,
        Option<crate::services::tui_o::repost::o_piece_attempts::AttemptResult>,
    )>,
}

#[tokio::test(flavor = "current_thread")]
async fn d4act2_gateway_repost_reload_off_and_rollback_quiescence_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, Candidate};
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::repost::admission::{Admitter, OriginalAttempt};
    use crate::services::tui_o::repost::identity::PieceKey;
    use crate::services::tui_o::repost::io::probe::evidence::{
        FIRST_PASS_AFTER, SECOND_PASS_AFTER,
    };
    use crate::services::tui_o::repost::o_piece_attempts::{AttemptResult, attempts};
    use crate::services::tui_o::repost::provenance::{ProvenanceEntry, ProvenanceLog};
    use crate::services::tui_o::shadow::{UnitKey, UnitKind};
    use crate::services::tui_o::store::ledger::LedgerEntry;
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    use ::serenity::{all::ChannelId, http::Route};
    use std::task::Poll;

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
    let documented = rollback_steps();
    let mut activated_ledgers = Vec::new();
    let mut cases = Vec::new();
    let mut reloads_applied = true;
    for case in 0..3 {
        let enabled = case != 0;
        let channel = 63250731 + case;
        let root = runtime.path().join(channel.to_string());
        std::fs::create_dir_all(&root).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        let era = store
            .begin_era(&[channel], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: Vec::new(),
                    initial_anchor: 100,
                    build_digest: "loopback".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let unit = |native: &str| UnitKey {
            channel_id: channel,
            provider: ShadowProvider::Claude,
            native_key: native.into(),
            kind: UnitKind::Body,
        };
        let key = PieceKey::new(unit("wire-target"), 0).unwrap();
        let mut ledger = store.open_channel(&era, channel).unwrap().unwrap();
        for entry in [
            LedgerEntry::Prepared {
                local_retry: None,
                serial: 0,
                unit_key: key.unit().clone(),
                piece_index: 0,
                payload: "absent target".into(),
                anchor_id: 100,
                epoch: 1,
            },
            LedgerEntry::NotFound { serial: 0 },
            LedgerEntry::Prepared {
                local_retry: None,
                serial: 1,
                unit_key: unit("permission-proof"),
                piece_index: 0,
                payload: "earlier permission".into(),
                anchor_id: 100,
                epoch: 1,
            },
            LedgerEntry::Posted {
                serial: 1,
                msg_id: 900,
            },
        ] {
            ledger.append_ledger(entry).unwrap();
        }
        let original = ledger.ledger().piece(0).unwrap().clone();
        drop(ledger);
        let ledger_path = root.join(format!("o_store/{channel}/ledger.jsonl"));
        let old_bytes = std::fs::read(&ledger_path).unwrap();
        activated_ledgers.push(ledger_path.clone());
        let sidecar = runtime
            .path()
            .join("tui_o/repost")
            .join(format!("{channel}.jsonl"));
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        let mut provenance = ProvenanceLog::open(&sidecar).unwrap();
        let generation = provenance.activate(0).unwrap().generation;
        provenance
            .append(ProvenanceEntry::intent(0, &original, generation).unwrap())
            .unwrap();
        let mut on = on_switch(channel);
        Admitter::when_on(&mut on, &pool, "o-writer", 42)
            .unwrap()
            .adopt_uncertain(
                &mut provenance,
                &OriginalAttempt {
                    serial: 0,
                    key: key.clone(),
                    payload: original.payload,
                    anchor: original.anchor_id,
                },
            )
            .await
            .unwrap();
        drop(provenance);
        let config_path = runtime.path().join(format!("{channel}.yaml"));
        reloads_applied &= validated_repost(&config_path, enabled, channel);
        let server = WireServer::start(channel, &key).await;
        let http = Arc::new(serenity::Http::new(&format!("loopback-{channel}")));
        let turns = Arc::new(WireTurns::default());
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
            port: Arc::new(
                GatewayPort::new(http.clone(), 42).with_repost_base(server.base.clone()),
            ),
            completed: Arc::new(AtomicUsize::new(0)),
            notice: Arc::new(Notify::new()),
            measure: Arc::new(Notify::new()),
            send: None,
            wire_turns: Some(turns.clone()),
        });
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let readiness = Arc::new(host::Readiness::default());
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
                readiness: readiness.clone(),
            },
        );
        let mut handle = handles.pop();
        turns.wait(1).await;
        if enabled {
            tokio::time::pause();
            tokio::time::advance(FIRST_PASS_AFTER).await;
            tokio::time::resume();
        }
        turns.proceed.notify_one();
        turns.wait(2).await;
        if enabled {
            tokio::time::pause();
            tokio::time::advance(SECOND_PASS_AFTER).await;
            tokio::time::resume();
        }
        turns.proceed.notify_one();
        let reached = if enabled {
            tokio::select! {
                () = server.first_429.notified() => true,
                () = turns.wait(3) => false,
            }
        } else {
            turns.wait(3).await;
            false
        };
        let first_posts = server.posts.load(Ordering::SeqCst);
        let (mut early_tags, mut allow_old, mut queue_pending, mut rate_headers) =
            (0, None, false, false);
        if reached {
            let routes = http.ratelimiter.as_ref().unwrap().routes();
            let bucket = routes
                .read()
                .await
                .get(
                    &Route::ChannelMessages {
                        channel_id: ChannelId::new(channel),
                    }
                    .ratelimiting_bucket(),
                )
                .unwrap()
                .clone();
            // On this executor the sole lock holder can yield inside post_hook only at its 429 sleep.
            tokio::time::timeout(Duration::from_secs(5), async {
                while bucket.try_lock().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the actual route hook is waiting after 429");
            let mut queued = Some(Box::pin(bucket.lock_owned()));
            let mut already_held = None;
            queue_pending =
                std::future::poll_fn(|cx| match queued.as_mut().unwrap().as_mut().poll(cx) {
                    Poll::Pending => Poll::Ready(true),
                    Poll::Ready(guard) => {
                        already_held = Some(guard);
                        Poll::Ready(false)
                    }
                })
                .await;
            if !queue_pending {
                queued = None;
                drop(already_held.take());
            }
            if case == 1 {
                if let Some(waiter) = queued.take() {
                    let guard = waiter.await;
                    rate_headers = guard.limit() == 17
                        && guard.remaining() == 0
                        && guard.reset_after() == Some(Duration::from_secs(2))
                        && guard.reset().is_some();
                    drop(guard);
                }
                turns.wait(3).await;
            }
            let canonical = [
                RollbackStep::Off,
                RollbackStep::Wait,
                RollbackStep::Scan,
                RollbackStep::Decide,
            ];
            let steps = if documented.len() == 4 {
                &documented[..]
            } else {
                &canonical[..]
            };
            let mut scanned = None;
            let mut off_confirmed = false;
            let mut joined = false;
            for step in steps {
                match step {
                    RollbackStep::Off => {
                        off_confirmed = validated_repost(&config_path, false, channel);
                        reloads_applied &= off_confirmed;
                        assert!(
                            scan_activated_ledgers(
                                &activated_ledgers,
                                off_confirmed,
                                joined,
                                true,
                                fixture_not_sent_tags,
                            )
                            .is_err()
                        );
                        // This early observation cannot authorize rollback before the writer joins.
                        early_tags = fixture_not_sent_tags(&ledger_path).unwrap();
                    }
                    RollbackStep::Wait => {
                        if let Some(waiter) = queued.take() {
                            let guard = waiter.await;
                            rate_headers = guard.limit() == 17
                                && guard.remaining() == 0
                                && guard.reset_after() == Some(Duration::from_secs(2))
                                && guard.reset().is_some();
                            drop(guard);
                        }
                        turns.wait(3).await;
                        if let Some(writer) = handle.take() {
                            writer.stop_and_join().await.unwrap();
                        }
                        joined = !readiness.is_hosted(channel) && handle.is_none();
                    }
                    RollbackStep::Scan => {
                        scanned = scan_activated_ledgers(
                            &activated_ledgers,
                            off_confirmed,
                            joined,
                            true,
                            fixture_not_sent_tags,
                        )
                        .ok()
                        .map(|reads| reads.iter().any(|(_, tags)| !matches!(tags, Ok(0))));
                    }
                    RollbackStep::Decide => {
                        allow_old = Some(scanned == Some(false));
                    }
                }
            }
            if let Some(waiter) = queued.take() {
                drop(waiter.await);
            }
        }
        if let Some(writer) = handle.take() {
            writer.stop_and_join().await.unwrap();
        }
        let spent = attempts(&pool, &key)
            .await
            .unwrap()
            .into_iter()
            .map(|attempt| (attempt.slot, attempt.result))
            .collect();
        let bytes = std::fs::read(&ledger_path).unwrap();
        cases.push(WireCase {
            first_posts,
            posts: server.posts.load(Ordering::SeqCst),
            valid_posts: server.valid_posts.load(Ordering::SeqCst),
            reads: server.reads.load(Ordering::SeqCst),
            early_tags,
            late_tags: fixture_not_sent_tags(&ledger_path).unwrap(),
            allow_old,
            queue_pending,
            rate_headers,
            quiescent: !readiness.is_hosted(channel),
            old_bytes: if case == 0 {
                bytes == old_bytes
            } else {
                bytes.starts_with(&old_bytes)
            },
            spent,
        });
        server.task.abort();
        let _ = server.task.await;
        if enabled && !reached {
            break;
        }
    }
    pool.close().await;
    fixture.drop().await;
    for case in cases.iter().skip(1) {
        assert_eq!(
            case.first_posts, 1,
            "the third production turn must reach the actual first POST"
        );
    }
    assert_eq!(
        cases.len(),
        3,
        "all three cases use the same real factory harness"
    );
    assert!(
        reloads_applied,
        "every switch change came from a validated reload"
    );
    assert!(cases.iter().all(|case| case.quiescent && case.old_bytes));
    assert!(
        cases.iter().all(|case| case.valid_posts == case.posts),
        "each actual POST, including the first 429, carried the original payload and identifiers"
    );
    assert_eq!(
        (cases[0].posts, cases[0].reads, cases[0].late_tags),
        (0, 0, 0)
    );
    for case in cases.iter().skip(1) {
        assert!(
            case.queue_pending && case.rate_headers,
            "the factory's route hook really waited on 429"
        );
        assert_eq!(
            case.reads, 4,
            "two real permission/history passes supplied absence"
        );
        assert_eq!(
            case.early_tags, 0,
            "an effective OFF inspection can precede terminal recording"
        );
    }
    assert_eq!((cases[1].posts, cases[1].late_tags), (2, 0));
    assert_eq!((cases[2].posts, cases[2].late_tags), (1, 1));
    assert!(
        std::fs::read(&activated_ledgers[2])
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .any(|line| line["entry"]["type"] == "not_sent" && line["entry"]["serial"] == 2),
        "late NotSent belongs to the new re-post; old Prepared/NotFound remain intact"
    );
    assert_eq!(
        cases[1].spent,
        vec![
            (0, Some(AttemptResult::Uncertain)),
            (1, Some(AttemptResult::Created))
        ]
    );
    assert_eq!(
        cases[2].spent,
        vec![
            (0, Some(AttemptResult::Uncertain)),
            (1, Some(AttemptResult::NotSent))
        ]
    );
    assert_eq!(
        documented.len(),
        4,
        "the runbook must supply all four rollback operations"
    );
    assert_eq!(
        cases[1].allow_old,
        Some(true),
        "quiescent ledgers without the tag permit compatibility assessment"
    );
    assert_eq!(
        cases[2].allow_old,
        Some(false),
        "REFUSE an older reader after late NotSent; do not reuse an early scan"
    );
}

#[path = "allowlist_host_tests.rs"]
mod allowlist;
