//! PG contention tests for the run-owner guard: each holds a real cancellation (or a raw
//! lock) uncommitted, proves the writer waits on the exact lock, then checks the outcome.

use std::time::{Duration, Instant};

use regex::Regex;
use sqlx::{Connection, PgConnection, PgPool, Postgres, Transaction};
use tokio::task::JoinHandle;

use super::PhaseGateStateWrite;
use super::test_support::TestPostgresDb;

const AGENT: &str = "agent-owner-guard";
const CARD: &str = "card-owner-guard";
const TARGET: &str = "run-owner-target";
const FOREIGN: &str = "run-owner-foreign";
const PHASE: i64 = 3;
const GUARD_ROW_LOCK: &str = r"(?is)FROM\s+task_dispatches\b.*\bFOR\s+UPDATE";
const CANCEL_ROW_LOCK: &str = r"(?is)FROM\s+task_dispatches\b.*WHERE\s+id\s*=\s*\$1.*FOR\s+UPDATE";

enum AdvisoryKey<'a> {
    Run(&'a str),
    Phase(&'a str, i64),
}

struct Fixture {
    pg_db: TestPostgresDb,
    pool: PgPool,
    observer: PgConnection,
}

impl Fixture {
    async fn new() -> Self {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        let observer = PgConnection::connect(&pg_db.database_url)
            .await
            .expect("connect lock observer");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, 'Owner Guard Agent', 'claude', '123')",
        )
        .bind(AGENT)
        .execute(&pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, 'Owner Guard Card', 'in_progress', $2)",
        )
        .bind(CARD)
        .bind(AGENT)
        .execute(&pool)
        .await
        .expect("seed card");
        // The target run is older, so sorted token acquisition reaches it first.
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status, created_at)
             VALUES ($1, $3, 'active', NOW() - INTERVAL '1 hour'), ($2, $3, 'active', NOW())",
        )
        .bind(TARGET)
        .bind(FOREIGN)
        .bind(AGENT)
        .execute(&pool)
        .await
        .expect("seed runs");
        Self {
            pg_db,
            pool,
            observer,
        }
    }

    async fn dispatch(&self, id: &str, status: &str, context_run: Option<&str>) {
        let context = context_run.map(|run| format!(r#"{{"phase_gate":{{"run_id":"{run}"}}}}"#));
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context)
             VALUES ($1, $2, $3, 'implementation', $4, 'Owner Guard Dispatch', $5)",
        )
        .bind(id)
        .bind(CARD)
        .bind(AGENT)
        .bind(status)
        .bind(context)
        .execute(&self.pool)
        .await
        .expect("seed dispatch");
    }

    async fn gate(&self, run_id: &str, dispatch_id: &str) {
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (run_id, phase, status, dispatch_id)
             VALUES ($1, $2, 'pending', $3)",
        )
        .bind(run_id)
        .bind(PHASE)
        .bind(dispatch_id)
        .execute(&self.pool)
        .await
        .expect("seed phase gate");
    }

    async fn gate_rows(&mut self, run_id: &str, dispatch_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2 AND dispatch_id = $3",
        )
        .bind(run_id)
        .bind(PHASE)
        .bind(dispatch_id)
        .fetch_one(&mut self.observer)
        .await
        .expect("count phase-gate rows")
    }

    async fn dispatch_status(&self, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .expect("load dispatch status")
    }

    /// Live dispatches whose context names `run_id`, other than the seeded ones.
    async fn live_context_dispatches_besides(&self, run_id: &str, seeded: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM task_dispatches
             WHERE id <> $2
               AND status IN ('pending', 'dispatched')
               AND json_extract(context, '$.phase_gate.run_id') = $1",
        )
        .bind(run_id)
        .bind(seeded)
        .fetch_one(&self.pool)
        .await
        .expect("count live context dispatches")
    }

    /// Run the real cancellation body for `run_id` on a pool transaction and stop before commit.
    async fn hold_cancel(&self, run_id: &str) -> (Transaction<'static, Postgres>, i32) {
        let mut tx = self.pool.begin().await.expect("begin cancel holder");
        let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .expect("cancel holder pid");
        crate::services::auto_queue::cancel_run::terminalize_selected_runs_on_pg_tx_for_test(
            &mut tx,
            &[run_id.to_string()],
            "auto_queue_cancel",
        )
        .await
        .expect("run cancellation body");
        (tx, pid)
    }

    async fn close(self) {
        drop(self.observer);
        self.pool.close().await;
        self.pg_db.drop().await;
    }
}

async fn raw_holder(database_url: &str, statements: &[&str]) -> (PgConnection, i32) {
    let mut conn = PgConnection::connect(database_url)
        .await
        .expect("connect raw holder");
    sqlx::query("BEGIN")
        .execute(&mut conn)
        .await
        .expect("begin raw holder");
    for statement in statements {
        sqlx::query(sqlx::AssertSqlSafe(statement.to_string()))
            .execute(&mut conn)
            .await
            .unwrap_or_else(|error| panic!("raw holder {statement}: {error}"));
    }
    let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut conn)
        .await
        .expect("raw holder pid");
    (conn, pid)
}

async fn commit_raw(mut conn: PgConnection) {
    sqlx::query("COMMIT")
        .execute(&mut conn)
        .await
        .expect("commit raw holder");
}

async fn observer_pid(observer: &mut PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *observer)
        .await
        .expect("observer pid")
}

/// The single backend waiting on `key` behind exactly `holder_pid`. Zero waiters by the
/// deadline, extra waiters or holders, a foreign blocker or a finished writer all fail.
async fn wait_for_exact_advisory_waiter<T>(
    observer: &mut PgConnection,
    key: AdvisoryKey<'_>,
    holder_pid: i32,
    writer: &JoinHandle<T>,
) -> Result<i32, String> {
    let observer_pid = observer_pid(observer).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut *observer)
            .await
            .expect("clear stat snapshot");
        let base = "SELECT l.pid, l.granted FROM pg_locks l
             WHERE l.locktype = 'advisory'
               AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())";
        let rows: Vec<(i32, bool)> = match key {
            AdvisoryKey::Run(run_id) => sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{base} AND l.objsubid = 1
                   AND l.classid::bigint = ((hashtext('aq_run:' || $1)::bigint >> 32) & 4294967295)
                   AND l.objid::bigint = (hashtext('aq_run:' || $1)::bigint & 4294967295)"
            )))
            .bind(run_id)
            .fetch_all(&mut *observer)
            .await
            .expect("inspect run-token locks"),
            AdvisoryKey::Phase(run_id, phase) => sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{base} AND l.objsubid = 2
                   AND l.classid::bigint = (hashtext($1)::bigint & 4294967295)
                   AND l.objid::bigint = (hashtext($2::bigint::text)::bigint & 4294967295)"
            )))
            .bind(run_id)
            .bind(phase)
            .fetch_all(&mut *observer)
            .await
            .expect("inspect phase-key locks"),
        };
        let granted: Vec<i32> = rows.iter().filter(|row| row.1).map(|row| row.0).collect();
        let waiting: Vec<i32> = rows.iter().filter(|row| !row.1).map(|row| row.0).collect();
        if granted != [holder_pid] {
            return Err(format!("key held by {granted:?}, expected [{holder_pid}]"));
        }
        if waiting.len() > 1 {
            return Err(format!("extra key waiters {waiting:?}"));
        }
        if let [waiter] = waiting[..] {
            if waiter == holder_pid || waiter == observer_pid {
                return Err(format!("waiter {waiter} is the holder or the observer"));
            }
            let blockers: Vec<i32> = sqlx::query_scalar("SELECT pg_blocking_pids($1)")
                .bind(waiter)
                .fetch_one(&mut *observer)
                .await
                .expect("load key waiter blockers");
            if blockers != [holder_pid] {
                return Err(format!("key waiter {waiter} blocked by {blockers:?}"));
            }
            if writer.is_finished() {
                return Err("writer finished while a key waiter was listed".to_string());
            }
            return Ok(waiter);
        }
        if writer.is_finished() {
            return Err("writer finished without waiting on the advisory key".to_string());
        }
        if Instant::now() >= deadline {
            return Err("no advisory waiter within 5s".to_string());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The single backend whose statement matches `statement`, blocked by exactly `holder_pid`
/// and holding the tuple lock on the committed version of `dispatch_id`.
async fn wait_for_exact_row_waiter<T>(
    observer: &mut PgConnection,
    dispatch_id: &str,
    statement: &str,
    holder_pid: i32,
    writer: &JoinHandle<T>,
) -> Result<i32, String> {
    let statement = Regex::new(statement).expect("statement pattern");
    let observer_pid = observer_pid(observer).await;
    let (page, tuple): (i64, i64) = sqlx::query_as(
        "SELECT (ctid::text::point)[0]::bigint, (ctid::text::point)[1]::bigint
         FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_one(&mut *observer)
    .await
    .expect("locate dispatch tuple");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut *observer)
            .await
            .expect("clear stat snapshot");
        let candidates: Vec<(i32, String, Vec<i32>)> = sqlx::query_as(
            "SELECT a.pid, COALESCE(a.query, ''), pg_blocking_pids(a.pid)
             FROM pg_stat_activity a
             WHERE a.datname = current_database()
               AND a.pid <> pg_backend_pid()
               AND a.wait_event_type = 'Lock'
               AND a.wait_event IN ('transactionid', 'tuple')",
        )
        .fetch_all(&mut *observer)
        .await
        .expect("inspect row waiters");
        let matching: Vec<_> = candidates
            .into_iter()
            .filter(|(_, query, _)| statement.is_match(query))
            .collect();
        if matching.len() > 1 {
            return Err(format!("extra row waiters {matching:?}"));
        }
        if let [(waiter, _, blockers)] = &matching[..] {
            if *waiter == holder_pid || *waiter == observer_pid {
                return Err(format!("row waiter {waiter} is the holder or the observer"));
            }
            if blockers[..] != [holder_pid] {
                return Err(format!("row waiter {waiter} blocked by {blockers:?}"));
            }
            let tuple_locks: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT FROM pg_locks l
                 WHERE l.pid = $1 AND l.locktype = 'tuple' AND l.granted
                   AND l.relation = 'task_dispatches'::regclass
                   AND l.page = $2 AND l.tuple = $3",
            )
            .bind(waiter)
            .bind(page as i32)
            .bind(tuple as i16)
            .fetch_one(&mut *observer)
            .await
            .expect("inspect row waiter tuple lock");
            if tuple_locks != 1 {
                return Err(format!(
                    "row waiter {waiter} holds no tuple lock on {dispatch_id}"
                ));
            }
            if writer.is_finished() {
                return Err("writer finished while a row waiter was listed".to_string());
            }
            return Ok(*waiter);
        }
        if writer.is_finished() {
            return Err("writer finished without a matching row wait".to_string());
        }
        if Instant::now() >= deadline {
            return Err("no matching row waiter within 5s".to_string());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn gate_state(dispatch_ids: &[&str]) -> PhaseGateStateWrite {
    PhaseGateStateWrite {
        status: "pending".to_string(),
        dispatch_ids: dispatch_ids.iter().map(ToString::to_string).collect(),
        ..PhaseGateStateWrite::default()
    }
}

fn spawn_save(
    pool: &PgPool,
    run_id: &'static str,
    dispatch_id: &'static str,
) -> JoinHandle<Result<super::PhaseGateSaveResult, String>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        super::save_phase_gate_state_on_pg(&pool, run_id, PHASE, &gate_state(&[dispatch_id])).await
    })
}

/// Force-pause cleanup cancels under the run token, so it waits for a token holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_pause_cleanup_waits_for_run_token_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-pause", "pending", Some(TARGET)).await;
    let token = format!("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || '{TARGET}'))");
    let (holder, holder_pid) = raw_holder(&fx.pg_db.database_url, &[&token]).await;

    let pool = fx.pool.clone();
    let writer = tokio::spawn(async move {
        crate::services::auto_queue::cancel_run::cancel_live_dispatches_for_runs_pg(
            &pool,
            &[TARGET.to_string()],
            "force_pause",
        )
        .await
    });
    let waited = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Run(TARGET),
        holder_pid,
        &writer,
    )
    .await;
    commit_raw(holder).await;
    let cancelled = writer.await.expect("join cleanup").expect("cleanup cancel");

    assert_eq!(cancelled.dispatch_ids, vec!["d-pause".to_string()]);
    assert_eq!(
        fx.dispatch_status("d-pause").await.as_deref(),
        Some("cancelled")
    );
    waited.expect("cleanup must wait on the run token");
    fx.close().await;
}

/// Moving a gate from the cancelling run to another run waits for the cancel commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase_gate_transfer_target_to_foreign_serializes_with_cancel_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-move", "pending", None).await;
    fx.gate(TARGET, "d-move").await;
    let (holder, holder_pid) = fx.hold_cancel(TARGET).await;

    let writer = spawn_save(&fx.pool, FOREIGN, "d-move");
    let waited = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Run(TARGET),
        holder_pid,
        &writer,
    )
    .await;
    let foreign_rows_before_commit = fx.gate_rows(FOREIGN, "d-move").await;
    let finished_before_commit = writer.is_finished();
    holder.commit().await.expect("commit cancel holder");
    let saved = writer.await.expect("join save").expect("foreign save");

    assert_eq!(foreign_rows_before_commit, 0);
    assert!(
        !finished_before_commit,
        "save finished before the cancel commit"
    );
    assert_eq!(saved.persisted_dispatch_ids, vec!["d-move".to_string()]);
    assert_eq!(fx.gate_rows(FOREIGN, "d-move").await, 1);
    assert_eq!(
        fx.dispatch_status("d-move").await.as_deref(),
        Some("cancelled")
    );
    let cancel_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM dispatch_events
         WHERE dispatch_id = 'd-move' AND to_status = 'cancelled'",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("count cancel events");
    assert_eq!(cancel_events, 1);
    waited.expect("save must wait on the source run token");
    fx.close().await;
}

/// Moving a live gate into a run that is being cancelled is refused after the cancel commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase_gate_transfer_foreign_to_target_refused_after_cancel_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-steal", "pending", None).await;
    fx.gate(FOREIGN, "d-steal").await;
    let (holder, holder_pid) = fx.hold_cancel(TARGET).await;

    let writer = spawn_save(&fx.pool, TARGET, "d-steal");
    let waited = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Run(TARGET),
        holder_pid,
        &writer,
    )
    .await;
    holder.commit().await.expect("commit cancel holder");
    let saved = writer.await.expect("join save");

    assert!(saved.is_err(), "cancelled run took a live gate: {saved:?}");
    assert_eq!(
        fx.dispatch_status("d-steal").await.as_deref(),
        Some("pending")
    );
    assert_eq!(fx.gate_rows(FOREIGN, "d-steal").await, 1);
    assert_eq!(fx.gate_rows(TARGET, "d-steal").await, 0);
    waited.expect("save must wait on the target run token");
    let error = saved.expect_err("refusal");
    assert!(error.contains("is not live"), "{error}");
    fx.close().await;
}

/// A phase-gate context dispatch for a run being cancelled is refused after the cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase_gate_context_create_refused_after_cancel_pg() {
    let mut fx = Fixture::new().await;
    let (holder, holder_pid) = fx.hold_cancel(TARGET).await;

    let pool = fx.pool.clone();
    let writer = tokio::spawn(async move {
        crate::dispatch::create_dispatch_core_with_id_and_options(
            &pool,
            "d-create",
            CARD,
            AGENT,
            "phase-gate",
            "Owner Guard Create",
            &serde_json::json!({"phase_gate": {"run_id": TARGET, "batch_phase": PHASE}}),
            crate::dispatch::DispatchCreateOptions::default(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let waited = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Run(TARGET),
        holder_pid,
        &writer,
    )
    .await;
    holder.commit().await.expect("commit cancel holder");
    let created = writer.await.expect("join create");

    assert!(
        created.is_err(),
        "cancelled run gained a live dispatch: {created:?}"
    );
    assert_eq!(fx.dispatch_status("d-create").await, None);
    waited.expect("create must wait on the context run token");
    let error = created.expect_err("refusal");
    assert!(
        error.contains("refusing to publish a live dispatch owner"),
        "{error}"
    );
    fx.close().await;
}

fn retry_meta(origin: &str) -> crate::db::dispatched_sessions::RetryDispatchMeta {
    crate::db::dispatched_sessions::RetryDispatchMeta {
        origin_dispatch_id: origin.to_string(),
        card_id: CARD.to_string(),
        to_agent_id: Some(AGENT.to_string()),
        dispatch_type: Some("implementation".to_string()),
        title: Some("Owner Guard Retry".to_string()),
        context: Some(format!(r#"{{"phase_gate":{{"run_id":"{TARGET}"}}}}"#)),
        retry_count: 0,
    }
}

fn spawn_retry(pool: &PgPool, origin: &'static str) -> JoinHandle<Result<String, String>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        crate::db::dispatched_sessions::create_retry_dispatch_pg(&pool, &retry_meta(origin)).await
    })
}

/// A context-only retry of an origin a foreign gate keeps live cannot escape the cancel. The
/// cancel row-locks the vetoed origin (main did not), and the retry waits on the run token first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_context_only_retry_cannot_escape_cancel_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-origin-veto", "pending", Some(TARGET)).await;
    fx.gate(FOREIGN, "d-origin-veto").await;
    let (holder, holder_pid) = fx.hold_cancel(TARGET).await;

    let writer = spawn_retry(&fx.pool, "d-origin-veto");
    let waited = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Run(TARGET),
        holder_pid,
        &writer,
    )
    .await;
    holder.commit().await.expect("commit cancel holder");
    let retried = writer.await.expect("join retry");

    assert!(retried.is_err(), "retry escaped the cancel: {retried:?}");
    assert_eq!(
        fx.live_context_dispatches_besides(TARGET, "d-origin-veto")
            .await,
        0
    );
    assert_eq!(
        fx.dispatch_status("d-origin-veto").await.as_deref(),
        Some("pending")
    );
    waited.expect("retry must wait on the context run token");
    fx.close().await;
}

/// A context-only retry of a dispatch the cancel is terminalizing waits on the run token,
/// not on the origin row the cancel holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_context_only_retry_waits_on_run_token_not_origin_row_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-origin", "pending", Some(TARGET)).await;
    let (holder, holder_pid) = fx.hold_cancel(TARGET).await;

    let writer = spawn_retry(&fx.pool, "d-origin");
    let waited = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Run(TARGET),
        holder_pid,
        &writer,
    )
    .await;
    holder.commit().await.expect("commit cancel holder");
    let retried = writer.await.expect("join retry");

    assert!(
        retried.is_err(),
        "retry of a cancelled dispatch succeeded: {retried:?}"
    );
    assert_eq!(
        fx.live_context_dispatches_besides(TARGET, "d-origin").await,
        0
    );
    assert_eq!(
        fx.dispatch_status("d-origin").await.as_deref(),
        Some("cancelled")
    );
    waited.expect("retry must wait on the run token, not the origin row");
    let error = retried.expect_err("refusal");
    assert!(
        error.contains("refusing to publish a live dispatch owner"),
        "{error}"
    );
    fx.close().await;
}

/// Cancellation re-reads a dispatch that a claim moved to `dispatched` while it waited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_rereads_dispatch_claimed_mid_cancel_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-claim", "pending", Some(TARGET)).await;
    let (holder, holder_pid) = raw_holder(
        &fx.pg_db.database_url,
        &[
            "SELECT id FROM task_dispatches WHERE id = 'd-claim' FOR UPDATE",
            "UPDATE task_dispatches SET status = 'dispatched' WHERE id = 'd-claim'",
        ],
    )
    .await;

    let pool = fx.pool.clone();
    let writer = tokio::spawn(async move {
        crate::services::auto_queue::cancel_run::cancel_selected_runs_with_pg(
            None,
            &pool,
            &[TARGET.to_string()],
            "auto_queue_cancel",
        )
        .await
    });
    let waited = wait_for_exact_row_waiter(
        &mut fx.observer,
        "d-claim",
        CANCEL_ROW_LOCK,
        holder_pid,
        &writer,
    )
    .await;
    commit_raw(holder).await;
    let response = writer.await.expect("join cancel").expect("cancel run");

    assert_eq!(
        fx.dispatch_status("d-claim").await.as_deref(),
        Some("cancelled")
    );
    assert_eq!(response["remaining_live_dispatches"], serde_json::json!(0));
    waited.expect("cancel must wait on the dispatch row lock");
    fx.close().await;
}

/// Two saves claiming the same unowned dispatch serialize on its row: the later one sees
/// the committed owner after the wait and refuses instead of taking the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_phase_gate_saves_serialize_on_dispatch_row_pg() {
    let mut fx = Fixture::new().await;
    fx.dispatch("d-race", "pending", None).await;
    let key = format!("SELECT pg_advisory_xact_lock(hashtext('{TARGET}'), hashtext('{PHASE}'))");
    let (key_holder, key_holder_pid) = raw_holder(&fx.pg_db.database_url, &[&key]).await;

    let target_save = spawn_save(&fx.pool, TARGET, "d-race");
    let target_pid = wait_for_exact_advisory_waiter(
        &mut fx.observer,
        AdvisoryKey::Phase(TARGET, PHASE),
        key_holder_pid,
        &target_save,
    )
    .await;
    let foreign_save = spawn_save(&fx.pool, FOREIGN, "d-race");
    let row_wait = match target_pid {
        Ok(target_pid) => {
            wait_for_exact_row_waiter(
                &mut fx.observer,
                "d-race",
                GUARD_ROW_LOCK,
                target_pid,
                &foreign_save,
            )
            .await
        }
        Err(ref error) => Err(format!("target save never reached the phase key: {error}")),
    };
    commit_raw(key_holder).await;
    let target_saved = target_save.await.expect("join target save");
    let foreign_saved = foreign_save.await.expect("join foreign save");

    assert!(target_saved.is_ok(), "{target_saved:?}");
    assert!(
        foreign_saved.is_err(),
        "later save stole the gate: {foreign_saved:?}"
    );
    assert_eq!(fx.gate_rows(TARGET, "d-race").await, 1);
    assert_eq!(fx.gate_rows(FOREIGN, "d-race").await, 0);
    target_pid.expect("target save must wait on its phase key");
    row_wait.expect("foreign save must wait on the dispatch row");
    let error = foreign_saved.expect_err("refusal");
    assert!(error.contains("outside the locked set"), "{error}");
    fx.close().await;
}
