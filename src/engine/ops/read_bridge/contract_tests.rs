//! The reader through real Contexts on a real database: one connection per Context, fresh
//! values, the caller's deadline and the acquire bound, and Contexts that never share a reader.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::engine::loader::ScopedBridgeDeadline;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;

/// config scalar, config blob fallback, kv live, kv expiring later, kv missing.
const READ_ALL: &str = r#"JSON.stringify([
    agentdesk.config.get("amp_scalar"),
    agentdesk.config.get("maxEntryRetries"),
    agentdesk.kv.get("live"),
    agentdesk.kv.get("soon"),
    agentdesk.kv.get("missing")
])"#;
const RAW_LIVE: &str = r#"agentdesk.kv.__getRaw("live")"#;
const GET_LIVE: &str = r#"JSON.stringify(agentdesk.kv.get("live"))"#;
const GET_SOON: &str = r#"JSON.stringify(agentdesk.kv.get("soon"))"#;
const DEADLINE_EXCEEDED: &str =
    r#"{"error":"bridge deadline exceeded during async bridge operation"}"#;
const WAIT: Duration = Duration::from_secs(10);

/// One Context with every op registered as the policy engine registers them.
pub(super) struct Js {
    context: rquickjs::Context,
    _runtime: rquickjs::Runtime,
}

impl Js {
    pub(super) fn new(pool: &sqlx::PgPool) -> Self {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context
            .with(|ctx| {
                super::super::register_globals_with_supervisor_and_pg(
                    &ctx,
                    Some(pool.clone()),
                    crate::supervisor::BridgeHandle::new(),
                )
            })
            .unwrap();
        Self {
            context,
            _runtime: runtime,
        }
    }

    pub(super) fn eval(&self, code: &str) -> String {
        self.context
            .with(|ctx| ctx.eval::<String, _>(code))
            .unwrap()
    }
}

/// Writes through the existing kv write bridge, from a Context of its own.
pub(super) fn write(pool: &sqlx::PgPool, writes: &[String]) {
    let js = Js::new(pool);
    for write in writes {
        let answer = js.eval(&format!("JSON.stringify({write})"));
        assert!(answer.contains(r#""ok":true"#), "{write}: {answer}");
    }
}

/// `amp_scalar` and `live` take the given values; `maxEntryRetries` exists only in the blob.
pub(super) fn seed(pool: &sqlx::PgPool, scalar: &str, live: &str) {
    write(
        pool,
        &[
            r#"JSON.parse(agentdesk.kv.delete("maxEntryRetries"))"#.to_string(),
            format!("agentdesk.kv.set('amp_scalar', {scalar:?}, 0)"),
            r#"agentdesk.kv.set("runtime-config", '{"maxEntryRetries":7}', 0)"#.to_string(),
            format!("agentdesk.kv.set('live', {live:?}, 0)"),
            r#"agentdesk.kv.set("soon", "later", 3600)"#.to_string(),
        ],
    );
}

/// Executions of either policy read SELECT the proxy forwarded since its last reset.
fn policy_selects(proxy: &PgWireProxy) -> usize {
    proxy
        .executions()
        .iter()
        .filter(|sql| sql.contains("FROM kv_meta") && sql.contains("WHERE key = $1"))
        .count()
}

/// Waits for a condition the database or proxy reaches on its own, failing after `WAIT`.
pub(super) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_read_bridge_reuses_context_resources() {
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    seed(&direct, "s1", "v1");
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);
    let js = Js::new(&pool);
    assert_eq!(
        proxy.accepted(),
        0,
        "registering a Context opens no connection"
    );

    for _ in 0..10 {
        assert_eq!(js.eval(READ_ALL), r#"["s1",7,"v1","later",null]"#);
    }
    assert_eq!(proxy.accepted(), 1, "every read shares one connection");
    assert_eq!(policy_selects(&proxy), 60, "every read runs its SELECTs");

    write(
        &direct,
        &[r#"agentdesk.kv.set("amp_scalar", "s2", 0)"#.to_string()],
    );
    assert_eq!(
        js.eval(
            r#"JSON.stringify([
                agentdesk.config.get("amp_scalar"),
                agentdesk.kv.set("live", "v2", 0),
                agentdesk.kv.get("live"),
                JSON.parse(agentdesk.kv.delete("live")),
                agentdesk.kv.get("live")
            ])"#
        ),
        r#"["s2",{"ok":true},"v2",{"ok":true},null]"#,
        "a read right after a write sees the write"
    );

    // Expiry is the database clock's: once another Context sees the key gone, so does this one.
    write(
        &direct,
        &[r#"agentdesk.kv.set("soon", "later", 1)"#.to_string()],
    );
    assert_eq!(js.eval(GET_SOON), r#""later""#);
    let observer = Js::new(&direct);
    until("the key to expire", || observer.eval(GET_SOON) == "null").await;
    assert_eq!(js.eval(GET_SOON), "null");
    drop(observer);

    pool.close().await;
    until("only the reader's own connection to stay open", || {
        proxy.open() == 1
    })
    .await;
    drop(js);
    until("the dropped Context's connection to close", || {
        proxy.open() == 0
    })
    .await;
    direct.close().await;
    drop(proxy);
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_read_bridge_deadline_and_context_isolation() {
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    seed(&direct, "s1", "v1");
    let proxy = PgWireProxy::start(&db.database_url, Duration::from_millis(400)).await;
    let pool = proxy.pool(2);

    // The handshake takes 400 ms and the SELECT is held: one 1.2 s budget covers both.
    proxy.stall("FROM kv_meta");
    let js = Js::new(&pool);
    let started = Instant::now();
    let answer = {
        let _deadline = ScopedBridgeDeadline::new(Duration::from_millis(1_200));
        js.eval(RAW_LIVE)
    };
    let elapsed = started.elapsed();
    assert_eq!(answer, DEADLINE_EXCEEDED);
    assert!(
        (Duration::from_millis(1_150)..Duration::from_millis(1_500)).contains(&elapsed),
        "the caller's deadline ends the read: {elapsed:?}"
    );
    until("the timed-out connection to close", || proxy.open() == 0).await;
    proxy.release();
    proxy.set_delay(Duration::ZERO);
    proxy.reset_executions();
    assert_eq!(js.eval(GET_LIVE), r#""v1""#);
    assert_eq!(proxy.accepted(), 2, "the next read opens a new connection");
    assert_eq!(
        policy_selects(&proxy),
        1,
        "the timed-out SELECT is not rerun"
    );

    // PostgreSQL refuses a NUL in text: the SELECT fails once and costs the connection.
    proxy.reset_executions();
    let failed = js.eval(r#"agentdesk.kv.__getRaw("bad\u0000key")"#);
    assert!(
        failed.starts_with(r#"{"error":"load postgres kv_meta bad\u0000key: "#),
        "{failed}"
    );
    assert_eq!(policy_selects(&proxy), 1, "a failed SELECT is not rerun");
    assert_eq!(js.eval(GET_LIVE), r#""v1""#);
    assert_eq!(proxy.accepted(), 3);

    // A Context whose SELECT is held does not hold up a Context on another pool.
    let side = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let side_pool = side.pool(2);
    proxy.stall("FROM kv_meta");
    let accepted = proxy.accepted();
    let (held_tx, held_rx) = mpsc::channel();
    let held_pool = pool.clone();
    let held = std::thread::spawn(move || {
        let js = Js::new(&held_pool);
        held_tx.send(js.eval(RAW_LIVE)).unwrap();
    });
    until("the held Context to connect", || {
        proxy.accepted() > accepted
    })
    .await;
    let (free_tx, free_rx) = mpsc::channel();
    let free_pool = side_pool.clone();
    let free = std::thread::spawn(move || {
        let js = Js::new(&free_pool);
        free_tx.send(js.eval(RAW_LIVE)).unwrap();
    });
    assert_eq!(
        free_rx
            .recv_timeout(WAIT)
            .expect("the other Context answers"),
        r#"{"found":true,"value":"v1"}"#
    );
    assert!(held_rx.try_recv().is_err(), "the held read is still held");
    proxy.release();
    assert_eq!(
        held_rx.recv_timeout(WAIT).expect("the held read finishes"),
        r#"{"found":true,"value":"v1"}"#
    );
    held.join().unwrap();
    free.join().unwrap();

    drop(js);
    pool.close().await;
    side_pool.close().await;
    direct.close().await;
    drop((proxy, side));
    db.drop().await;
}

/// Main and tick engines on two pools in front of one database, seeded by SQL rather than by a
/// Context, so no reader exists before theirs: each engine reads only through its own pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_read_bridge_engines_read_through_their_own_source() {
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    sqlx::query("DELETE FROM kv_meta WHERE key = 'maxEntryRetries'")
        .execute(&direct)
        .await
        .unwrap();
    for (key, value, ttl) in [
        ("amp_scalar", "s1", None),
        ("runtime-config", r#"{"maxEntryRetries":7}"#, None),
        ("live", "v1", None),
        ("soon", "later", Some(3_600_i64)),
    ] {
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second'))
             ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value, expires_at = EXCLUDED.expires_at",
        )
        .bind(key)
        .bind(value)
        .bind(ttl)
        .execute(&direct)
        .await
        .unwrap();
    }
    let (main_proxy, tick_proxy) = (
        PgWireProxy::start(&db.database_url, Duration::ZERO).await,
        PgWireProxy::start(&db.database_url, Duration::ZERO).await,
    );
    let (main_pool, tick_pool) = (main_proxy.pool(2), tick_proxy.pool(2));
    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from("/nonexistent");
    config.policies.hot_reload = false;
    let main = crate::engine::PolicyEngine::new_with_pg(&config, Some(main_pool.clone())).unwrap();
    let tick = crate::engine::PolicyEngine::new_for_tick(&config, Some(tick_pool.clone())).unwrap();
    let mut answers = Vec::new();
    for engine in [&main, &main, &main, &tick, &tick, &tick] {
        answers.push(engine.eval_js::<String>(READ_ALL).unwrap());
    }
    assert_eq!(
        (policy_selects(&main_proxy), policy_selects(&tick_proxy)),
        (18, 18),
        "each engine's six SELECTs per read go to its own pool"
    );
    assert!(
        answers
            .iter()
            .all(|answer| answer == r#"["s1",7,"v1","later",null]"#),
        "{answers:?}"
    );

    drop((main, tick));
    main_pool.close().await;
    tick_pool.close().await;
    direct.close().await;
    drop((main_proxy, tick_proxy));
    db.drop().await;
}

/// Runs reads on a thread with no runtime, as the policy actor does, each with its budget.
fn reads_on_a_thread(
    pool: sqlx::PgPool,
    budgets: Vec<Option<Duration>>,
) -> mpsc::Receiver<(String, Duration)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let js = Js::new(&pool);
        for budget in budgets {
            let started = Instant::now();
            let _deadline = budget.map(ScopedBridgeDeadline::new);
            let answer = js.eval(RAW_LIVE);
            if tx.send((answer, started.elapsed())).is_err() {
                return;
            }
        }
    });
    rx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_read_bridge_bounds_connecting_by_the_acquire_timeout_or_an_earlier_deadline() {
    let db = TestPostgresDb::create().await;
    // Every handshake waits 30 s at the proxy; the source pool gives up acquiring after 800 ms.
    let proxy = PgWireProxy::start(&db.database_url, Duration::from_secs(30)).await;
    let options = proxy.pool(1).connect_options().as_ref().clone();
    let source = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(800))
        .connect_lazy_with(options);

    let answers = reads_on_a_thread(source.clone(), vec![None, Some(Duration::from_millis(300))]);
    let (unbounded, elapsed) = answers
        .recv_timeout(WAIT)
        .expect("a read without a deadline ends at the acquire timeout");
    assert_eq!(
        unbounded,
        r#"{"error":"connect postgres bridge pool: pool timed out while waiting for an open connection"}"#
    );
    assert!(
        (Duration::from_millis(780)..Duration::from_millis(1_500)).contains(&elapsed),
        "{elapsed:?}"
    );
    let (bounded, elapsed) = answers
        .recv_timeout(WAIT)
        .expect("a read with a shorter deadline ends at it");
    assert_eq!(bounded, DEADLINE_EXCEEDED);
    assert!(
        (Duration::from_millis(280)..Duration::from_millis(780)).contains(&elapsed),
        "{elapsed:?}"
    );

    source.close().await;
    drop(proxy);
    db.drop().await;
}
