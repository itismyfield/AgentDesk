use super::*;
use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody, output};
use crate::services::message_outbox::{self, OutboxMessage};

#[tokio::test]
async fn output_origin_guard_database_error_retries_after_json_read_pg() {
    for stamped in [false, true] {
        let (_db, pool, origin) =
            output::tests::fixture(if stamped { "72280700" } else { "72280709" }).await;
        let mut tx = pool.begin().await.unwrap();
        let message = OutboxMessage {
            target: "channel:72280700",
            content: "guard recovery answer",
            bot: "notify",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key: Some(if stamped {
                "guard-retry-stamped"
            } else {
                "guard-retry-unstamped"
            }),
        };
        let id = if stamped {
            output::enqueue_on_tx(&mut tx, message, &origin.seal(message.content))
                .await
                .unwrap()
        } else {
            message_outbox::enqueue_outbox_pg_on_tx(&mut tx, message)
                .await
                .unwrap()
        };
        tx.commit().await.unwrap();
        let mock = super::origin_fixture::Discord::new().await;
        let registry = mock.registry().await;
        let pool = Arc::new(pool);
        let mut barrier = pool.begin().await.unwrap();
        let barrier_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *barrier)
            .await
            .unwrap();
        custody::lock(&mut barrier, &OwnerIdentity::new("claude", "72280700"))
            .await
            .unwrap();
        let draining_pool = pool.clone();
        let task = tokio::spawn(async move {
            drain_message_outbox_once(&draining_pool, &registry, "guard-retry-consumer").await
        });
        let pid = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let pid: Option<i32> = sqlx::query_scalar("SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%pg_advisory_xact_lock%' LIMIT 1")
                    .bind(barrier_pid).fetch_optional(pool.as_ref()).await.unwrap();
                if let Some(pid) = pid { break pid; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("consumer must reach guard lock after reading saved JSON");
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
                .bind(pid)
                .fetch_one(pool.as_ref())
                .await
                .unwrap()
        );
        barrier.rollback().await.unwrap();
        assert_eq!(task.await.unwrap(), 1);
        let (status, retries, claimed): (String, i64, Option<String>) =
            sqlx::query_as("SELECT status,retry_count,claim_owner FROM message_outbox WHERE id=$1")
                .bind(id)
                .fetch_one(pool.as_ref())
                .await
                .unwrap();
        assert_eq!(
            status, "pending",
            "database failure must retain worker retry, stamped={stamped}"
        );
        assert_eq!(retries, 1);
        assert!(claimed.is_none());
        assert!(mock.posts().is_empty());
        sqlx::query("UPDATE message_outbox SET next_attempt_at=NOW() WHERE id=$1")
            .bind(id)
            .execute(pool.as_ref())
            .await
            .unwrap();
        let registry = mock.registry().await;
        assert_eq!(
            drain_message_outbox_once(&pool, &registry, "recovered-consumer").await,
            1
        );
        assert_eq!(
            mock.posts().len(),
            1,
            "recovered guard must perform one actual POST, stamped={stamped}"
        );
        let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(status, "sent");
    }
}

#[tokio::test]
async fn output_origin_confirmed_denials_remain_held_on_real_drain_pg() {
    for kind in ["revoked", "unstamped", "malformed"] {
        let (_db, pool, origin) = output::tests::fixture("72280700").await;
        let mut tx = pool.begin().await.unwrap();
        let message = OutboxMessage {
            target: "channel:72280700",
            content: "denied answer",
            bot: "notify",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key: None,
        };
        let id = if kind == "unstamped" {
            message_outbox::enqueue_outbox_pg_on_tx(&mut tx, message)
                .await
                .unwrap()
        } else {
            output::enqueue_on_tx(&mut tx, message, &origin.seal(message.content))
                .await
                .unwrap()
        };
        tx.commit().await.unwrap();
        if kind == "revoked" {
            output::tests::revoke(&pool, &origin).await;
        }
        if kind == "malformed" {
            sqlx::query(
                "UPDATE message_outbox SET output_origin='{\"origin\":{}}'::jsonb WHERE id=$1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mock = super::origin_fixture::Discord::new().await;
        assert_eq!(
            drain_message_outbox_once(
                &Arc::new(pool.clone()),
                &mock.registry().await,
                "denial-consumer"
            )
            .await,
            1
        );
        let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "held",
            "confirmed {kind} denial must not enter retry"
        );
        assert!(mock.posts().is_empty());
    }
}
