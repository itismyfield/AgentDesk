use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_capacity_routes_concurrent_channels_without_overflow_pg() {
    struct Restore(crate::config::Config);
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::config_live_reload::install(self.0.clone());
        }
    }
    let previous = crate::config_live_reload::current()
        .map(|c| (*c).clone())
        .unwrap_or_default();
    let _restore = Restore(previous.clone());
    let mut config = previous;
    config.cluster.intake_routing.capacity_aware = true;
    crate::config_live_reload::install(config);
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    for node in ["a-windows", "z-linux"] {
        seed_worker_node_with_capabilities(
            &pool,
            node,
            json!(["worker"]),
            "online",
            json!({
                "intake_worker":{"enabled":true,"providers":["claude"]},
                "execution_capacity":{"version":1,"slots":1}
            }),
        )
        .await;
    }
    for n in 0..8 {
        seed_agent_with_preference(
            &pool,
            &format!("agent-{n}"),
            &format!("830{n}"),
            json!(["worker"]),
        )
        .await;
    }
    let mut jobs = tokio::task::JoinSet::new();
    for n in 0..8 {
        let pool = pool.clone();
        jobs.spawn(async move {
            let channel = format!("830{n}");
            try_route_intake(
                &pool,
                &ctx_for_channel(IntakeRoutingMode::Enforce, &channel),
            )
            .await
        });
    }
    let mut nodes = std::collections::BTreeSet::new();
    while let Some(result) = jobs.join_next().await {
        match result.unwrap() {
            IntakeRouterDecision::Forwarded {
                target_instance_id, ..
            } => {
                assert!(nodes.insert(target_instance_id));
            }
            IntakeRouterDecision::Blocked { .. } => {}
            other => panic!("capacity must not fall back locally: {other:?}"),
        }
    }
    assert_eq!(
        nodes.into_iter().collect::<Vec<_>>(),
        vec!["a-windows", "z-linux"]
    );
    // An explicit owner/override stays pinned when full; the other node is not used.
    let mut pinned = ctx_for_channel(IntakeRoutingMode::Enforce, "8399");
    pinned.node_override_instance_id = Some("a-windows");
    sqlx::query("DELETE FROM intake_outbox WHERE target_instance_id='z-linux'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        try_route_intake(&pool, &pinned).await,
        IntakeRouterDecision::Blocked { .. }
    ));
    pool.close().await;
    fixture.drop().await;
}
