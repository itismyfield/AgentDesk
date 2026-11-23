//! Moved from src/db/dispatched_sessions.rs: these force-kill retry tests pass the KE dispatch writer.

use crate::db::dispatched_sessions::selector_cleanup_tests::{
    TestPostgresDb, assert_cleanup_preserved_selectors, seed_session_with_selectors, session_state,
};
use crate::db::dispatched_sessions::*;

#[tokio::test]
async fn disconnect_session_and_prepare_retry_pg_preserves_provider_selectors() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let session_key = "host:selector-force-kill";

    seed_session_with_selectors(&pool, session_key, "idle", Some("dispatch-1841")).await;

    let retry_meta = disconnect_session_and_prepare_retry_pg(
        &pool,
        session_key,
        None,
        false,
        crate::dispatch::set_dispatch_status_on_pg_boxed,
    )
    .await
    .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    assert!(retry_meta.is_none());
    assert_cleanup_preserved_selectors(&pool, session_key).await;

    pool.close().await;
    pg_db.drop().await;
}

/// Force-kill on a session whose dispatch is already `cancelled` keeps that status and makes no
/// retry; the session row itself is still disconnected (operator intent).
#[tokio::test]
async fn disconnect_session_and_prepare_retry_pg_skips_cancelled_dispatch() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let session_key = "host:selector-cancelled-guard";
    let dispatch_id = "dispatch-2045-cancelled-guard";
    let card_id = "card-2045-cancelled-guard";

    // Seed: card + cancelled dispatch + session pointing at it.
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
     VALUES ($1, 'guard card', 'backlog', NOW(), NOW())",
    )
    .bind(card_id)
    .execute(&pool)
    .await
    .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    sqlx::query(
        "INSERT INTO task_dispatches
     (id, kanban_card_id, dispatch_type, status, title, context,
      created_at, updated_at, completed_at)
     VALUES ($1, $2, 'implementation', 'cancelled', 'guard',
             '{}', NOW(), NOW(), NOW())",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .execute(&pool)
    .await
    .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    seed_session_with_selectors(&pool, session_key, "idle", Some(dispatch_id)).await;

    // Caller asks for retry=true. Guard must reject both the failure
    // overwrite and the retry creation.
    let retry_meta = disconnect_session_and_prepare_retry_pg(
        &pool,
        session_key,
        Some(dispatch_id),
        true,
        crate::dispatch::set_dispatch_status_on_pg_boxed,
    )
    .await
    .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    assert!(
        retry_meta.is_none(),
        "cancelled dispatch must not produce retry metadata"
    );

    // Dispatch status remains 'cancelled' (NOT overwritten to 'failed').
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_one(&pool)
            .await
            .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    assert_eq!(
        dispatch_status, "cancelled",
        "force-kill must not overwrite cancelled → failed"
    );

    // Session row is still disconnected (force-kill side effect is fine).
    let (session_status, active_dispatch_id, _, _) = session_state(&pool, session_key).await;
    assert_eq!(session_status, "disconnected");
    assert_eq!(active_dispatch_id, None);

    pool.close().await;
    pg_db.drop().await;
}
