//! A new node cannot reclaim held work even when its old dispatch lease has expired.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

fn claim_request() -> TaskDispatchClaimRequest {
    TaskDispatchClaimRequest {
        claim_owner: "replacement-node".into(),
        ttl_secs: Some(60),
        limit: Some(50),
        to_agent_id: None,
        dispatch_type: None,
        lease_ttl_secs: Some(60),
    }
}

async fn receipt(pool: &PgPool, disposition: &str, index: usize) -> i64 {
    let channel = format!("dispatch-replay-{index}");
    let source = format!("dispatch-source-{index}");
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status,
            replay_only, replay_disposition, replay_source_message_ids, replay_request_key,
            replay_preserved
         ) VALUES ('old-node', 'leader', $1, $2, 'user', 'original request',
                   'foreground', 'replay-agent', 'claude', 'unknown', TRUE, $3,
                   ARRAY[$2]::TEXT[], $4,
                   '{\"partial_body\":\"retained output\",\"delivery_owed\":true}'::JSONB)
         RETURNING id",
    )
    .bind(channel)
    .bind(source)
    .bind(disposition)
    .bind(format!("dispatch-replay-key-{index}"))
    .fetch_one(pool)
    .await
    .unwrap()
}

type DispatchSnapshot = (
    String,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    String,
    String,
    i64,
);

async fn snapshot(pool: &PgPool) -> Vec<DispatchSnapshot> {
    sqlx::query_as(
        "SELECT id, status, claim_owner, claimed_at, claim_expires_at, result,
                replay_disposition, replay_receipt_id
           FROM task_dispatches ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn replay_hold_prevents_actual_dispatch_claim_and_expired_lease_reclaim_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    for (index, disposition) in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ]
    .into_iter()
    .enumerate()
    {
        let receipt = receipt(&pool, disposition, index).await;
        for (mode, status, owner, expired) in [
            ("pending", "pending", None, false),
            ("expired", "dispatched", Some("old-node"), true),
            ("untimed", "dispatched", Some("old-node"), false),
            ("unowned", "dispatched", None, false),
        ] {
            sqlx::query(
                "INSERT INTO task_dispatches (
                    id, status, claim_owner, claimed_at, claim_expires_at,
                    result, replay_receipt_id
                 ) VALUES ($1, $2, $3,
                           CASE WHEN $3::TEXT IS NULL THEN NULL ELSE NOW() - INTERVAL '2 hours' END,
                           CASE WHEN $4::BOOLEAN THEN NOW() - INTERVAL '1 hour' ELSE NULL END,
                           'preserved partial output and delivery obligation', $5)",
            )
            .bind(format!("held-{index}-{mode}"))
            .bind(status)
            .bind(owner)
            .bind(expired)
            .bind(receipt)
            .execute(&pool)
            .await
            .unwrap();
        }
    }
    let before = snapshot(&pool).await;
    assert_eq!(
        before.len(),
        12,
        "every held disposition covers every claim candidate shape"
    );
    let outcome = claim_task_dispatches_with_cluster_config(
        &pool,
        &claim_request(),
        &ClusterConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        outcome.claimed.is_empty(),
        "no held dispatch becomes new node work: {outcome:?}"
    );
    assert!(
        outcome.skipped.is_empty(),
        "held work is excluded before routing diagnostics"
    );
    assert_eq!(
        snapshot(&pool).await,
        before,
        "old claim, result, and receipt stay unchanged"
    );
    let preserved: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT replay_preserved FROM intake_outbox ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(preserved.len(), 3);
    for result in preserved {
        assert_eq!(result["partial_body"], "retained output");
        assert_eq!(result["delivery_owed"], true);
    }
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn replay_schema_keeps_actual_dispatch_claim_without_a_hold_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    sqlx::query(
        "INSERT INTO task_dispatches (id, status, result)
         VALUES ('normal-dispatch', 'pending', 'preserved result')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let outcome = claim_task_dispatches_with_cluster_config(
        &pool,
        &claim_request(),
        &ClusterConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.claimed.len(), 1);
    assert_eq!(outcome.claimed[0].id, "normal-dispatch");
    assert_eq!(outcome.claimed[0].claim_owner, "replacement-node");
    assert!(outcome.skipped.is_empty());
    let state: (String, String, bool, bool, String, Option<String>) = sqlx::query_as(
        "SELECT status, claim_owner, claimed_at IS NOT NULL, claim_expires_at > NOW(),
                result, replay_disposition FROM task_dispatches WHERE id='normal-dispatch'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        state,
        (
            "dispatched".into(),
            "replacement-node".into(),
            true,
            true,
            "preserved result".into(),
            None
        )
    );
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM intake_outbox
          WHERE replay_disposition IN ('started_unclassified','startup_failed_no_effect','withheld')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(held, 0, "ordinary dormant deployment still claims work");
    pool.close().await;
    fixture.drop().await;
}
