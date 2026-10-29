use super::upsert_backlog_card_from_issue_pg;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::kanban::{IssueCardUpsert, upsert_card_from_issue_pg};
use serde_json::{Value, json};

fn issue_view(number: i64, labels: &[&str]) -> crate::github::IssueView {
    crate::github::IssueView {
        number,
        state: "OPEN".into(),
        title: format!("CLI issue {number}"),
        body: Some("body".into()),
        url: format!("https://github.com/cli/repo/issues/{number}"),
        labels: labels
            .iter()
            .map(|name| crate::github::sync::GhLabel {
                name: (*name).to_string(),
            })
            .collect(),
    }
}

async fn metadata(pool: &sqlx::PgPool, card: &str) -> Value {
    sqlx::query_scalar("SELECT metadata FROM kanban_cards WHERE id = $1")
        .bind(card)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn create_from_issue_on_existing_card_replaces_only_labels_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let card = upsert_card_from_issue_pg(
        &pool,
        IssueCardUpsert {
            repo_id: "cli/repo".into(),
            issue_number: 42,
            title: "Seeded".into(),
            metadata_json: Some(
                json!({"labels": "old", "depends_on": [100], "scope_assessment_status": "completed"})
                    .to_string(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .card_id;
    for (labels, expected) in [(vec!["bug", " ready "], "bug,ready"), (vec![], "")] {
        let returned =
            upsert_backlog_card_from_issue_pg(&pool, "cli/repo", &issue_view(42, &labels))
                .await
                .unwrap();
        assert_eq!(returned, card);
        assert_eq!(
            metadata(&pool, &card).await,
            json!({"labels": expected, "depends_on": [100], "scope_assessment_status": "completed"})
        );
    }
    let title: String = sqlx::query_scalar("SELECT title FROM kanban_cards WHERE id = $1")
        .bind(&card)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(title, "CLI issue 42");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn create_from_issue_inserts_new_card_with_labels_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (number, labels, expected) in [(43, vec!["p1", ""], "p1"), (44, vec![], "")] {
        let card =
            upsert_backlog_card_from_issue_pg(&pool, "cli/repo", &issue_view(number, &labels))
                .await
                .unwrap();
        assert_eq!(metadata(&pool, &card).await, json!({"labels": expected}));
        let (status, priority): (String, String) =
            sqlx::query_as("SELECT status, priority FROM kanban_cards WHERE id = $1")
                .bind(&card)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "backlog");
        assert_eq!(priority, if number == 43 { "high" } else { "medium" });
    }
    pool.close().await;
    db.drop().await;
}
