use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::kanban::upsert_card_from_issue_pg;
use crate::github::test_support::issue;
use serde_json::{Value, json};

async fn seed(pool: &PgPool) -> String {
    upsert_card_from_issue_pg(
        pool,
        IssueCardUpsert {
            repo_id: "sync/repo".into(),
            issue_number: 42,
            title: "Unrouted maintenance".into(),
            description: Some("No prerequisite in this body".into()),
            metadata_json: Some(
                json!({
                    "labels": "old", "depends_on": [100], "dependencies": [101],
                    "scope_assessment_status": "completed"
                })
                .to_string(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .card_id
}

async fn metadata(pool: &PgPool, card: &str) -> Value {
    sqlx::query_scalar("SELECT metadata FROM kanban_cards WHERE id = $1")
        .bind(card)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_preserved(value: &Value, labels: &str) {
    assert_eq!(value["labels"], labels);
    assert_eq!(value["depends_on"], json!([100]));
    assert_eq!(value["dependencies"], json!([101]));
    assert_eq!(value["scope_assessment_status"], "completed");
}

#[tokio::test]
async fn issue_sync_preserves_metadata_on_label_change_and_removal_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let card = seed(&pool).await;
    for labels in [vec!["bug"], vec!["enhancement", "ready"], vec![]] {
        let issue = issue(
            "Unrouted maintenance",
            Some("No prerequisite in this body"),
            &labels,
        );
        assert_eq!(
            triage_new_issues_pg(&pool, "sync/repo", &[issue])
                .await
                .unwrap(),
            0
        );
        assert_preserved(&metadata(&pool, &card).await, &labels.join(","));
    }
    sqlx::query("UPDATE kanban_cards SET status = 'ready' WHERE id = $1")
        .bind(&card)
        .execute(&pool)
        .await
        .unwrap();
    let config = crate::config::Config::default();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: std::sync::Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: None,
    };
    let body = serde_json::from_value(json!({"repo": "sync/repo"})).unwrap();
    let (_, axum::Json(response)) =
        crate::server::routes::auto_queue::generate(axum::extract::State(state), axum::Json(body))
            .await
            .unwrap();
    assert_eq!(
        response["skipped_due_to_dependency"],
        json!([
            {"issue_number": 42, "unresolved_deps": ["#100:missing", "#101:missing"]}
        ])
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auto_queue_entries WHERE kanban_card_id = $1")
            .bind(&card)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(queued, 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn inferred_label_write_preserves_metadata_and_replaces_labels_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let card = seed(&pool).await;
    for labels in [vec!["bug"], vec![]] {
        let issue = issue("Unrouted maintenance", None, &labels);
        update_card_metadata_with_inferred_agent_label_pg(
            &pool,
            &card,
            &issue,
            "project-agentdesk",
        )
        .await
        .unwrap();
        let expected = if labels.is_empty() {
            "agent:project-agentdesk"
        } else {
            "bug,agent:project-agentdesk"
        };
        assert_preserved(&metadata(&pool, &card).await, expected);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn issue_label_writes_keep_concurrent_metadata_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let card = seed(&pool).await;
    for inferred in [false, true] {
        let mut writer = pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *writer)
            .await
            .unwrap();
        sqlx::query("UPDATE kanban_cards SET metadata = metadata || jsonb_build_object('concurrent', $2::bool) WHERE id = $1")
            .bind(&card).bind(inferred).execute(&mut *writer).await.unwrap();
        let (task_pool, task_card) = (pool.clone(), card.clone());
        let sync = tokio::spawn(async move {
            let issue = issue("Unrouted maintenance", None, &["new"]);
            if inferred {
                update_card_metadata_with_inferred_agent_label_pg(
                    &task_pool,
                    &task_card,
                    &issue,
                    "project-agentdesk",
                )
                .await
                .unwrap();
            } else {
                triage_new_issues_pg(&task_pool, "sync/repo", &[issue])
                    .await
                    .unwrap();
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let blocked: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid))",
                ).bind(pid).fetch_one(&pool).await.unwrap();
                if blocked > 0 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("sync must contend with the metadata writer");
        writer.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), sync)
            .await
            .unwrap()
            .unwrap();
        let value = metadata(&pool, &card).await;
        assert_eq!(value["concurrent"], inferred);
        assert_preserved(
            &value,
            if inferred {
                "new,agent:project-agentdesk"
            } else {
                "new"
            },
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn label_writes_replace_non_object_metadata_with_object_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let card = seed(&pool).await;
    for (stored, sql, inferred) in [
        (
            "array",
            "UPDATE kanban_cards SET metadata = '[1]'::jsonb WHERE id = $1",
            false,
        ),
        (
            "string",
            "UPDATE kanban_cards SET metadata = '\"s\"'::jsonb WHERE id = $1",
            false,
        ),
        (
            "json null",
            "UPDATE kanban_cards SET metadata = 'null'::jsonb WHERE id = $1",
            true,
        ),
        (
            "sql null",
            "UPDATE kanban_cards SET metadata = NULL WHERE id = $1",
            true,
        ),
    ] {
        sqlx::query(sql).bind(&card).execute(&pool).await.unwrap();
        let issue = issue("Unrouted maintenance", None, &["bug"]);
        let expected = if inferred {
            update_card_metadata_with_inferred_agent_label_pg(
                &pool,
                &card,
                &issue,
                "project-agentdesk",
            )
            .await
            .unwrap();
            json!({"labels": "bug,agent:project-agentdesk"})
        } else {
            triage_new_issues_pg(&pool, "sync/repo", &[issue])
                .await
                .unwrap();
            json!({"labels": "bug"})
        };
        let kind: String =
            sqlx::query_scalar("SELECT jsonb_typeof(metadata) FROM kanban_cards WHERE id = $1")
                .bind(&card)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kind, "object", "{stored}");
        assert_eq!(metadata(&pool, &card).await, expected, "{stored}");
    }
    sqlx::query("UPDATE kanban_cards SET metadata = '[1]'::jsonb WHERE id = $1")
        .bind(&card)
        .execute(&pool)
        .await
        .unwrap();
    upsert_card_from_issue_pg(
        &pool,
        IssueCardUpsert {
            repo_id: "sync/repo".into(),
            issue_number: 42,
            title: "Unrouted maintenance".into(),
            metadata_json: Some(json!({"depends_on": [100]}).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(metadata(&pool, &card).await, json!({"depends_on": [100]}));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn new_issue_card_is_inserted_with_labels_before_inferred_label_write_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut new_issue = issue("Unrouted maintenance", None, &[" bug", " ", "ready "]);
    new_issue.number = 43;
    assert_eq!(
        triage_new_issues_pg(&pool, "sync/repo", &[new_issue.clone()])
            .await
            .unwrap(),
        1
    );
    let card: String = sqlx::query_scalar(
        "SELECT id FROM kanban_cards WHERE repo_id = 'sync/repo' AND github_issue_number = 43",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(metadata(&pool, &card).await, json!({"labels": "bug,ready"}));
    update_card_metadata_with_inferred_agent_label_pg(
        &pool,
        &card,
        &new_issue,
        "project-agentdesk",
    )
    .await
    .unwrap();
    assert_eq!(
        metadata(&pool, &card).await,
        json!({"labels": "bug,ready,agent:project-agentdesk"})
    );
    pool.close().await;
    db.drop().await;
}
