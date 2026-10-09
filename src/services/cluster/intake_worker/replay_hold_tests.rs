//! Replay authority remains in PostgreSQL when a worker or its in-memory runtime is replaced.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{
    FailedPreAcceptSweepOutcome, InsertPendingPayload, insert_pending, sweep_failed_pre_accept_once,
};
use crate::services::discord::health::owner_runtime_for_tests;
use test_executor::Checkpoint;

const WORKER: &str = "replay-worker";

async fn seed_agent(pool: &PgPool) {
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('replay-agent', 'Test', 'claude', 'unused')")
        .execute(pool)
        .await
        .unwrap();
}

fn payload(channel: u64, message: u64, target: &str) -> InsertPendingPayload {
    InsertPendingPayload {
        target_instance_id: target.into(),
        forwarded_by_instance_id: "replay-leader".into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.to_string(),
        user_msg_id: message.to_string(),
        request_owner_id: "100".into(),
        request_owner_name: Some("Tester".into()),
        user_text: "preserve the original request".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "standard".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "replay-agent".into(),
        provider: "claude".into(),
        home_epoch: None,
    }
}

async fn receipt(pool: &PgPool, channel: u64, sources: &[u64], disposition: &str) -> i64 {
    let sources: Vec<String> = sources.iter().map(u64::to_string).collect();
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status,
            replay_only, replay_disposition, replay_source_message_ids, replay_request_key,
            replay_hold_reason, replay_preserved
         ) VALUES ('receipt-only', 'replay-leader', $1, $2, '100',
                   'preserve the original request', 'standard', 'replay-agent', 'claude',
                   'unknown', TRUE, $3, $4, $5, 'classified partial output',
                   '{\"partial_text\":\"keep this result\",\"delivery_owed\":true}'::JSONB)
         RETURNING id",
    )
    .bind(channel.to_string())
    .bind(&sources[0])
    .bind(disposition)
    .bind(&sources)
    .bind(format!("worker-replay-{channel}-{}", sources[0]))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn pending_without_provider_effect(pool: &PgPool, row: i64) {
    let state: (String, Option<String>, bool, bool, String) = sqlx::query_as(
        "SELECT status::TEXT, claim_owner, accepted_at IS NULL, spawned_at IS NULL, user_text
           FROM intake_outbox WHERE id = $1",
    )
    .bind(row)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        state,
        (
            "pending".into(),
            None,
            true,
            true,
            "preserve the original request".into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn replay_hold_survives_worker_replacement_and_foreign_claim_owners_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    seed_agent(&pool).await;
    let recorder = test_executor::record();
    for (offset, disposition) in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 6_008_610 + offset as u64;
        let first_source = channel * 10;
        let absorbed_source = first_source + 1;
        let target = if offset == 1 {
            "foreign-worker"
        } else {
            WORKER
        };
        let row = insert_pending(&pool, &payload(channel, absorbed_source, target), 1, None)
            .await
            .unwrap();
        let receipt = receipt(
            &pool,
            channel,
            &[first_source, absorbed_source],
            disposition,
        )
        .await;
        let (registry, shared) = owner_runtime_for_tests::registered("claude").await;
        assert_eq!(
            run_intake_worker_tick(&pool, &shared, target, "claude", "old-owner", &|| false)
                .await
                .unwrap(),
            TickOutcome::QueueEmpty,
            "{disposition} excludes every absorbed source before claim"
        );
        drop((registry, shared));
        let (_replacement_registry, replacement) =
            owner_runtime_for_tests::registered("claude").await;
        for owner in ["replacement-owner", "another-node-owner"] {
            assert_eq!(
                run_intake_worker_tick(&pool, &replacement, target, "claude", owner, &|| false)
                    .await
                    .unwrap(),
                TickOutcome::QueueEmpty,
                "a fresh runtime or foreign claim owner cannot forget {disposition}"
            );
        }
        pending_without_provider_effect(&pool, row).await;
        let preserved: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT replay_disposition, user_text, replay_preserved FROM intake_outbox WHERE id=$1",
        )
        .bind(receipt)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(preserved.0, disposition);
        assert_eq!(preserved.1, "preserve the original request");
        assert_eq!(preserved.2["partial_text"], "keep this result");
        assert_eq!(preserved.2["delivery_owed"], true);
    }
    assert!(
        recorder.channels().is_empty(),
        "no held source starts a provider"
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replay_hold_committed_after_worker_claim_blocks_accept_and_provider_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    seed_agent(&pool).await;
    let channel = 6_008_620;
    let message = channel * 10;
    let receipt = receipt(&pool, channel, &[message], "registered_not_started").await;
    let row = insert_pending(&pool, &payload(channel, message, WORKER), 1, None)
        .await
        .unwrap();
    let (_registry, shared) = owner_runtime_for_tests::registered("claude").await;
    let held_pool = pool.clone();
    let _hook = test_executor::hook(Box::new(move |seen| {
        let pool = held_pool.clone();
        Box::pin(async move {
            if seen == Checkpoint::AfterClaim {
                sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld' WHERE id=$1")
                    .bind(receipt)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        })
    }));
    let recorder = test_executor::record();
    let outcome =
        run_intake_worker_tick(&pool, &shared, WORKER, "claude", "race-owner", &|| false).await;
    assert_eq!(
        outcome.unwrap(),
        TickOutcome::LostClaimBeforeAccept,
        "a committed hold refuses accept and rollback without weakening authority"
    );
    let state: (String, Option<String>, bool, bool, String) = sqlx::query_as(
        "SELECT status::TEXT, claim_owner, accepted_at IS NULL, spawned_at IS NULL, user_text
           FROM intake_outbox WHERE id=$1",
    )
    .bind(row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        state,
        (
            "claimed".into(),
            Some("race-owner".into()),
            true,
            true,
            "preserve the original request".into()
        )
    );
    let held_receipt: (String, String, serde_json::Value) = sqlx::query_as(
        "SELECT status::TEXT, replay_disposition, replay_preserved
           FROM intake_outbox WHERE id=$1",
    )
    .bind(receipt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(held_receipt.0, "unknown");
    assert_eq!(held_receipt.1, "withheld");
    assert_eq!(held_receipt.2["partial_text"], "keep this result");
    assert_eq!(held_receipt.2["delivery_owed"], true);
    assert!(
        recorder.channels().is_empty(),
        "no provider executes after the hold commits"
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replay_schema_preserves_normal_worker_delivery_and_preaccept_attempt_budget_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    seed_agent(&pool).await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, labels, capabilities, last_heartbeat_at)
         VALUES ($1, 'online', '[]', '{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"]}}', NOW())",
    )
    .bind(WORKER)
    .execute(&pool)
    .await
    .unwrap();
    let (_registry, shared) = owner_runtime_for_tests::registered("claude").await;
    let recorder = test_executor::record();
    let delivered_channel = 6_008_630;
    let delivered_message = delivered_channel * 10;
    receipt(
        &pool,
        delivered_channel,
        &[delivered_message],
        "registered_not_started",
    )
    .await;
    let delivered_row = insert_pending(
        &pool,
        &payload(delivered_channel, delivered_message, WORKER),
        1,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        run_intake_worker_tick(&pool, &shared, WORKER, "claude", "normal-owner", &|| false)
            .await
            .unwrap(),
        TickOutcome::Processed
    );
    let delivered: (String, bool, bool, bool, i32) = sqlx::query_as(
        "SELECT status::TEXT, accepted_at IS NOT NULL, spawned_at IS NOT NULL,
                completed_at IS NOT NULL, attempt_no FROM intake_outbox WHERE id=$1",
    )
    .bind(delivered_row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(delivered, ("done".into(), true, true, true, 1));
    assert_eq!(recorder.channels(), vec![delivered_channel]);

    let retry_channel = 6_008_631;
    let retry_message = retry_channel * 10;
    let replay_receipt = receipt(
        &pool,
        retry_channel,
        &[retry_message],
        "registered_not_started",
    )
    .await;
    let mut retry_payload = payload(retry_channel, retry_message, WORKER);
    retry_payload.request_owner_id = "invalid-snowflake".into();
    let mut row = insert_pending(&pool, &retry_payload, 1, None)
        .await
        .unwrap();
    for attempt in 1..=3 {
        assert_eq!(
            run_intake_worker_tick(&pool, &shared, WORKER, "claude", "retry-owner", &|| false)
                .await
                .unwrap(),
            TickOutcome::Processed
        );
        let failed: (String, i32, i32, bool, bool, String) = sqlx::query_as(
            "SELECT status::TEXT, attempt_no, retry_count, accepted_at IS NULL,
                    spawned_at IS NULL, last_error FROM intake_outbox WHERE id=$1",
        )
        .bind(row)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(failed.0, "failed_pre_accept");
        assert_eq!(failed.1, attempt);
        assert_eq!(failed.2, 1);
        assert!(failed.3 && failed.4);
        assert!(failed.5.starts_with("payload conversion:"));
        let sweep = sweep_failed_pre_accept_once(&pool, "replay-leader", 3, 60, None)
            .await
            .unwrap();
        if attempt < 3 {
            let FailedPreAcceptSweepOutcome::Retried {
                source_id,
                new_id,
                attempt_no,
            } = sweep
            else {
                panic!("normal failed-preaccept retry changed under new schema: {sweep:?}");
            };
            assert_eq!(source_id, row);
            assert_eq!(attempt_no, attempt + 1);
            row = new_id;
        } else {
            assert_eq!(
                sweep,
                FailedPreAcceptSweepOutcome::BudgetExhausted {
                    source_id: row,
                    attempt_no: 3
                }
            );
        }
    }
    let family: Vec<(i32, bool, Option<i64>)> = sqlx::query_as(
        "SELECT attempt_no, replay_only, parent_outbox_id FROM intake_outbox
          WHERE channel_id=$1 AND user_msg_id=$2 ORDER BY replay_only, attempt_no",
    )
    .bind(retry_channel.to_string())
    .bind(retry_message.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        family.len(),
        4,
        "receipt plus exactly three delivery attempts"
    );
    assert_eq!(
        family
            .iter()
            .filter(|r| !r.1)
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(family.last().unwrap(), &(1, true, None));
    assert!(
        family.iter().all(|r| r.2 != Some(replay_receipt)),
        "receipt never parents a retry"
    );
    assert_eq!(
        recorder.channels(),
        vec![delivered_channel],
        "only normal delivery starts a provider"
    );
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM intake_outbox
          WHERE replay_disposition IN ('started_unclassified','startup_failed_no_effect','withheld')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        held, 0,
        "this is a dormant and mixed-version runtime fixture"
    );
    pool.close().await;
    fixture.drop().await;
}
