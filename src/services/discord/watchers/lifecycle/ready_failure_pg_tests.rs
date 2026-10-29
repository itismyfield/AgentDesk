use super::update_card_ready_failure_marker_pg;
use crate::db::auto_queue::test_support::TestPostgresDb;
use serde_json::{Value, json};

async fn card_state(pool: &sqlx::PgPool) -> (Value, Option<String>) {
    sqlx::query_as("SELECT metadata, blocked_reason FROM kanban_cards WHERE id = 'card-ready'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn ready_failure_marker_appends_label_once_without_losing_concurrent_metadata_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, metadata, created_at, updated_at)
         VALUES ('card-ready', 'Ready card', 'in_progress',
                 '{\"labels\": \" bug, ,ready \", \"depends_on\": [7]}'::jsonb, NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut writer = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE kanban_cards SET metadata = metadata || '{\"concurrent\": true}'::jsonb
         WHERE id = 'card-ready'",
    )
    .execute(&mut *writer)
    .await
    .unwrap();
    let task_pool = pool.clone();
    let marker = tokio::spawn(async move {
        update_card_ready_failure_marker_pg(&task_pool, "card-ready", "stalled").await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(pid)
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("marker write must contend with the metadata writer");
    writer.commit().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), marker)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let expected =
        json!({"labels": "bug,ready,stuck_at_ready", "depends_on": [7], "concurrent": true});
    assert_eq!(
        card_state(&pool).await,
        (expected.clone(), Some("stalled".into()))
    );

    assert!(
        update_card_ready_failure_marker_pg(&pool, "card-ready", "stalled again")
            .await
            .unwrap()
    );
    assert_eq!(
        card_state(&pool).await,
        (expected, Some("stalled again".into()))
    );

    sqlx::query("UPDATE kanban_cards SET metadata = '[\"x\"]'::jsonb WHERE id = 'card-ready'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        update_card_ready_failure_marker_pg(&pool, "card-ready", "stalled")
            .await
            .unwrap()
    );
    assert_eq!(
        card_state(&pool).await.0,
        json!({"labels": "stuck_at_ready"})
    );
    sqlx::query(
        "UPDATE kanban_cards SET metadata = '{\"labels\": [\"x\"]}'::jsonb WHERE id = 'card-ready'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        update_card_ready_failure_marker_pg(&pool, "card-ready", "stalled")
            .await
            .unwrap()
    );
    assert_eq!(
        card_state(&pool).await.0,
        json!({"labels": "stuck_at_ready"})
    );

    assert!(
        !update_card_ready_failure_marker_pg(&pool, "missing-card", "stalled")
            .await
            .unwrap()
    );
    pool.close().await;
    db.drop().await;
}
