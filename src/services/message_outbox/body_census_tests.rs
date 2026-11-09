use super::store;
use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg_on_tx};
use serde_json::json;

/// sqlx answers a failed savepoint BEGIN by rolling back the whole outer
/// transaction, so the enqueue must fail rather than commit a lost row.
#[tokio::test]
async fn failed_census_savepoint_fails_the_enqueue_instead_of_committing_a_lost_row_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_body_census_savepoint",
        "body census savepoint tests",
    )
    .await
    else {
        return;
    };
    // A second connection terminates the outer transaction's backend.
    let pool = pg_db.connect_and_migrate_with_max_connections(2).await;
    for case in ["backend_terminated", "outer_aborted"] {
        let mut tx = pool.begin().await.expect("begin outer enqueue tx");
        let message = OutboxMessage {
            target: "channel:7852",
            content: case,
            bot: "claude",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key: None,
        };
        let id = enqueue_outbox_pg_on_tx(&mut tx, message)
            .await
            .expect("insert outbox row");
        if case == "backend_terminated" {
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .expect("outer backend pid");
            let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1, 5000)")
                .bind(pid)
                .fetch_one(&pool)
                .await
                .expect("terminate outer backend");
            assert!(
                terminated,
                "outer backend must be gone before the savepoint"
            );
        } else {
            sqlx::query("SELECT no_such_column FROM message_outbox")
                .execute(&mut *tx)
                .await
                .expect_err("failed statement aborts the outer transaction");
        }

        let census = json!({"schema_version": 1});
        let outcome: Result<(), sqlx::Error> = async move {
            store(&mut tx, id, Some(&census)).await?;
            tx.commit().await
        }
        .await;

        let error = outcome.expect_err(&format!("{case}: enqueue must not report success"));
        if case == "outer_aborted" {
            let code = error.as_database_error().and_then(|db| db.code());
            assert_eq!(code.as_deref(), Some("25P02"), "{case}: {error}");
        }
        let durable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("count enqueued row");
        assert_eq!(durable, 0, "{case}: the outer transaction was lost");
    }
    pool.close().await;
    pg_db.drop().await;
}
