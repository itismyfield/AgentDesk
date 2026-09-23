use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_session_owner,
    seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::execution_requirements::tests::ready_node;
use serde_json::json;

async fn seed_ready(pool: &PgPool, id: &str, os: &str, status: &str) {
    let mut caps = ready_node(id, os)["capabilities"].clone();
    caps["execution_capacity"] = json!({"version":1,"slots":1});
    seed_worker_node_with_capabilities(pool, id, json!([]), status, caps).await;
}

async fn set_primary(pool: &PgPool, primary: &str) {
    sqlx::query("UPDATE agents SET default_execution_node_id=$1 WHERE id='default-agent'")
        .bind(primary)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_primary_inherits_into_thread_with_bounded_fallback_without_moving_owner_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "default-agent", "8331", json!([])).await;
    sqlx::query("UPDATE agents SET default_execution_node_id='win' WHERE id='default-agent'")
        .execute(&pool)
        .await
        .unwrap();
    for (id, os) in [
        ("leader-1", "macos"),
        ("win", "windows"),
        ("linux", "linux"),
    ] {
        seed_ready(&pool, id, os, "online").await;
    }
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "8332");
    ctx.policy_channel_id = "8331";
    let id = match try_route_intake(&pool, &ctx).await {
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
    let snapshot: (String, String, serde_json::Value) = sqlx::query_as(
        "SELECT channel_id, agent_id, execution_requirements FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(snapshot, ("8332".into(), "default-agent".into(), json!({})));
    let central: serde_json::Value =
        sqlx::query_scalar("SELECT execution_requirements FROM agents WHERE id='default-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(central, json!({}));

    // The primary's slot is reserved. Another fresh thread can use Linux.
    ctx.channel_id = "8335";
    assert!(matches!(try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Forwarded { target_instance_id, .. } if target_instance_id == "linux"));
    ctx.channel_id = "8336";
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::LeaderIsOnlyEligible
        }
    ));

    // A different pre-existing conversation on this agent keeps its Mac owner.
    seed_session_owner(
        &pool,
        "claude:default-owner",
        "claude",
        "8333",
        "leader-1",
        "turn_active",
    )
    .await;
    ctx.channel_id = "8333";
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::LiveSessionOwnerIsLocal
        }
    ));

    // A channel override outranks the agent default for an ownerless conversation.
    ctx.channel_id = "8334";
    ctx.node_override_instance_id = Some("win");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    ctx.node_override_instance_id = Some("leader-1");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::NodeOverrideIsLeader
        }
    ));
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_primary_uses_only_ready_compatible_candidates_and_never_moves_stale_owner_pg()
{
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "default-agent", "8441", json!([])).await;
    set_primary(&pool, "win").await;
    seed_ready(&pool, "win", "windows", "offline").await;
    seed_ready(&pool, "leader-1", "macos", "online").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "8441");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::LeaderIsOnlyEligible
        }
    ));

    // A stale owner is not no owner, even if the leader is healthy.
    seed_session_owner(
        &pool,
        "claude:offline-owner",
        "claude",
        "8442",
        "win",
        "turn_active",
    )
    .await;
    ctx.channel_id = "8442";
    ctx.policy_channel_id = "8441";
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    ctx.channel_id = "8441";

    // Required OS rules out the otherwise available Mac fallback.
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='default-agent'")
        .bind(json!({"os":["windows"]}))
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    sqlx::query("UPDATE agents SET execution_requirements='{}' WHERE id='default-agent'")
        .execute(&pool)
        .await
        .unwrap();

    // Heartbeat alone cannot prove a fallback can execute a turn.
    sqlx::query("UPDATE worker_nodes SET capabilities=capabilities-'execution_readiness' WHERE instance_id='leader-1'")
        .execute(&pool).await.unwrap();
    for mode in [
        IntakeRoutingMode::Enforce,
        IntakeRoutingMode::Disabled,
        IntakeRoutingMode::Observe,
    ] {
        ctx.mode = mode;
        assert!(matches!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked { .. }
        ));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preferred_leader_wins_but_unconfigured_agent_keeps_legacy_placement_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "default-agent", "8551", json!([])).await;
    for (id, os) in [("leader-1", "macos"), ("win", "windows")] {
        seed_ready(&pool, id, os, "online").await;
    }
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Disabled, "8551");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::HookDisabled
        }
    ));
    ctx.mode = IntakeRoutingMode::Enforce;
    assert!(!super::super::execution_capacity::automatic_enabled());
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::AgentHasNoPreference
        }
    ));
    set_primary(&pool, "leader-1").await;
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::AgentDefaultIsLeader
        }
    ));

    // A hard requirement wins over a preferred leader.
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='default-agent'")
        .bind(json!({"os":["windows"]}))
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Forwarded { target_instance_id, .. } if target_instance_id == "win"));
    pool.close().await;
    fixture.drop().await;
}
