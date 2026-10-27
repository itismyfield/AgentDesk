//! Boot orphan-dispatch recovery clears stale session links and requeues orphan dispatches.

use super::recover_orphan_pending_dispatches_once;
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

/// A terminal dispatch's session link is cleared and a pending dispatch's stale marker is
/// replaced by a pending notify outbox row.
#[tokio::test]
async fn orphan_recovery_clears_stale_links_and_requeues_pending_dispatches_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let sibling = 6_325_580_003u64;
    crate::db::agents::insert_agent_channels_for_tests(
        &pool,
        "c2b2a-b",
        Some(&sibling.to_string()),
        None,
    )
    .await;
    card_and_dispatch(&pool, "c2b2a-b", "c2b2a-done-b", "completed").await;
    active_session(&pool, "c2b2a-b", sibling, "c2b2a-done-b").await;
    card_and_dispatch(&pool, "c2b2a-b", "c2b2a-pending-b", "pending").await;
    let pid = crate::cli::agentdesk_runtime_root().unwrap();
    let pid = pid.join("runtime").join("dcserver.pid");
    std::fs::create_dir_all(pid.parent().unwrap()).unwrap();
    std::fs::write(&pid, "1").unwrap();
    recover_orphan_pending_dispatches_once(Some(&pool)).await;

    assert_eq!(
        session_link(&pool, "c2b2a-b").await,
        ("idle".to_string(), None)
    );
    assert_eq!(notified(&pool, "c2b2a-pending-b").await, (false, true));
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_orphan_boot_keeps_protected_dispatch_notify_fenced_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let mut dispatches = Vec::new();
    for (n, state) in [
        "withheld",
        "started_unclassified",
        "startup_failed_no_effect",
        "legacy",
    ]
    .into_iter()
    .enumerate()
    {
        let dispatch = format!("p3-orphan-dispatch-{n}");
        let agent = format!("p3-orphan-agent-{n}");
        crate::db::agents::insert_agent_channels_for_tests(&pool, &agent, None, None).await;
        card_and_dispatch(&pool, &agent, &dispatch, "pending").await;
        if state != "legacy" {
            let receipt:i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,user_text,provider,status,replay_only,replay_disposition,replay_source_message_ids,request_owner_id,turn_kind,agent_id,replay_preserved)
                VALUES('worker','leader',$1,$1,'original','claude','unknown',TRUE,'registered_not_started',ARRAY[$1],'7','standard','agent','{}'::jsonb) RETURNING id")
                .bind(format!("p3-orphan-{n}")).fetch_one(&pool).await.unwrap();
            sqlx::query("UPDATE task_dispatches SET replay_receipt_id=$2 WHERE id=$1")
                .bind(&dispatch)
                .bind(receipt)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1",
            )
            .bind(receipt)
            .execute(&pool)
            .await
            .unwrap();
            if state != "started_unclassified" {
                sqlx::query("UPDATE intake_outbox SET replay_disposition=$2 WHERE id=$1")
                    .bind(receipt)
                    .bind(state)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        dispatches.push((dispatch, state));
    }
    let path = crate::cli::agentdesk_runtime_root()
        .unwrap()
        .join("runtime/dcserver.pid");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "1").unwrap();
    recover_orphan_pending_dispatches_once(Some(&pool)).await;
    for (dispatch, state) in dispatches {
        assert_eq!(
            notified(&pool, &dispatch).await.1,
            state == "legacy",
            "{state}: protected boot rearm must stay zero, healthy control one"
        );
    }
    pool.close().await;
    db.drop().await;
}
