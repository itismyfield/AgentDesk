//! Moved from src/db/auto_queue/entries.rs: these cases write entry status through the KE writer.

use crate::db::auto_queue::entries::test_access::load_entry_status_row_pg_for_test as load_entry_status_row_pg;
use crate::db::auto_queue::entries::tests::{
    begin_run_token_holder, entry_status, run_token_waiter_sample, seed_dispatch, setup_entry,
    transition_count,
};
use crate::db::auto_queue::entries::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::transactions::{
    update_entry_status_on_pg, update_entry_status_on_pg_tx,
};
use crate::test_support::wait::poll_within;

#[tokio::test]
async fn cancelled_run_rejects_dispatched_tx_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "cancelled", ENTRY_STATUS_PENDING).await;

    let mut tx = pool.begin().await.expect("begin entry update tx");
    let error = update_entry_status_on_pg_tx(
        &mut tx,
        "gate-entry",
        ENTRY_STATUS_DISPATCHED,
        "test_tx_gate",
        &EntryStatusUpdateOptions::default(),
    )
    .await
    .expect_err("cancelled run must reject tx-scoped attach");
    assert!(error.contains("gate-run is cancelled"), "{error}");
    tx.rollback().await.expect("rollback rejected tx attach");
    assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancelled_run_rejects_dispatched_pool_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "cancelled", ENTRY_STATUS_PENDING).await;

    let error = update_entry_status_on_pg(
        &pool,
        "gate-entry",
        ENTRY_STATUS_DISPATCHED,
        "test_pool_gate",
        &EntryStatusUpdateOptions::default(),
    )
    .await
    .expect_err("cancelled run must reject pool-scoped attach");
    assert!(error.contains("gate-run is cancelled"), "{error}");
    assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);

    pool.close().await;
    pg_db.drop().await;
}

async fn assert_terminal_transition_does_not_wait(initial_status: &str, terminal_status: &str) {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "cancelled", initial_status).await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        update_entry_status_on_pg(
            &pool,
            "gate-entry",
            terminal_status,
            "test_terminal_arm",
            &EntryStatusUpdateOptions::default(),
        ),
    )
    .await
    .expect("terminal arm must not wait for the dispatched gate")
    .expect("terminalize cancelled-run entry");
    assert!(result.changed);
    assert_eq!(entry_status(&pool).await, terminal_status);
    sqlx::query("COMMIT")
        .execute(&mut token_holder)
        .await
        .expect("release gate run token");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancelled_run_allows_skipped_without_waiting_for_gate_pg() {
    assert_terminal_transition_does_not_wait(ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED).await;
}

#[tokio::test]
async fn cancelled_run_allows_failed_without_waiting_for_gate_pg() {
    assert_terminal_transition_does_not_wait(ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_FAILED).await;
}

#[tokio::test]
async fn cancelled_run_allows_cancelled_without_waiting_for_gate_pg() {
    assert_terminal_transition_does_not_wait(ENTRY_STATUS_PENDING, ENTRY_STATUS_USER_CANCELLED)
        .await;
}

#[tokio::test]
async fn cancelled_run_allows_done_without_waiting_for_gate_pg() {
    assert_terminal_transition_does_not_wait(ENTRY_STATUS_PENDING, ENTRY_STATUS_DONE).await;
}

async fn assert_live_run_allows_dispatched_transition(run_status: &str, dispatch_id: &str) {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, run_status, ENTRY_STATUS_PENDING).await;
    seed_dispatch(&pool, dispatch_id).await;

    let result = update_entry_status_on_pg(
        &pool,
        "gate-entry",
        ENTRY_STATUS_DISPATCHED,
        "test_live_gate",
        &EntryStatusUpdateOptions {
            dispatch_id: Some(dispatch_id.to_string()),
            slot_index: Some(0),
        },
    )
    .await
    .expect("live run remains eligible for attachment");
    assert!(result.changed);
    assert_eq!(entry_status(&pool).await, ENTRY_STATUS_DISPATCHED);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn active_run_allows_dispatched_transition_pg() {
    assert_live_run_allows_dispatched_transition("active", "dispatch-active").await;
}

#[tokio::test]
async fn paused_run_allows_dispatched_transition_pg() {
    assert_live_run_allows_dispatched_transition("paused", "dispatch-paused").await;
}

#[tokio::test]
async fn restoring_run_allows_dispatched_transition_pg() {
    assert_live_run_allows_dispatched_transition("restoring", "dispatch-restoring").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_status_retry_rejects_competing_dispatch_identity_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
    seed_dispatch(&pool, "dispatch-requested-status-update").await;
    seed_dispatch(&pool, "dispatch-competing-status-update").await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

    let update_pool = pool.clone();
    let update = tokio::spawn(async move {
        update_entry_status_on_pg(
            &update_pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "test_pool_status_retry",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-requested-status-update".to_string()),
                slot_index: Some(0),
            },
        )
        .await
    });
    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'dispatched',
             dispatch_id = 'dispatch-competing-status-update',
             slot_index = 1,
             completed_at = NULL
         WHERE id = 'gate-entry'",
    )
    .execute(&mut token_holder)
    .await
    .expect("attach competing dispatch during pool status update");
    sqlx::query("COMMIT")
        .execute(&mut token_holder)
        .await
        .expect("commit competing dispatch and release run token");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), update)
        .await
        .expect("pool status update finishes after competing attachment")
        .expect("join pool status update")
        .expect_err("pool status retry must reject a competing dispatch identity");
    assert!(
        error.contains("dispatch identity changed during status update"),
        "{error}"
    );
    let identity = load_entry_status_row_pg(&pool, "gate-entry")
        .await
        .expect("load preserved competing dispatch identity");
    assert_eq!(
        identity.dispatch_id_for_test(),
        Some("dispatch-competing-status-update")
    );
    assert_eq!(identity.slot_index_for_test(), Some(1));

    drop(token_holder);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_status_retry_distinguishes_null_and_empty_dispatch_identity_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

    let update_pool = pool.clone();
    let update = tokio::spawn(async move {
        update_entry_status_on_pg(
            &update_pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "test_pool_status_retry",
            &EntryStatusUpdateOptions::default(),
        )
        .await
    });
    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'dispatched',
             dispatch_id = '',
             completed_at = NULL
         WHERE id = 'gate-entry'",
    )
    .execute(&mut token_holder)
    .await
    .expect("replace NULL dispatch identity with empty text");
    sqlx::query("COMMIT")
        .execute(&mut token_holder)
        .await
        .expect("commit empty dispatch identity and release run token");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), update)
        .await
        .expect("pool status update finishes after empty identity wins")
        .expect("join pool status update")
        .expect_err("pool status retry must not accept empty text as the pinned NULL identity");
    assert!(
        error.contains("dispatch identity changed during status update"),
        "{error}"
    );
    let identity = load_entry_status_row_pg(&pool, "gate-entry")
        .await
        .expect("load preserved empty dispatch identity");
    assert_eq!(identity.dispatch_id_for_test(), Some(""));

    drop(token_holder);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_status_retry_absorbs_competing_status_change_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
    seed_dispatch(&pool, "dispatch-requested-status-race").await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

    let update_pool = pool.clone();
    let update = tokio::spawn(async move {
        update_entry_status_on_pg(
            &update_pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "test_pool_status_retry",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-requested-status-race".to_string()),
                slot_index: Some(0),
            },
        )
        .await
    });
    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    // Only the status moves; the row keeps the NULL dispatch identity this
    // attempt observed, so the retry must converge instead of aborting.
    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'skipped',
             completed_at = NOW()
         WHERE id = 'gate-entry'",
    )
    .execute(&mut token_holder)
    .await
    .expect("race the status only during pool status update");
    sqlx::query("COMMIT")
        .execute(&mut token_holder)
        .await
        .expect("commit racing status and release run token");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), update)
        .await
        .expect("pool status update finishes after the racing status commits")
        .expect("join pool status update")
        .expect("pool status retry must absorb a pure status race");
    assert!(result.changed);
    assert_eq!(result.from_status, ENTRY_STATUS_SKIPPED);
    let identity = load_entry_status_row_pg(&pool, "gate-entry")
        .await
        .expect("load attached dispatch identity");
    assert_eq!(identity.status_for_test(), ENTRY_STATUS_DISPATCHED);
    assert_eq!(
        identity.dispatch_id_for_test(),
        Some("dispatch-requested-status-race")
    );
    assert_eq!(identity.slot_index_for_test(), Some(0));

    drop(token_holder);
    pool.close().await;
    pg_db.drop().await;
}

struct RacedDoneAttachment {
    result: Result<EntryStatusUpdateResult, String>,
    row: EntryStatusRow,
    transitions: Vec<(String, String, String)>,
    dispatch_history: Vec<String>,
}

/// Stages a new attachment in flight while a competitor moves the entry to `done` without touching
/// the observed dispatch identity; judging drift against the observed pre-state lets the retry converge.
async fn race_entry_to_done_during_new_attachment(
    trigger_source: &str,
    requested_dispatch_status: &str,
) -> RacedDoneAttachment {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
    seed_dispatch(&pool, "dispatch-requested-done-race").await;
    sqlx::query(
        "UPDATE task_dispatches
         SET status = $1, updated_at = NOW()
         WHERE id = 'dispatch-requested-done-race'",
    )
    .bind(requested_dispatch_status)
    .execute(&pool)
    .await
    .expect("seed requested dispatch status");
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

    let trigger_source = trigger_source.to_string();
    let update_pool = pool.clone();
    let update = tokio::spawn(async move {
        update_entry_status_on_pg(
            &update_pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            &trigger_source,
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-requested-done-race".to_string()),
                slot_index: Some(0),
            },
        )
        .await
    });
    // Waiting here is the run-token gate itself: the dispatched arm cannot
    // reach its CAS until this holder commits, so the racing `done` is
    // guaranteed to land between the attempt's reload and its write.
    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'done',
             completed_at = NOW()
         WHERE id = 'gate-entry'",
    )
    .execute(&mut token_holder)
    .await
    .expect("race the entry to done during the new attachment");
    sqlx::query("COMMIT")
        .execute(&mut token_holder)
        .await
        .expect("commit racing done status and release run token");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), update)
        .await
        .expect("pool status update finishes after the racing done commits")
        .expect("join pool status update");
    let row = load_entry_status_row_pg(&pool, "gate-entry")
        .await
        .expect("load entry state after the racing done");
    let transitions = sqlx::query_as::<_, (String, String, String)>(
        "SELECT from_status, to_status, trigger_source
         FROM auto_queue_entry_transitions
         WHERE entry_id = 'gate-entry'
         ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .expect("load transitions recorded during the reattachment race");
    let dispatch_history = list_entry_dispatch_history_pg(&pool, "gate-entry")
        .await
        .expect("load dispatch history recorded during the reattachment race");

    drop(token_holder);
    pool.close().await;
    pg_db.drop().await;
    RacedDoneAttachment {
        result,
        row,
        transitions,
        dispatch_history,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_status_retry_reattaches_after_racing_done_pg() {
    let reattached = race_entry_to_done_during_new_attachment("pmd_reopen", "dispatched").await;
    let result = reattached
        .result
        .expect("pool status retry must converge onto the raced done row");
    assert!(result.changed);
    assert_eq!(result.from_status, ENTRY_STATUS_DONE);
    assert_eq!(reattached.row.status_for_test(), ENTRY_STATUS_DISPATCHED);
    assert_eq!(
        reattached.row.dispatch_id_for_test(),
        Some("dispatch-requested-done-race")
    );
    assert_eq!(reattached.row.slot_index_for_test(), Some(0));
    assert!(reattached.row.completed_at_for_test().is_none());
    // Convergence runs through the canonical bookkeeping, not around it.
    assert_eq!(
        reattached.transitions,
        vec![(
            ENTRY_STATUS_DONE.to_string(),
            ENTRY_STATUS_DISPATCHED.to_string(),
            "pmd_reopen".to_string()
        )]
    );
    assert_eq!(
        reattached.dispatch_history,
        vec!["dispatch-requested-done-race".to_string()]
    );

    // The approved-source gate is real: the same race under a source the
    // done -> dispatched transition table does not name is refused, and the
    // competitor's row is left untouched.
    let unapproved =
        race_entry_to_done_during_new_attachment("test_pool_status_retry", "dispatched").await;
    let error = unapproved
        .result
        .expect_err("an unapproved source must not reattach a raced done entry");
    assert!(
        error.contains("invalid auto-queue entry transition for gate-entry: done -> dispatched"),
        "{error}"
    );
    assert_eq!(unapproved.row.status_for_test(), ENTRY_STATUS_DONE);
    assert_eq!(unapproved.row.dispatch_id_for_test(), None);
    assert_eq!(unapproved.row.slot_index_for_test(), None);
    assert!(unapproved.transitions.is_empty());
    assert!(unapproved.dispatch_history.is_empty());

    // The attachment guard is real too. It runs before the CAS, so this
    // leg is refused on the first attempt rather than at the retry — which
    // is what shows the converging leg above was granted by a live
    // dispatch, not by an absent liveness check.
    let terminal_dispatch = race_entry_to_done_during_new_attachment("pmd_reopen", "failed").await;
    let error = terminal_dispatch
        .result
        .expect_err("a terminal dispatch must not be attached to a raced done entry");
    assert!(error.contains("is failed: refusing to attach"), "{error}");
    assert_eq!(terminal_dispatch.row.status_for_test(), ENTRY_STATUS_DONE);
    assert_eq!(terminal_dispatch.row.dispatch_id_for_test(), None);
    assert!(terminal_dispatch.transitions.is_empty());
    assert!(terminal_dispatch.dispatch_history.is_empty());
}

#[tokio::test]
async fn failed_dispatch_is_rechecked_before_late_attachment_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
    seed_dispatch(&pool, "dispatch-failed-before-attach").await;
    sqlx::query(
        "UPDATE auto_queue_entries
         SET dispatch_id = 'dispatch-failed-before-attach'
         WHERE id = 'gate-entry'",
    )
    .execute(&pool)
    .await
    .expect("seed stale pending-entry dispatch pointer");
    sqlx::query(
        "UPDATE task_dispatches
         SET status = 'failed', updated_at = NOW()
         WHERE id = 'dispatch-failed-before-attach'",
    )
    .execute(&pool)
    .await
    .expect("terminalize dispatch before late attachment");

    let error = update_entry_status_on_pg(
        &pool,
        "gate-entry",
        ENTRY_STATUS_DISPATCHED,
        "restore_run_attach_existing_dispatch",
        &EntryStatusUpdateOptions {
            dispatch_id: Some("dispatch-failed-before-attach".to_string()),
            slot_index: Some(0),
        },
    )
    .await
    .expect_err("failed dispatch must not be attached after the restore-path observation");
    assert!(error.contains("is failed: refusing to attach"), "{error}");
    assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);
    assert_eq!(transition_count(&pool).await, 0);
    assert!(
        list_entry_dispatch_history_pg(&pool, "gate-entry")
            .await
            .expect("load rejected late-attach history")
            .is_empty()
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn terminal_dispatch_commit_blocks_and_rejects_late_attachment_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
    seed_dispatch(&pool, "dispatch-terminalizing-during-attach").await;

    let mut terminalizer = pool.acquire().await.expect("acquire terminalizer");
    sqlx::query("BEGIN")
        .execute(&mut *terminalizer)
        .await
        .expect("begin terminalizer");
    sqlx::query(
        "UPDATE task_dispatches
         SET status = 'failed', updated_at = NOW()
         WHERE id = 'dispatch-terminalizing-during-attach'",
    )
    .execute(&mut *terminalizer)
    .await
    .expect("terminalize dispatch without committing");

    let attach_pool = pool.clone();
    let attachment = tokio::spawn(async move {
        update_entry_status_on_pg(
            &attach_pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "restore_run_attach_existing_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-terminalizing-during-attach".to_string()),
                slot_index: Some(0),
            },
        )
        .await
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut *terminalizer)
            .await
            .expect("clear late-attach waiter snapshot");
        let waiting = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND wait_event_type = 'Lock'
                   AND query LIKE '%FOR SHARE%'
             )",
        )
        .fetch_one(&mut *terminalizer)
        .await
        .expect("inspect dispatch FOR SHARE waiter");
        if waiting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "late attachment did not wait on the terminal dispatch update"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    sqlx::query("COMMIT")
        .execute(&mut *terminalizer)
        .await
        .expect("commit terminal dispatch update");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), attachment)
        .await
        .expect("late attachment finishes after terminal commit")
        .expect("join late attachment")
        .expect_err("committed terminal dispatch must reject late attachment");
    assert!(error.contains("is failed: refusing to attach"), "{error}");
    assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);
    assert_eq!(transition_count(&pool).await, 0);

    drop(terminalizer);
    pool.close().await;
    pg_db.drop().await;
}
