//! Fresh issue worktrees created while a dispatch is being made: hook deadline and baseline.

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::git::test_origin_fixture::{OriginFixture, git};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::Path;

const AGENT: &str = "agent-fresh-wt";

async fn seed_agent(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id) VALUES ($1, 'Fresh WT', '123456789012345678')",
    )
    .bind(AGENT)
    .execute(pool)
    .await
    .expect("seed agent");
}

async fn seed_card(pool: &PgPool, card_id: &str, issue_number: i64, repo_dir: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, github_issue_number, repo_id, assigned_agent_id)
         VALUES ($1, 'Fresh worktree card', 'requested', $2, $3, $4)",
    )
    .bind(card_id)
    .bind(issue_number)
    .bind(repo_dir)
    .bind(AGENT)
    .execute(pool)
    .await
    .expect("seed card");
}

async fn dispatch_context(pool: &PgPool, dispatch_id: &str) -> Value {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT context FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_one(pool)
            .await
            .expect("dispatch context");
    serde_json::from_str(raw.as_deref().unwrap_or("{}")).expect("context json")
}

fn worktree_head(context: &Value) -> String {
    let path = context["worktree_path"].as_str().expect("worktree_path");
    let head = git(Path::new(path), &["rev-parse", "HEAD"]);
    let _ = std::fs::remove_dir_all(path);
    head
}

async fn create_implementation(pool: &PgPool, card_id: &str, context: Value) -> Value {
    let (dispatch_id, _, _) = crate::dispatch::create_dispatch_core_with_options(
        pool,
        card_id,
        AGENT,
        "implementation",
        "Fresh worktree",
        &context,
        crate::dispatch::DispatchCreateOptions {
            skip_outbox: true,
            sidecar_dispatch: false,
        },
    )
    .await
    .expect("create implementation dispatch");
    dispatch_context(pool, &dispatch_id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_worktree_start_is_the_automatic_baseline_and_explicit_baseline_is_kept_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let fixture = OriginFixture::new("main");
    let latest = fixture.push_new_commit("landed after the desk cloned");
    seed_agent(&pool).await;
    seed_card(&pool, "card-auto-baseline", 761_301, fixture.desk_dir()).await;
    seed_card(&pool, "card-kept-baseline", 761_302, fixture.desk_dir()).await;

    let auto = create_implementation(&pool, "card-auto-baseline", json!({})).await;
    assert_eq!(auto["baseline_commit"], json!(latest));
    assert_eq!(worktree_head(&auto), latest);

    let explicit = json!({ "baseline_commit": fixture.main_tip });
    let kept = create_implementation(&pool, "card-kept-baseline", explicit).await;
    assert_eq!(kept["baseline_commit"], json!(fixture.main_tip));
    assert_eq!(worktree_head(&kept), latest);

    pool.close().await;
    pg_db.drop().await;
}

/// The default 5s policy hook deadline must survive a fetch that only finishes after 6s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_direct_implementation_is_created_and_linked_despite_a_slow_origin_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let fixture = OriginFixture::new("main");
    fixture.push_new_commit("not fetched within the hook");
    fixture.delay_desk_fetches(6);
    let card_id = "card-scope-direct";
    seed_agent(&pool).await;
    seed_card(&pool, card_id, 761_303, fixture.desk_dir()).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-scope', 'owner/repo', $1, 'active')",
    )
    .bind(AGENT)
    .execute(&pool)
    .await
    .expect("seed run");
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ('entry-scope', 'run-scope', $1, $2, 'pending')",
    )
    .bind(card_id)
    .bind(AGENT)
    .execute(&pool)
    .await
    .expect("seed entry");
    sqlx::query(
        "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
         VALUES ('dispatch-scope', $1, $2, 'scope-assessment', 'completed', 'Scope', $3, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(AGENT)
    .bind(json!({ "scope_depth": "direct" }).to_string())
    .execute(&pool)
    .await
    .expect("seed completed scope-assessment");

    let mut config = crate::config::Config::default();
    assert_eq!(config.policies.hook_timeout_ms, 5_000);
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .expect("create policy engine");
    let hook_engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        crate::kanban::fire_event_hooks_with_backends(
            &hook_engine,
            "on_dispatch_completed",
            "OnDispatchCompleted",
            json!({
                "dispatch_id": "dispatch-scope",
                "kanban_card_id": card_id,
                "result": { "scope_depth": "direct" },
            }),
        );
    })
    .await
    .expect("fire scope completion");

    let implementations: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_dispatches WHERE kanban_card_id = $1 AND dispatch_type = 'implementation'",
    )
    .bind(card_id)
    .fetch_all(&pool)
    .await
    .expect("implementation dispatches");
    assert_eq!(implementations.len(), 1, "{implementations:?}");
    let (entry_status, entry_dispatch): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-scope'",
    )
    .fetch_one(&pool)
    .await
    .expect("entry");
    assert_eq!(entry_status, "dispatched");
    assert_eq!(entry_dispatch.as_deref(), Some(implementations[0].as_str()));
    let context = dispatch_context(&pool, &implementations[0]).await;
    assert_eq!(worktree_head(&context), fixture.main_tip);

    drop(engine);
    pg_db.drop().await;
}
