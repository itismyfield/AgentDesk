//! Boot orphan-dispatch recovery leaves a protected channel's dispatch and session links.

use super::recover_orphan_pending_dispatches_once;
use crate::services::discord::input_runtime::fence::{Gate, test_health};
use crate::services::provider::ProviderKind;
use sqlx::PgPool;

async fn card_and_dispatch(pool: &PgPool, agent: &str, dispatch: &str, status: &str) {
    let card = format!("card-{dispatch}");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', $2)",
    )
    .bind(&card)
    .bind(agent)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at)
         VALUES ($1, $2, $3, 'implementation', $4, $1, NOW() - INTERVAL '2 days')",
    )
    .bind(dispatch)
    .bind(&card)
    .bind(agent)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, '1')")
        .bind(format!("dispatch_notified:{dispatch}"))
        .execute(pool)
        .await
        .unwrap();
}

async fn active_session(pool: &PgPool, agent: &str, channel: u64, dispatch: &str) {
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id, channel_id)
         VALUES ($1, $2, 'turn_active', $3, $4)",
    )
    .bind(format!("c2b2a-session-{agent}"))
    .bind(agent)
    .bind(dispatch)
    .bind(channel.to_string())
    .execute(pool)
    .await
    .unwrap();
}

async fn session_link(pool: &PgPool, agent: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, active_dispatch_id FROM sessions WHERE agent_id = $1")
        .bind(agent)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn notified(pool: &PgPool, dispatch: &str) -> (bool, bool) {
    let marker: Option<String> = sqlx::query_scalar("SELECT key FROM kv_meta WHERE key = $1")
        .bind(format!("dispatch_notified:{dispatch}"))
        .fetch_optional(pool)
        .await
        .unwrap();
    let outbox: Option<String> = sqlx::query_scalar(
        "SELECT status FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(dispatch)
    .fetch_optional(pool)
    .await
    .unwrap();
    (marker.is_some(), outbox.as_deref() == Some("pending"))
}

/// The hygiene pass keeps a protected channel's session linked and a pending dispatch whose
/// delivery channel is protected keeps its marker and gets no outbox row; the unprotected
/// sibling's stale link is cleared and its orphan dispatch requeued as before.
#[tokio::test]
async fn c2b_orphan_recovery_leaves_a_protected_channel_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (linked, target, sibling) = (6_325_580_001u64, 6_325_580_002u64, 6_325_580_003u64);
    for (agent, channel) in [
        ("c2b2a-a", linked),
        ("c2b2a-c", target),
        ("c2b2a-b", sibling),
    ] {
        let channel = channel.to_string();
        crate::db::agents::insert_agent_channels_for_tests(&pool, agent, Some(&channel), None)
            .await;
    }
    card_and_dispatch(&pool, "c2b2a-a", "c2b2a-done-a", "completed").await;
    card_and_dispatch(&pool, "c2b2a-b", "c2b2a-done-b", "completed").await;
    active_session(&pool, "c2b2a-a", linked, "c2b2a-done-a").await;
    active_session(&pool, "c2b2a-b", sibling, "c2b2a-done-b").await;
    card_and_dispatch(&pool, "c2b2a-c", "c2b2a-pending-c", "pending").await;
    card_and_dispatch(&pool, "c2b2a-b", "c2b2a-pending-b", "pending").await;
    let pid = crate::cli::agentdesk_runtime_root().unwrap();
    let pid = pid.join("runtime").join("dcserver.pid");
    std::fs::create_dir_all(pid.parent().unwrap()).unwrap();
    std::fs::write(&pid, "1").unwrap();
    let gates = [linked, target].map(|channel| {
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        test_health::Clear::new(&gate)
    });

    recover_orphan_pending_dispatches_once(Some(&pool)).await;

    assert_eq!(
        session_link(&pool, "c2b2a-a").await,
        ("turn_active".to_string(), Some("c2b2a-done-a".to_string()))
    );
    assert_eq!(
        session_link(&pool, "c2b2a-b").await,
        ("idle".to_string(), None)
    );
    assert_eq!(notified(&pool, "c2b2a-pending-c").await, (true, false));
    assert_eq!(notified(&pool, "c2b2a-pending-b").await, (false, true));
    drop(gates);
    pool.close().await;
    db.drop().await;
}
