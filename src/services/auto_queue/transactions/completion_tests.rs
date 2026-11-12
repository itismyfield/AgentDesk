//! Run-completion notice acceptance: observation points under config reload, one tx for the
//! completing writes and every notice, census metadata failures and racing callers.

use super::{finalize_run_if_ready_on_pg_tx, finalize_run_if_ready_pg};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::auto_queue::{
    ENTRY_STATUS_SKIPPED, EntryStatusUpdateOptions, RunCompletionBlockedReason,
    RunCompletionOutcome,
};
use crate::test_support::wait::{WAIT_CAP, await_within};
use serde_json::{Value, json};
use sqlx::PgPool;

/// Advisory key the test triggers wait on.
const GATE_KEY: i64 = 7_353_014;

struct Isolation {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}

impl Isolation {
    /// `None` leaves the live snapshot uninstalled.
    fn new(config: Option<crate::config::Config>) -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.yaml");
        std::fs::write(&path, "server: {}\n").unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let guards = [
            set("AGENTDESK_ROOT_DIR", root.path()),
            set("AGENTDESK_CONFIG", &path),
        ];
        if let Some(config) = config {
            crate::config_live_reload::install(config);
        }
        Self {
            _guards: guards,
            _live: live,
            _root: root,
        }
    }
}

/// `system_agent` adds an agent whose id equals the notice source, which withholds the census.
fn config(default_provider: &str, system_agent: bool) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.onboarding.default_provider = Some(default_provider.to_string());
    if system_agent {
        config
            .agents
            .push(serde_json::from_value(json!({"id": "system", "name": "collision"})).unwrap());
    }
    config
}

/// One completable run whose slot is held. `channels` are legacy-primary agents (provider
/// NULL), each owning one finished entry; with one channel the run's own agent is the target.
async fn completable_run(label: &str, channels: &[&str]) -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(6).await;
    for (index, channel) in channels.iter().enumerate() {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, $1, NULL, $2)",
        )
        .bind(format!("agent-{index}"))
        .bind(channel)
        .execute(&pool)
        .await
        .unwrap();
    }
    let run_agent = (channels.len() == 1).then_some("agent-0");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-1', $1, $2, 'active')",
    )
    .bind(label)
    .bind(run_agent)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-0', 0, 'run-1', 0, CAST('{}' AS jsonb))",
    )
    .execute(&pool)
    .await
    .unwrap();
    for index in 0..channels.len() {
        seed_entry(
            &pool,
            &format!("entry-{index}"),
            &format!("agent-{index}"),
            "done",
        )
        .await;
    }
    (db, pool)
}

async fn seed_entry(pool: &PgPool, entry_id: &str, agent_id: &str, status: &str) {
    let card_id = format!("card-{entry_id}");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', $2)",
    )
    .bind(&card_id)
    .bind(agent_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ($1, 'run-1', $2, $3, $4)",
    )
    .bind(entry_id)
    .bind(&card_id)
    .bind(agent_id)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

/// Row trigger on `message_outbox` that runs `action` for rows whose target is `target`.
async fn outbox_trigger(pool: &PgPool, name: &str, timing: &str, target: &str, action: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {name}() RETURNS trigger AS $$
         BEGIN
             IF NEW.target = '{target}' THEN {action}; END IF;
             RETURN NEW;
         END $$ LANGUAGE plpgsql"
    )))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER {name} {timing} ON message_outbox
         FOR EACH ROW EXECUTE FUNCTION {name}()"
    )))
    .execute(pool)
    .await
    .unwrap();
}

const WAIT_ON_GATE: &str = "PERFORM pg_advisory_xact_lock(7353014)";

/// Holds [`GATE_KEY`] on its own connection until [`Gate::open`].
struct Gate(sqlx::pool::PoolConnection<sqlx::Postgres>);

impl Gate {
    async fn close(pool: &PgPool) -> Self {
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(GATE_KEY)
            .execute(&mut *connection)
            .await
            .unwrap();
        Self(connection)
    }

    /// Backend pid of the session parked on the gate.
    async fn waiter(pool: &PgPool) -> i32 {
        await_within(
            async {
                loop {
                    let waiter: Option<i32> = sqlx::query_scalar(
                        "SELECT pid FROM pg_locks
                          WHERE locktype = 'advisory' AND NOT granted AND objid = $1",
                    )
                    .bind(GATE_KEY as i32)
                    .fetch_optional(pool)
                    .await
                    .unwrap();
                    if let Some(pid) = waiter {
                        return pid;
                    }
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await
    }

    async fn open(mut self) {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(GATE_KEY)
            .execute(&mut *self.0)
            .await
            .unwrap();
    }
}

async fn await_table_waiter(pool: &PgPool, relation: &str) {
    await_within(
        async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks
                                     WHERE NOT granted AND relation = $1::text::regclass)",
                )
                .bind(relation)
                .fetch_one(pool)
                .await
                .unwrap();
                if waiting {
                    return;
                }
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await
}

fn spawn_finalize(pool: &PgPool) -> tokio::task::JoinHandle<Result<RunCompletionOutcome, String>> {
    let pool = pool.clone();
    tokio::spawn(async move { finalize_run_if_ready_pg(&pool, "run-1").await })
}

/// `(target, provider_hint, body_census)` of every notice, by target.
async fn notices(pool: &PgPool) -> Vec<(String, Option<String>, Option<Value>)> {
    sqlx::query_as::<_, (String, Option<Value>, Option<Value>)>(
        "SELECT target, session_notice, body_census FROM message_outbox ORDER BY target, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(target, designation, census)| {
        let hint =
            designation.and_then(|value| value["provider_hint"].as_str().map(str::to_string));
        (target, hint, census)
    })
    .collect()
}

async fn run_state(pool: &PgPool) -> (String, Option<String>) {
    sqlx::query_as(
        "SELECT r.status, s.assigned_run_id
           FROM auto_queue_runs r, auto_queue_slots s
          WHERE r.id = 'run-1' AND s.agent_id = 'agent-0' AND s.slot_index = 0",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn census_kind(census: &Option<Value>) -> Option<&str> {
    census
        .as_ref()
        .and_then(|value| value["notice_kind"].as_str())
}

/// A reload while the notice targets are pending reaches D; one after D reaches only C.
#[tokio::test]
async fn completion_notice_reads_default_after_targets_and_census_after_insert_pg() {
    let _isolation = Isolation::new(Some(config("claude", true)));
    let (db, pool) = completable_run("observation", &["7353101"]).await;
    outbox_trigger(
        &pool,
        "gate_insert",
        "BEFORE INSERT",
        "channel:7353101",
        WAIT_ON_GATE,
    )
    .await;
    let gate = Gate::close(&pool).await;
    let mut agents_gate = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *agents_gate)
        .await
        .unwrap();
    let finalizer = spawn_finalize(&pool);

    await_table_waiter(&pool, "agents").await;
    crate::config_live_reload::install(config("codex", true));
    agents_gate.commit().await.unwrap();
    Gate::waiter(&pool).await;
    crate::config_live_reload::install(config("claude", false));
    gate.open().await;

    let outcome = await_within(finalizer, WAIT_CAP).await.unwrap().unwrap();
    assert_eq!(outcome, RunCompletionOutcome::Completed);
    let notices = notices(&pool).await;
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].1.as_deref(),
        Some("codex"),
        "D reads the default after the targets and keeps it through the INSERT"
    );
    assert_eq!(
        census_kind(&notices[0].2),
        Some("system"),
        "C sees the config installed while the INSERT waited"
    );
    pool.close().await;
    db.drop().await;
}

/// The notice targets are read after the completing UPDATE: a channel change that commits
/// while that UPDATE waits on the run row reaches the notice.
#[tokio::test]
async fn completion_notice_reads_targets_after_the_completing_update_pg() {
    let _isolation = Isolation::new(Some(config("claude", false)));
    let (db, pool) = completable_run("targets", &["7353151"]).await;
    let mut row_lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM auto_queue_runs WHERE id = 'run-1' FOR UPDATE")
        .execute(&mut *row_lock)
        .await
        .unwrap();
    let finalizer = spawn_finalize(&pool);
    await_table_waiter_on_rows(&pool).await;
    sqlx::query("UPDATE agents SET discord_channel_id = '7353152' WHERE id = 'agent-0'")
        .execute(&pool)
        .await
        .unwrap();
    row_lock.commit().await.unwrap();

    let outcome = await_within(finalizer, WAIT_CAP).await.unwrap().unwrap();
    assert_eq!(outcome, RunCompletionOutcome::Completed);
    let targets: Vec<_> = notices(&pool).await.into_iter().map(|n| n.0).collect();
    assert_eq!(targets, vec!["channel:7353152".to_string()]);
    pool.close().await;
    db.drop().await;
}

/// Waits until some session is parked on a row lock (`transactionid` wait).
async fn await_table_waiter_on_rows(pool: &PgPool) {
    await_within(
        async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks
                                     WHERE NOT granted AND locktype = 'transactionid')",
                )
                .fetch_one(pool)
                .await
                .unwrap();
                if waiting {
                    return;
                }
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await
}

/// Each target reads its own default: a reload between two notices reaches only the second.
#[tokio::test]
async fn completion_notice_targets_read_the_default_one_row_at_a_time_pg() {
    let _isolation = Isolation::new(Some(config("claude", false)));
    let (db, pool) = completable_run("two rows", &["7353201", "7353202"]).await;
    // The first notice parks while storing its census, after its INSERT and before the second D.
    outbox_trigger(
        &pool,
        "gate_census",
        "BEFORE UPDATE",
        "channel:7353201",
        WAIT_ON_GATE,
    )
    .await;
    let gate = Gate::close(&pool).await;
    let finalizer = spawn_finalize(&pool);

    Gate::waiter(&pool).await;
    crate::config_live_reload::install(config("codex", false));
    gate.open().await;

    let outcome = await_within(finalizer, WAIT_CAP).await.unwrap().unwrap();
    assert_eq!(outcome, RunCompletionOutcome::Completed);
    let notices = notices(&pool).await;
    let hints: Vec<_> = notices
        .iter()
        .map(|(target, hint, _)| (target.as_str(), hint.as_deref()))
        .collect();
    assert_eq!(
        hints,
        vec![
            ("channel:7353201", Some("claude")),
            ("channel:7353202", Some("codex"))
        ]
    );
    assert!(
        notices
            .iter()
            .all(|notice| census_kind(&notice.2) == Some("system"))
    );
    pool.close().await;
    db.drop().await;
}

/// Without a config snapshot, or with an agent named like the source, the notice keeps its
/// designation and commits without census metadata.
#[tokio::test]
async fn completion_notice_withholds_census_without_config_or_on_source_collision_pg() {
    for (case, installed) in [
        ("no config", None),
        ("collision", Some(config("claude", true))),
    ] {
        let _isolation = Isolation::new(installed.clone());
        if installed.is_none() {
            assert!(
                crate::config::snapshot().is_none(),
                "this case needs an uninstalled live config"
            );
        }
        let (db, pool) = completable_run(case, &["7353301"]).await;
        assert_eq!(
            finalize_run_if_ready_pg(&pool, "run-1").await.unwrap(),
            RunCompletionOutcome::Completed,
            "{case}"
        );
        let notices = notices(&pool).await;
        assert_eq!(notices.len(), 1, "{case}");
        assert!(notices[0].1.is_some(), "{case}: the notice is designated");
        assert_eq!(notices[0].2, None, "{case}: census withheld");
        assert_eq!(
            run_state(&pool).await,
            ("completed".to_string(), None),
            "{case}"
        );
        pool.close().await;
        db.drop().await;
    }
}

/// The completing UPDATE, slot release and every notice share one tx on one backend; a
/// failed second INSERT leaves none of them.
#[tokio::test]
async fn completion_and_every_notice_commit_or_roll_back_together_pg() {
    let _isolation = Isolation::new(Some(config("claude", false)));
    let (db, pool) = completable_run("one tx", &["7353401", "7353402"]).await;
    sqlx::query("CREATE TABLE notice_probe (target TEXT, pid INT, xid BIGINT)")
        .execute(&pool)
        .await
        .unwrap();
    outbox_trigger(
        &pool,
        "probe_insert",
        "AFTER INSERT",
        "channel:7353401",
        "INSERT INTO notice_probe VALUES (NEW.target, pg_backend_pid(), txid_current())",
    )
    .await;
    outbox_trigger(
        &pool,
        "reject_second",
        "BEFORE INSERT",
        "channel:7353402",
        "RAISE EXCEPTION 'second notice rejected by test'",
    )
    .await;

    let error = finalize_run_if_ready_pg(&pool, "run-1")
        .await
        .expect_err("a failed notice fails the completion");
    assert!(error.contains("second notice rejected by test"), "{error}");
    assert_eq!(
        run_state(&pool).await,
        ("active".to_string(), Some("run-1".to_string()))
    );
    assert!(notices(&pool).await.is_empty());
    let probes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notice_probe")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        probes, 0,
        "the first notice rolled back with the completion"
    );

    sqlx::query("DROP TRIGGER reject_second ON message_outbox")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let (pid, xid): (i32, i64) = sqlx::query_as("SELECT pg_backend_pid(), txid_current()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        finalize_run_if_ready_on_pg_tx(&mut tx, "run-1")
            .await
            .unwrap(),
        RunCompletionOutcome::Completed
    );
    let probe: (i32, i64) = sqlx::query_as("SELECT pid, xid FROM notice_probe")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        probe,
        (pid, xid),
        "the notice INSERT ran on the caller's backend and tx"
    );
    assert_eq!(
        run_state(&pool).await.0,
        "active",
        "nothing is visible before commit"
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        run_state(&pool).await,
        ("active".to_string(), Some("run-1".to_string()))
    );
    assert!(notices(&pool).await.is_empty());
    pool.close().await;
    db.drop().await;
}

/// A failed census UPDATE is optional metadata; a lost connection inside the savepoint is not.
#[tokio::test]
async fn completion_census_update_failure_commits_but_lost_savepoint_fails_pg() {
    let _isolation = Isolation::new(Some(config("claude", false)));
    let (db, pool) = completable_run("census update", &["7353501"]).await;
    outbox_trigger(
        &pool,
        "reject_census",
        "BEFORE UPDATE",
        "channel:7353501",
        "RAISE EXCEPTION 'census rejected by test'",
    )
    .await;
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1").await.unwrap(),
        RunCompletionOutcome::Completed
    );
    let stored = notices(&pool).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].2, None, "the rolled-back savepoint keeps the row");
    assert_eq!(run_state(&pool).await, ("completed".to_string(), None));
    pool.close().await;
    db.drop().await;

    let (db, pool) = completable_run("census rollback", &["7353502"]).await;
    outbox_trigger(
        &pool,
        "gate_census",
        "BEFORE UPDATE",
        "channel:7353502",
        WAIT_ON_GATE,
    )
    .await;
    let gate = Gate::close(&pool).await;
    let finalizer = spawn_finalize(&pool);
    let pid = Gate::waiter(&pool).await;
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1, 5000)")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(terminated);
    gate.open().await;
    await_within(finalizer, WAIT_CAP)
        .await
        .unwrap()
        .expect_err("the savepoint rollback cannot report success on a lost backend");
    assert_eq!(
        run_state(&pool).await,
        ("active".to_string(), Some("run-1".to_string()))
    );
    assert!(notices(&pool).await.is_empty());
    pool.close().await;
    db.drop().await;
}

/// An entry writer whose try-lock loses to a waiting explicit completion leaves the run to it;
/// the run completes once, and a caller after a restart finds it terminal without a new notice.
#[tokio::test]
async fn racing_completion_callers_and_a_restart_queue_one_notice_pg() {
    let _isolation = Isolation::new(Some(config("claude", false)));
    let (db, pool) = completable_run("two callers", &["7353601"]).await;
    seed_entry(&pool, "entry-last", "agent-0", "pending").await;
    let mut token = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'run-1'))")
        .execute(&mut *token)
        .await
        .unwrap();
    let finalizer = spawn_finalize(&pool);
    await_within(
        async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks
                                     WHERE locktype = 'advisory' AND NOT granted)",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                if waiting {
                    return;
                }
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await;

    let skipped = crate::services::auto_queue::transactions::update_entry_status_on_pg(
        &pool,
        "entry-last",
        ENTRY_STATUS_SKIPPED,
        "test_racing_skip",
        &EntryStatusUpdateOptions::default(),
    )
    .await
    .unwrap();
    assert!(skipped.changed);
    assert_eq!(
        run_state(&pool).await.0,
        "active",
        "the contended entry writer defers"
    );
    assert!(notices(&pool).await.is_empty());
    token.commit().await.unwrap();
    assert_eq!(
        await_within(finalizer, WAIT_CAP).await.unwrap().unwrap(),
        RunCompletionOutcome::Completed
    );
    assert_eq!(run_state(&pool).await, ("completed".to_string(), None));
    assert_eq!(notices(&pool).await.len(), 1);

    pool.close().await;
    let restarted = db.connect_and_migrate().await;
    assert_eq!(
        finalize_run_if_ready_pg(&restarted, "run-1").await.unwrap(),
        RunCompletionOutcome::AlreadyTerminal
    );
    assert_eq!(notices(&restarted).await.len(), 1);
    restarted.close().await;
    db.drop().await;
}

/// A blocked attempt writes nothing, so it can never leave a notice behind.
#[tokio::test]
async fn blocked_completion_queues_no_notice_pg() {
    let _isolation = Isolation::new(Some(config("claude", false)));
    let (db, pool) = completable_run("blocked", &["7353701"]).await;
    seed_entry(&pool, "entry-open", "agent-0", "pending").await;
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1").await.unwrap(),
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::RunnableEntry)
    );
    assert!(notices(&pool).await.is_empty());
    assert_eq!(
        run_state(&pool).await,
        ("active".to_string(), Some("run-1".to_string()))
    );
    pool.close().await;
    db.drop().await;
}
