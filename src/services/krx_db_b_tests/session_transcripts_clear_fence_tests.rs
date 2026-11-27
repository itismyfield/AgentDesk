//! Moved from src/db/session_transcripts.rs: these clear-fence tests drive the KE routine store.

use crate::db::session_transcripts::capture_channel_clear_fence_for_test as capture_channel_clear_fence;
use crate::db::session_transcripts::clear_fence_pg_tests::{channel_pairs, create_pool, entry};
use crate::db::session_transcripts::*;
use crate::services::discord::health::routine_ports::build_routine_agent_executor;
use crate::services::routines::RoutineStore;
use sqlx::PgPool;
use std::sync::Arc;

async fn sa2_routine_store(pool: &PgPool) -> RoutineStore {
    sqlx::query(
        "INSERT INTO routines (id, script_ref, name, in_flight_run_id) VALUES ('routine', 'fixture', 'fixture', 'run')",
    )
    .execute(pool)
    .await
    .expect("routine");
    sqlx::query("INSERT INTO routine_runs (id, routine_id) VALUES ('run', 'routine'), ('duplicate', 'routine')").execute(pool).await.expect("runs");
    RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 1024)
}

async fn sa2_mark(store: &RoutineStore, run: &str, channel: &str, thread: Option<&str>) {
    let proof = serde_json::json!({"channel_id": channel, "discord_thread_id": thread});
    assert!(
        store
            .mark_agent_turn_started(run, "discord:200:9001", Some(proof), "fixture", "fresh")
            .await
            .expect("mark attempt")
    );
}

#[tokio::test]
async fn sa2_own_clear_and_midturn_clear_pg() {
    for response in ["오늘 브리핑입니다", "NO_REPLY"] {
        let (db, pool) = create_pool().await;
        let store = sa2_routine_store(&pool).await;
        let executor = build_routine_agent_executor(Arc::new(pool.clone()), None, 1800);
        sa2_mark(&store, "run", "200", Some("200")).await;
        record_channel_clear_boundary(Some(&pool), "200")
            .await
            .expect("own clear");
        let fence = capture_channel_clear_fence(Some(&pool), "200").await;
        let mut item = entry("discord:200:9001", "200", 0); // Synthetic snowflake predates own clear.
        item.assistant_message = response;
        assert!(
            persist_turn_db_with_clear_fence(Some(&pool), item, Some(fence.clone()))
                .await
                .expect("own insert")
        );
        assert_eq!(channel_pairs(&pool, "200").await.len(), 1);
        let outcomes = executor
            .poll_agent_runs(&store, 10, false)
            .await
            .expect("completion poll");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, "succeeded");
        assert_eq!(outcomes[0].run_id, "run");
        record_channel_clear_boundary(Some(&pool), "200")
            .await
            .expect("midturn clear");
        assert!(
            !persist_turn_db_with_clear_fence(
                Some(&pool),
                entry("stale", "200", i64::MAX),
                Some(fence)
            )
            .await
            .expect("stale insert")
        );
        assert!(channel_pairs(&pool, "200").await.is_empty());
        pool.close().await;
        db.drop().await;
    }
}

#[tokio::test]
async fn sa2_exact_routine_attempt_proof_pg() {
    let (db, pool) = create_pool().await;
    let store = sa2_routine_store(&pool).await;
    for (channel, thread, expected) in [
        ("200", Some("200"), true),
        ("200", None, true),
        ("parent", None, false),
        ("parent", Some("200"), false),
        ("200", Some("sibling"), false),
    ] {
        sa2_mark(&store, "run", channel, thread).await;
        assert_eq!(
            routine_attempt_owns_turn_pg(Some(&pool), "discord:200:9001", "200").await,
            expected
        );
    }
    assert!(!routine_attempt_owns_turn_pg(Some(&pool), "foreign", "200").await);
    assert!(!routine_attempt_owns_turn_pg(None, "discord:200:9001", "200").await);
    for run in ["run", "duplicate"] {
        sa2_mark(&store, run, "200", Some("200")).await;
    }
    assert!(!routine_attempt_owns_turn_pg(Some(&pool), "discord:200:9001", "200").await);
    sqlx::query("ALTER TABLE routine_runs RENAME TO hidden_runs")
        .execute(&pool)
        .await
        .expect("hide proof");
    assert!(!routine_attempt_owns_turn_pg(Some(&pool), "discord:200:9001", "200").await);
    pool.close().await;
    db.drop().await;
}
