use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn snapshot_activation_persists_published_routing_pg() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\ncluster: {dispatch_routing: {default_preferred_labels: [snapshot]}}",
            "server: {}\ncluster: {dispatch_routing: {default_preferred_labels: [disk]}}",
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_id) VALUES ('snapshot-agent','Snapshot','claude','41')").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO kanban_cards(id,title,status,assigned_agent_id,metadata) VALUES ('snapshot-card','Snapshot','in_progress','snapshot-agent','{\"sandbox_preflight\":true,\"production_mutation_allowed\":false}'::jsonb)").execute(&pool).await.unwrap();
            let id = create_activate_dispatch_pg_inner(&pool,"snapshot-card","snapshot-agent","implementation","Snapshot",&serde_json::json!({}),None).await.unwrap();
            let required: serde_json::Value = sqlx::query_scalar("SELECT required_capabilities FROM task_dispatches WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
            assert_eq!(required, serde_json::json!({"preferred":{"labels":["snapshot"]}}));
            pool.close().await;
            db.drop().await;
        });
    });
}

/// Re-checking an existing live dispatch attaches the pending entry to it in the create tx,
/// with the requested slot, instead of creating a second dispatch.
#[tokio::test]
async fn existing_dispatch_recheck_attaches_the_pending_entry_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::raw_sql(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('attach-agent', 'Attach', 'claude', '42');
         INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('attach-run', 'repo', 'attach-agent', 'active');
         INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ('attach-card', 'Attach', 'in_progress', 'attach-agent');
         INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status)
         VALUES ('d-existing', 'attach-card', 'attach-agent', 'implementation', 'pending');
         INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ('attach-entry', 'attach-run', 'attach-card', 'attach-agent', 'pending');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let id = create_activate_dispatch_for_entry_pg(
        &pool,
        "attach-card",
        "attach-agent",
        "implementation",
        "Attach",
        &serde_json::json!({}),
        ActivateDispatchEntryAttachment::new("attach-entry", Some(2), "test_existing_attach"),
    )
    .await
    .unwrap();
    assert_eq!(id, "d-existing");
    let entry: (String, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT status, dispatch_id, slot_index::BIGINT FROM auto_queue_entries
          WHERE id = 'attach-entry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        entry,
        (
            "dispatched".to_string(),
            Some("d-existing".to_string()),
            Some(2)
        )
    );
    let dispatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'attach-card'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dispatches, 1);
    let transitions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auto_queue_entry_transitions
          WHERE entry_id = 'attach-entry' AND to_status = 'dispatched'
            AND trigger_source = 'test_existing_attach'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(transitions, 1);
    pool.close().await;
    db.drop().await;
}
