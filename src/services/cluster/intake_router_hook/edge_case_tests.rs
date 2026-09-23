//! Cross-boundary regressions for per-agent placement; every fixture is an isolated DB.
use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_session_owner,
    seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::execution_requirements::tests::ready_node;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

fn capabilities(id: &str, os: &str) -> Value {
    let mut caps = ready_node(id, os)["capabilities"].clone();
    caps["execution_capacity"] = json!({"version":1,"slots":1});
    caps
}

async fn fixture(channel: &str) -> (TestPostgresDb, PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "edge-agent", channel, json!([])).await;
    sqlx::query("UPDATE agents SET default_execution_node_id='win-primary', execution_requirements=$1 WHERE id='edge-agent'")
        .bind(json!({"os":["windows"],"backends":["process"]}))
        .execute(&pool).await.unwrap();
    for (id, os) in [
        ("leader-1", "macos"),
        ("win-primary", "windows"),
        ("win-backup", "windows"),
    ] {
        seed_worker_node_with_capabilities(&pool, id, json!([]), "online", capabilities(id, os))
            .await;
    }
    (fixture, pool)
}

fn forwarded(decision: IntakeRouterDecision, target: &str) -> i64 {
    match decision {
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
            ..
        } => {
            assert_eq!(target_instance_id, target);
            outbox_id
        }
        other => panic!("expected {target}, got {other:?}"),
    }
}

async fn complete(pool: &PgPool, id: i64) {
    sqlx::query("UPDATE intake_outbox SET status='done', completed_at=now() WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn primary_readiness_loss_and_removed_registration_only_use_compatible_fallback_pg() {
    let (fixture, pool) = fixture("9200").await;
    // A heartbeat can remain fresh while a credential, poller, CLI, or probe expires.
    for (index, (pointer, value)) in [
        ("/execution_readiness/expires_at_ms", json!(0)),
        (
            "/execution_readiness/providers/claude/cli_usable",
            json!(false),
        ),
        (
            "/execution_readiness/providers/claude/credential_profiles/default",
            json!(false),
        ),
        ("/intake_poller/claude", json!(0)),
        ("/execution_capacity/slots", json!(0)),
        ("/execution_readiness/os", json!("linux")),
        ("/execution_readiness/backends", json!([])),
    ]
    .into_iter()
    .enumerate()
    {
        let mut caps = capabilities("win-primary", "windows");
        *caps.pointer_mut(pointer).unwrap() = value;
        sqlx::query("UPDATE worker_nodes SET capabilities=$1 WHERE instance_id='win-primary'")
            .bind(caps)
            .execute(&pool)
            .await
            .unwrap();
        let channel = format!("9201{index}");
        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, &channel);
        ctx.policy_channel_id = "9200";
        let id = forwarded(try_route_intake(&pool, &ctx).await, "win-backup");
        let policy: Value =
            sqlx::query_scalar("SELECT execution_requirements FROM intake_outbox WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            policy,
            json!({"os":["windows"],"backends":["process"]}),
            "{pointer}"
        );
        complete(&pool, id).await;
    }
    // Previously selected registrations can disappear; the saved preference is soft.
    sqlx::query("DELETE FROM worker_nodes WHERE instance_id='win-primary'")
        .execute(&pool)
        .await
        .unwrap();
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "92099");
    ctx.policy_channel_id = "9200";
    let id = forwarded(try_route_intake(&pool, &ctx).await, "win-backup");
    complete(&pool, id).await;
    let saved: String =
        sqlx::query_scalar("SELECT default_execution_node_id FROM agents WHERE id='edge-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved, "win-primary");
    // A hard node requirement must never inherit the preference's fallback behavior.
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='edge-agent'")
        .bind(json!({"nodes":["win-primary"]}))
        .execute(&pool)
        .await
        .unwrap();
    ctx.channel_id = "92100";
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE channel_id='92100'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_agent_primary_race_reserves_two_slots_and_reuses_a_released_slot_pg() {
    let (fixture, pool) = fixture("9300").await;
    assert!(!super::super::execution_capacity::automatic_enabled());
    let barrier = Arc::new(tokio::sync::Barrier::new(12));
    let mut jobs = tokio::task::JoinSet::new();
    for index in 0..12 {
        let pool = pool.clone();
        let barrier = barrier.clone();
        jobs.spawn(async move {
            let channel = format!("9301{index}");
            let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, &channel);
            ctx.policy_channel_id = "9300";
            barrier.wait().await;
            try_route_intake(&pool, &ctx).await
        });
    }
    let mut admitted = BTreeSet::new();
    let mut blocked = 0;
    while let Some(result) = jobs.join_next().await {
        match result.unwrap() {
            IntakeRouterDecision::Forwarded {
                target_instance_id, ..
            } => {
                assert!(
                    admitted.insert(target_instance_id),
                    "a slot was admitted twice"
                );
            }
            IntakeRouterDecision::Blocked { .. } => blocked += 1,
            other => panic!("must not run on incompatible leader: {other:?}"),
        }
    }
    assert_eq!(
        admitted,
        BTreeSet::from(["win-primary".into(), "win-backup".into()])
    );
    assert_eq!(blocked, 10);
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT target_instance_id,count(*) FROM intake_outbox GROUP BY target_instance_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(_, count)| *count == 1));
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE target_instance_id='win-primary'")
        .execute(&pool)
        .await
        .unwrap();
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "93099");
    ctx.policy_channel_id = "9300";
    forwarded(try_route_intake(&pool, &ctx).await, "win-primary");
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_message_race_and_replay_after_primary_change_do_not_execute_twice_pg() {
    let (fixture, pool) = fixture("9400").await;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let barrier = barrier.clone();
        jobs.spawn(async move {
            let ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "9400");
            barrier.wait().await;
            try_route_intake(&pool, &ctx).await
        });
    }
    let mut row = None;
    let mut duplicates = 0;
    while let Some(result) = jobs.join_next().await {
        match result.unwrap() {
            IntakeRouterDecision::Forwarded { outbox_id, .. } => {
                assert!(row.replace(outbox_id).is_none());
            }
            IntakeRouterDecision::SkippedDuplicate { .. } => duplicates += 1,
            other => {
                panic!("duplicate must not execute locally or create another route: {other:?}")
            }
        }
    }
    assert_eq!(duplicates, 7);
    complete(&pool, row.unwrap()).await;
    sqlx::query("UPDATE agents SET default_execution_node_id='win-backup' WHERE id='edge-agent'")
        .execute(&pool)
        .await
        .unwrap();
    let ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "9400");
    assert!(matches!(
        try_route_intake(&pool, &ctx).await,
        IntakeRouterDecision::SkippedDuplicate { .. }
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_owner_overrides_new_primary_and_offline_or_conflicting_owners_block_pg() {
    let (fixture, pool) = fixture("9500").await;
    seed_session_owner(
        &pool,
        "claude:edge-owner",
        "claude",
        "9500",
        "win-backup",
        "idle",
    )
    .await;
    // Idle session heartbeats are not leases; a fresh worker still owns the conversation.
    sqlx::query("UPDATE sessions SET last_heartbeat=now()-interval '7 days'")
        .execute(&pool)
        .await
        .unwrap();
    let ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "9500");
    let decision = try_route_intake(&pool, &ctx).await;
    assert!(matches!(
        &decision,
        IntakeRouterDecision::Forwarded {
            basis: IntakeRoutingBasis::LiveForeignOwner,
            ..
        }
    ));
    let id = forwarded(decision, "win-backup");
    complete(&pool, id).await;
    sqlx::query("UPDATE worker_nodes SET status='offline' WHERE instance_id='win-backup'")
        .execute(&pool)
        .await
        .unwrap();
    let mut next = ctx_for_channel(IntakeRoutingMode::Enforce, "9500");
    next.user_msg_id = "10000";
    assert!(matches!(
        try_route_intake(&pool, &next).await,
        IntakeRouterDecision::Blocked {
            reason: IntakeBlockedReason::StaleSessionOwners { .. }
        }
    ));
    sqlx::query("UPDATE worker_nodes SET status='online' WHERE instance_id='win-backup'")
        .execute(&pool)
        .await
        .unwrap();
    seed_session_owner(
        &pool,
        "claude:edge-conflict",
        "claude",
        "9500",
        "win-primary",
        "idle",
    )
    .await;
    assert!(matches!(
        try_route_intake(&pool, &next).await,
        IntakeRouterDecision::Blocked {
            reason: IntakeBlockedReason::ConflictingLiveSessionOwners { .. }
        }
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "blocked inputs must not create an executable row");
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_override_cannot_bypass_requirements_or_silently_use_primary_pg() {
    let (fixture, pool) = fixture("9600").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "9600");
    for explicit in ["leader-1", "  leader-1  ", "missing-node"] {
        ctx.node_override_instance_id = Some(explicit);
        assert!(
            matches!(
                try_route_intake(&pool, &ctx).await,
                IntakeRouterDecision::Blocked { .. }
            ),
            "{explicit}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    ctx.node_override_instance_id = Some("   ");
    forwarded(try_route_intake(&pool, &ctx).await, "win-primary");
    pool.close().await;
    fixture.drop().await;
}
