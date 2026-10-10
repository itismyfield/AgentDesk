use super::{BackgroundChildSpawn, close_background_child_pg, insert_background_child_pg};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::upsert_hook_session_with_identity_pg;
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::session_status::{AWAITING_BG, IDLE, normalize_session_status};
use crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied;
use sqlx::{PgPool, Row};
use std::time::Duration;

async fn setup() -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(6).await;
    (db, pool)
}

async fn seed_parent(pool: &PgPool, key: &str, status: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status, active_turn_nonce)
         VALUES ($1, 'claude', $2, 'ordering-turn') RETURNING id",
    )
    .bind(key)
    .bind(status)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn spawn(key: &str, tool_use_id: &str) -> BackgroundChildSpawn {
    BackgroundChildSpawn {
        parent_session_key: key.into(),
        provider: Some("claude".into()),
        tool_name: "Task".into(),
        tool_input: "{}".into(),
        tool_use_id: Some(tool_use_id.into()),
    }
}

fn terminal_params(key: &str) -> HookSessionUpsert<'_> {
    HookSessionUpsert {
        session_key: key,
        instance_id: None,
        agent_id: None,
        provider: "claude",
        status: AWAITING_BG,
        session_info: None,
        model: None,
        tokens: None,
        cwd: None,
        active_dispatch_id: None,
        thread_channel_id: None,
        channel_id: None,
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: Some("ordering-turn"),
        dispatched_origin: false,
    }
}

async fn parent_state(pool: &PgPool, key: &str) -> (i32, String) {
    let row = sqlx::query("SELECT active_children, status FROM sessions WHERE session_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap();
    (row.get("active_children"), row.get("status"))
}

async fn assert_idle(pool: &PgPool, key: &str) {
    let (children, status) = parent_state(pool, key).await;
    assert_eq!(children, 0);
    assert_eq!(status, IDLE, "last child must release awaiting_bg");
    assert!(idle_cleanup_session_is_unoccupied(pool, key).await);
}

async fn assert_occupied(pool: &PgPool, key: &str) {
    let (children, status) = parent_state(pool, key).await;
    assert_eq!(
        children, 1,
        "the newly registered child must remain counted"
    );
    assert_eq!(
        normalize_session_status(Some(&status), children),
        AWAITING_BG
    );
    assert!(!idle_cleanup_session_is_unoccupied(pool, key).await);
}

// The trigger pauses after the mutation has locked its parent row. Waiting on
// PostgreSQL's lock graph fixes the interleaving without scheduler sleeps.
async fn install_parent_update_gate(pool: &PgPool, parent_id: i64, old_count: i32) {
    sqlx::query(
        "CREATE FUNCTION background_parent_gate() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF OLD.id = TG_ARGV[0]::bigint
                AND OLD.active_children = TG_ARGV[1]::integer
                AND NEW.active_children = OLD.active_children - 1 THEN
                 PERFORM pg_advisory_xact_lock(669103::bigint);
             END IF;
             RETURN NEW;
         END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER background_parent_gate BEFORE UPDATE ON sessions
         FOR EACH ROW EXECUTE FUNCTION background_parent_gate('{parent_id}', '{old_count}')"
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn install_child_insert_gate(pool: &PgPool, parent_id: i64) {
    sqlx::query(
        "CREATE FUNCTION background_insert_gate() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.parent_session_id = TG_ARGV[0]::bigint THEN
                 PERFORM pg_advisory_xact_lock(669103::bigint);
             END IF;
             RETURN NEW;
         END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER background_insert_gate BEFORE INSERT ON sessions
         FOR EACH ROW EXECUTE FUNCTION background_insert_gate('{parent_id}')"
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn install_terminal_update_gate(pool: &PgPool, parent_id: i64) {
    sqlx::query(
        "CREATE FUNCTION background_terminal_gate() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF OLD.id = TG_ARGV[0]::bigint
                AND OLD.status = 'turn_active' AND NEW.status = 'awaiting_bg' THEN
                 PERFORM pg_advisory_xact_lock(669103::bigint);
             END IF;
             RETURN NEW;
         END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER background_terminal_gate BEFORE UPDATE ON sessions
         FOR EACH ROW EXECUTE FUNCTION background_terminal_gate('{parent_id}')"
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn hold_gate(pool: &PgPool) -> sqlx::Transaction<'_, sqlx::Postgres> {
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(669103::bigint)")
        .execute(&mut *gate)
        .await
        .unwrap();
    gate
}

async fn await_gate_waiter(pool: &PgPool) -> i32 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pid: Option<i32> = sqlx::query_scalar(
                "SELECT pid FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND wait_event = 'advisory' AND pid <> pg_backend_pid()",
            )
            .fetch_optional(pool)
            .await
            .unwrap();
            if let Some(pid) = pid {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mutation did not reach the parent gate")
}

async fn await_parent_waiter(pool: &PgPool, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_stat_activity
                      WHERE datname = current_database() AND wait_event_type = 'Lock'
                        AND pid <> $1 AND $1 = ANY(pg_blocking_pids(pid))
                        AND (query LIKE '%FROM sessions%FOR%UPDATE%'
                             OR query LIKE '%GREATEST(active_children - 1, 0)%')
                 )",
            )
            .bind(blocker)
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("second mutation did not wait on the locked parent");
}

#[tokio::test]
async fn background_sibling_closes_refresh_snapshot_after_parent_lock_pg() {
    let (db, pool) = setup().await;
    let key = "ordering-sibling-closes";
    let parent_id = seed_parent(&pool, key, AWAITING_BG).await;
    let first = insert_background_child_pg(&pool, &spawn(key, "first"))
        .await
        .unwrap()
        .unwrap();
    let second = insert_background_child_pg(&pool, &spawn(key, "second"))
        .await
        .unwrap()
        .unwrap();
    install_parent_update_gate(&pool, parent_id, 2).await;
    let gate = hold_gate(&pool).await;
    let first_close = tokio::spawn({
        let pool = pool.clone();
        async move { close_background_child_pg(&pool, first, IDLE).await }
    });
    let first_pid = await_gate_waiter(&pool).await;
    let second_close = tokio::spawn({
        let pool = pool.clone();
        async move { close_background_child_pg(&pool, second, IDLE).await }
    });
    await_parent_waiter(&pool, first_pid).await;
    gate.commit().await.unwrap();
    assert!(first_close.await.unwrap().unwrap());
    assert!(second_close.await.unwrap().unwrap());
    assert_idle(&pool, key).await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn background_insert_before_last_close_preserves_new_child_pg() {
    let (db, pool) = setup().await;
    let key = "ordering-insert-before-close";
    let parent_id = seed_parent(&pool, key, AWAITING_BG).await;
    let old_child = insert_background_child_pg(&pool, &spawn(key, "old"))
        .await
        .unwrap()
        .unwrap();
    install_child_insert_gate(&pool, parent_id).await;
    let gate = hold_gate(&pool).await;
    let insert = tokio::spawn({
        let pool = pool.clone();
        async move { insert_background_child_pg(&pool, &spawn(key, "new")).await }
    });
    let insert_pid = await_gate_waiter(&pool).await;
    let close = tokio::spawn({
        let pool = pool.clone();
        async move { close_background_child_pg(&pool, old_child, IDLE).await }
    });
    await_parent_waiter(&pool, insert_pid).await;
    gate.commit().await.unwrap();
    let new_child = insert.await.unwrap().unwrap().unwrap();
    assert!(close.await.unwrap().unwrap());
    assert_occupied(&pool, key).await;
    assert!(
        close_background_child_pg(&pool, new_child, IDLE)
            .await
            .unwrap()
    );
    assert_idle(&pool, key).await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn background_last_close_before_insert_preserves_new_child_pg() {
    let (db, pool) = setup().await;
    let key = "ordering-close-before-insert";
    let parent_id = seed_parent(&pool, key, AWAITING_BG).await;
    let old_child = insert_background_child_pg(&pool, &spawn(key, "old"))
        .await
        .unwrap()
        .unwrap();
    install_parent_update_gate(&pool, parent_id, 1).await;
    let gate = hold_gate(&pool).await;
    let close = tokio::spawn({
        let pool = pool.clone();
        async move { close_background_child_pg(&pool, old_child, IDLE).await }
    });
    let close_pid = await_gate_waiter(&pool).await;
    let insert = tokio::spawn({
        let pool = pool.clone();
        async move { insert_background_child_pg(&pool, &spawn(key, "new")).await }
    });
    await_parent_waiter(&pool, close_pid).await;
    gate.commit().await.unwrap();
    assert!(close.await.unwrap().unwrap());
    let new_child = insert.await.unwrap().unwrap().unwrap();
    assert_occupied(&pool, key).await;
    assert!(
        close_background_child_pg(&pool, new_child, IDLE)
            .await
            .unwrap()
    );
    assert_idle(&pool, key).await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn background_last_close_before_final_upsert_uses_committed_snapshot_pg() {
    let (db, pool) = setup().await;
    let key = "ordering-close-before-terminal";
    let parent_id = seed_parent(&pool, key, "turn_active").await;
    let child = insert_background_child_pg(&pool, &spawn(key, "child"))
        .await
        .unwrap()
        .unwrap();
    install_parent_update_gate(&pool, parent_id, 1).await;
    let gate = hold_gate(&pool).await;
    let close = tokio::spawn({
        let pool = pool.clone();
        async move { close_background_child_pg(&pool, child, IDLE).await }
    });
    let close_pid = await_gate_waiter(&pool).await;
    let terminal = tokio::spawn({
        let pool = pool.clone();
        async move { upsert_hook_session_with_identity_pg(&pool, terminal_params(key), None).await }
    });
    await_parent_waiter(&pool, close_pid).await;
    gate.commit().await.unwrap();
    assert!(close.await.unwrap().unwrap());
    terminal.await.unwrap().unwrap();
    assert_idle(&pool, key).await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn background_final_upsert_before_last_close_releases_occupancy_pg() {
    let (db, pool) = setup().await;
    let key = "ordering-terminal-before-close";
    let parent_id = seed_parent(&pool, key, "turn_active").await;
    let child = insert_background_child_pg(&pool, &spawn(key, "child"))
        .await
        .unwrap()
        .unwrap();
    install_terminal_update_gate(&pool, parent_id).await;
    let gate = hold_gate(&pool).await;
    let terminal = tokio::spawn({
        let pool = pool.clone();
        async move { upsert_hook_session_with_identity_pg(&pool, terminal_params(key), None).await }
    });
    let terminal_pid = await_gate_waiter(&pool).await;
    let close = tokio::spawn({
        let pool = pool.clone();
        async move { close_background_child_pg(&pool, child, IDLE).await }
    });
    await_parent_waiter(&pool, terminal_pid).await;
    gate.commit().await.unwrap();
    terminal.await.unwrap().unwrap();
    assert!(close.await.unwrap().unwrap());
    assert_idle(&pool, key).await;
    pool.close().await;
    db.drop().await;
}
