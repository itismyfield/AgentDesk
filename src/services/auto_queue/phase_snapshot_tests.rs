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
