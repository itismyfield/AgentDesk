use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_session_owner,
    seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::*;
use crate::services::cluster::execution_requirements::tests::ready_node;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_requirements_block_fallback_and_survive_recovery_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "hard-agent", "8111", json!([])).await;
    let policy = json!({"os":["windows"],"nodes":["win"],"repositories":["kunkunGames/AgentDesk"]});
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='hard-agent'")
        .bind(&policy)
        .execute(&pool)
        .await
        .unwrap();
    for (id, os, status) in [
        ("leader-1", "macos", "online"),
        ("win", "windows", "offline"),
    ] {
        seed_worker_node_with_capabilities(
            &pool,
            id,
            json!([]),
            status,
            ready_node(id, os)["capabilities"].clone(),
        )
        .await;
    }
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "8111");
    for mode in [
        IntakeRoutingMode::Disabled,
        IntakeRoutingMode::Observe,
        IntakeRoutingMode::Enforce,
    ] {
        ctx.mode = mode;
        assert!(matches!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked { .. }
        ));
    }
    ctx.node_override_instance_id = Some("leader-1");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    ctx.node_override_instance_id = None;
    sqlx::query("UPDATE worker_nodes SET status='online' WHERE instance_id='win'")
        .execute(&pool)
        .await
        .unwrap();
    let decision = try_route_intake(&pool, &ctx).await;
    let id = match decision {
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
            ..
        } => {
            assert_eq!(target_instance_id, "win");
            outbox_id
        }
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::SkippedDuplicate { .. }
    ));
    let claimed = claim_pending_for_target(&pool, "win", "claude", "worker-test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.execution_requirements, policy);
    assert_eq!(claimed.id, id);
    assert!(
        crate::services::cluster::execution_requirements::validate_worker(&claimed).is_err(),
        "an unprobed receiver cannot accept the persisted hard policy"
    );
    mark_failed_pre_accept(&pool, id, "worker-test", "fixture unavailable")
        .await
        .unwrap();
    sqlx::query("UPDATE worker_nodes SET status='offline' WHERE instance_id='win'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        sweep_failed_pre_accept_once(&pool, "leader-1", 4, 60, None)
            .await
            .unwrap(),
        FailedPreAcceptSweepOutcome::NoCapableTarget { .. }
    ));
    sqlx::query(
        "UPDATE worker_nodes SET status='online',last_heartbeat_at=NOW() WHERE instance_id='win'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let child = match sweep_failed_pre_accept_once(&pool, "leader-1", 4, 60, None)
        .await
        .unwrap()
    {
        FailedPreAcceptSweepOutcome::Retried { new_id, .. } => new_id,
        other => panic!("{other:?}"),
    };
    let persisted: serde_json::Value =
        sqlx::query_scalar("SELECT execution_requirements FROM intake_outbox WHERE id=$1")
            .bind(child)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(persisted, policy);
    let row = claim_pending_for_target(&pool, "win", "claude", "worker-test")
        .await
        .unwrap()
        .unwrap();
    assert!(mark_accepted(&pool, row.id, "worker-test").await.unwrap());
    assert!(mark_spawned(&pool, row.id, "worker-test").await.unwrap());
    let retry = crate::db::intake_outbox_force_fail::force_fail_and_retry_as_new(
        &pool,
        row.id,
        "fixture retry",
    )
    .await
    .unwrap();
    let persisted: serde_json::Value =
        sqlx::query_scalar("SELECT execution_requirements FROM intake_outbox WHERE id=$1")
            .bind(retry)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(persisted, policy);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_requirements_reject_incompatible_existing_owner_without_reassignment_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "owner-agent", "8222", json!([])).await;
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='owner-agent'")
        .bind(json!({"os":["windows"]}))
        .execute(&pool)
        .await
        .unwrap();
    for (id, os) in [("leader-1", "macos"), ("win", "windows")] {
        seed_worker_node_with_capabilities(
            &pool,
            id,
            json!([]),
            "online",
            ready_node(id, os)["capabilities"].clone(),
        )
        .await;
    }
    seed_session_owner(
        &pool,
        "claude:required-owner",
        "claude",
        "8222",
        "leader-1",
        "turn_active",
    )
    .await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "8222");
    ctx.node_override_instance_id = Some("win");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
    fixture.drop().await;
}
