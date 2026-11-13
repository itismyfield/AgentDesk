//! Runtime completion of a review-disabled run's dispatch: the entry sync, run completion and
//! its notice commit with the dispatch, event and reconcile marker, or none of them do.

use super::{runtime_pg_complete_dispatch_with_result, runtime_postgres_reconcile_key};
use crate::db::auto_queue::test_support::TestPostgresDb;
use sqlx::PgPool;

/// Live config whose database section points the runtime pool at the test database.
struct RuntimeDatabase {
    _live: crate::config_live_reload::TestLiveGuard,
}

impl RuntimeDatabase {
    /// Takes the shared test-env lock, so it must run before the test database is created.
    fn lock() -> Self {
        Self {
            _live: crate::config_live_reload::test_live_guard(
                crate::config::test_env_lock::acquire_shared_test_env_lock(),
            ),
        }
    }

    fn install(&self, database_url: &str) {
        let url = url::Url::parse(database_url).unwrap();
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.host = url.host_str().unwrap().to_string();
        config.database.port = url.port().unwrap_or(5432);
        config.database.user = url.username().to_string();
        config.database.password = url.password().map(str::to_string);
        config.database.dbname = url.path().trim_start_matches('/').to_string();
        config.database.pool_max = 2;
        config.database.foreground_reserve = 0;
        crate::config_live_reload::install(config);
    }
}

async fn state(pool: &PgPool) -> (String, String, String, Option<String>, i64, i64, i64) {
    sqlx::query_as(
        "SELECT d.status, e.status, r.status, s.assigned_run_id,
                (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = 'd-runtime'),
                (SELECT COUNT(*) FROM kv_meta WHERE key = $1),
                (SELECT COUNT(*) FROM message_outbox WHERE bot = 'notify')
           FROM task_dispatches d, auto_queue_entries e, auto_queue_runs r, auto_queue_slots s
          WHERE d.id = 'd-runtime' AND e.id = 'entry-runtime' AND r.id = 'run-runtime'
            AND s.agent_id = 'agent-runtime' AND s.slot_index = 0",
    )
    .bind(runtime_postgres_reconcile_key("d-runtime"))
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_completion_completes_run_in_the_dispatch_tx_or_rolls_back_all_pg() {
    let runtime = RuntimeDatabase::lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    runtime.install(&db.database_url);
    sqlx::raw_sql(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-runtime', 'Runtime', 'claude', '7353801');
         INSERT INTO auto_queue_runs (id, repo, agent_id, status, review_mode)
         VALUES ('run-runtime', 'repo', 'agent-runtime', 'active', 'disabled');
         INSERT INTO auto_queue_slots
             (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-runtime', 0, 'run-runtime', 0, CAST('{}' AS jsonb));
         INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ('card-runtime', 'Runtime', 'in_progress', 'agent-runtime');
         INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status)
         VALUES ('d-runtime', 'card-runtime', 'agent-runtime', 'implementation', 'dispatched');
         INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id)
         VALUES ('entry-runtime', 'run-runtime', 'card-runtime', 'agent-runtime', 'dispatched',
                 'd-runtime');
         CREATE FUNCTION reject_outbox_insert() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'outbox rejected by test'; END $$ LANGUAGE plpgsql;
         CREATE TRIGGER reject_outbox BEFORE INSERT ON message_outbox
         FOR EACH ROW EXECUTE FUNCTION reject_outbox_insert();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let result = serde_json::json!({"completion_source": "watcher_streaming_final"});

    assert!(!runtime_pg_complete_dispatch_with_result(
        "d-runtime",
        &result,
        "watcher_streaming_final"
    ));
    assert_eq!(
        state(&pool).await,
        (
            "dispatched".to_string(),
            "dispatched".to_string(),
            "active".to_string(),
            Some("run-runtime".to_string()),
            0,
            0,
            0
        ),
        "a failed completion notice rolls back the dispatch, event and marker too"
    );

    sqlx::query("DROP TRIGGER reject_outbox ON message_outbox")
        .execute(&pool)
        .await
        .unwrap();
    assert!(runtime_pg_complete_dispatch_with_result(
        "d-runtime",
        &result,
        "watcher_streaming_final"
    ));
    assert_eq!(
        state(&pool).await,
        (
            "completed".to_string(),
            "done".to_string(),
            "completed".to_string(),
            None,
            1,
            1,
            1
        )
    );
    pool.close().await;
    db.drop().await;
}
