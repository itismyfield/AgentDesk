use super::*;

#[tokio::test]
async fn dormant_schema_keeps_runtime_delivery_and_preaccept_retry_attempt_policy_pg() {
    let (fixture, pool) = setup().await;
    let first = intake_outbox::insert_pending(
        &pool,
        &payload("normal-delivery", "normal-message"),
        1,
        None,
    )
    .await
    .expect("existing admission API under new schema");
    let claimed =
        intake_outbox::claim_pending_for_target(&pool, "worker-a", "claude", "worker-a:claude")
            .await
            .unwrap()
            .expect("normal claim still succeeds");
    assert_eq!(claimed.id, first);
    assert!(
        intake_outbox::mark_accepted(&pool, first, "worker-a:claude")
            .await
            .unwrap()
    );
    assert!(
        intake_outbox::mark_spawned(&pool, first, "worker-a:claude")
            .await
            .unwrap()
    );
    assert!(
        intake_outbox::mark_done(&pool, first, "worker-a:claude")
            .await
            .unwrap()
    );
    sqlx::query("INSERT INTO auto_queue_runs (id,status) VALUES ('normal-run','active')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO auto_queue_entries (id,run_id,status) VALUES ('normal-entry','normal-run','dispatched')")
        .execute(&pool).await.unwrap();
    let result = crate::db::auto_queue::record_entry_dispatch_failure_on_pg(
        &pool,
        "normal-entry",
        3,
        "normal-policy",
    )
    .await
    .expect("normal AutoQueue retry still runs");
    assert!(result.changed);
    assert_eq!(result.to_status, "pending");
    sqlx::query("INSERT INTO task_dispatches (id,status) VALUES ('normal-dispatch','pending')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        crate::db::dispatches::outbox::dispatch_notify_delivery_suppressed_pg(
            &pool,
            "normal-dispatch",
            "notify",
        )
        .await
        .unwrap()
        .is_none()
    );
    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id,action) VALUES ('normal-dispatch','notify')",
    )
    .execute(&pool)
    .await
    .expect("normal dispatch notify remains available");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );

    // A registered replay-only receipt at attempt one does not consume normal attempts 1/2/3.
    receipt_on(
        &pool,
        "retry-delivery",
        "claude",
        &["retry-message"],
        "registered_not_started",
    )
    .await
    .expect("dormant receipt coexists with the first normal attempt");
    sqlx::query("INSERT INTO worker_nodes (instance_id,status,labels,capabilities,last_heartbeat_at)
                 VALUES ('worker-a','online','[]','{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"]}}',NOW())")
        .execute(&pool).await.unwrap();
    let mut current =
        intake_outbox::insert_pending(&pool, &payload("retry-delivery", "retry-message"), 1, None)
            .await
            .expect("normal tuple unique index excludes replay-only attempt one");
    for expected_attempt in [2, 3] {
        let claimed =
            intake_outbox::claim_pending_for_target(&pool, "worker-a", "claude", "worker-a:claude")
                .await
                .unwrap()
                .expect("preaccept worker claim");
        assert_eq!(claimed.id, current);
        assert!(
            intake_outbox::mark_failed_pre_accept(
                &pool,
                current,
                "worker-a:claude",
                "transient validation"
            )
            .await
            .unwrap()
        );
        sqlx::query("UPDATE intake_outbox SET admission_kind='local' WHERE id=$1")
            .bind(current)
            .execute(&pool)
            .await
            .unwrap();
        let outcome = intake_outbox::sweep_failed_pre_accept_once(&pool, "leader", 3, 60, None)
            .await
            .expect("existing sweep chooses normal family maximum");
        let FailedPreAcceptSweepOutcome::Retried {
            source_id,
            new_id,
            attempt_no,
        } = outcome
        else {
            panic!("expected normal retry {expected_attempt}, observed {outcome:?}")
        };
        assert_eq!(source_id, current);
        assert_eq!(attempt_no, expected_attempt);
        current = new_id;
    }
    assert_eq!(
        intake_outbox::family_max_attempt(&pool, "retry-delivery", "retry-message")
            .await
            .unwrap(),
        3
    );
    let claimed =
        intake_outbox::claim_pending_for_target(&pool, "worker-a", "claude", "worker-a:claude")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(claimed.id, current);
    assert!(
        intake_outbox::mark_failed_pre_accept(&pool, current, "worker-a:claude", "budget limit")
            .await
            .unwrap()
    );
    assert!(!matches!(
        intake_outbox::sweep_failed_pre_accept_once(&pool, "leader", 3, 60, None)
            .await
            .unwrap(),
        FailedPreAcceptSweepOutcome::Retried { .. }
    ));
    finish(fixture, pool).await;
}
