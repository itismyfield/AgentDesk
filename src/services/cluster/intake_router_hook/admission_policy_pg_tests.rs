//! Admission policy reads go through the production router, including its owner and O fences.
use super::pg_tests::{ctx_for_channel, seed_agent_with_preference, seed_session_owner};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;
use serde_json::json;
use std::time::Duration;

fn policy_reads(proxy: &PgWireProxy) -> Vec<String> {
    proxy
        .executions()
        .into_iter()
        .filter(|sql| {
            let sql = sql.to_ascii_lowercase();
            sql.contains("from agents")
                && (sql.contains("execution_requirements")
                    || sql.contains("default_execution_node_id"))
        })
        .collect()
}

fn dependency_failure(decision: IntakeRouterDecision) -> String {
    match decision {
        IntakeRouterDecision::Blocked {
            reason: IntakeBlockedReason::RoutingDependencyFailed { detail },
        } => detail,
        other => panic!("expected policy dependency block, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_owner_route_reads_requirement_and_default_once_pg() {
    let fixture = TestPostgresDb::create().await;
    let setup = fixture.connect_and_migrate().await;
    let channel = "policy-query-budget";
    seed_agent_with_preference(&setup, "budget-agent", channel, json!([])).await;
    seed_session_owner(
        &setup,
        "claude:policy-query-budget",
        "claude",
        channel,
        "leader-1",
        "idle",
    )
    .await;
    let proxy = PgWireProxy::start(&fixture.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);
    sqlx::query("SELECT 1").execute(&pool).await.unwrap();
    proxy.reset_executions();

    let decision =
        try_route_intake(&pool, &ctx_for_channel(IntakeRoutingMode::Enforce, channel)).await;
    assert_eq!(
        decision,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::LiveSessionOwnerIsLocal,
        }
    );
    let reads = policy_reads(&proxy);
    assert_eq!(reads.len(), 1, "one admission policy read");

    pool.close().await;
    setup.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_policy_preserves_mode_parse_and_missing_defaults_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let channel = "policy-mode-and-parse";
    let disabled = ctx_for_channel(IntakeRoutingMode::Disabled, channel);
    let hook_disabled = IntakeRouterDecision::RanLocal {
        reason: RanLocalReason::HookDisabled,
    };
    assert_eq!(try_route_intake(&pool, &disabled).await, hook_disabled);
    seed_agent_with_preference(&pool, "mode-agent", channel, json!([])).await;
    assert_eq!(try_route_intake(&pool, &disabled).await, hook_disabled);

    for mode in [IntakeRoutingMode::Disabled, IntakeRoutingMode::Observe] {
        let ctx = ctx_for_channel(mode, channel);
        sqlx::query(
            "UPDATE agents SET default_execution_node_id='policy-default' WHERE id='mode-agent'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            dependency_failure(try_route_intake(&pool, &ctx).await),
            "agent default execution node requires enforce routing"
        );
        sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='mode-agent'")
            .bind(json!({"os":["windows"]}))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            dependency_failure(try_route_intake(&pool, &ctx).await),
            "hard execution requirements require enforce routing"
        );
        sqlx::query("UPDATE agents SET execution_requirements='{}'::jsonb WHERE id='mode-agent'")
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='mode-agent'")
        .bind(json!({"os":"windows"}))
        .execute(&pool)
        .await
        .unwrap();
    let ctx = ctx_for_channel(IntakeRoutingMode::Enforce, channel);
    assert!(
        dependency_failure(try_route_intake(&pool, &ctx).await)
            .starts_with("execution policy lookup failed:")
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_policy_ambiguous_binding_uses_both_legacy_reads_pg() {
    let fixture = TestPostgresDb::create().await;
    let setup = fixture.connect_and_migrate().await;
    let channel = "policy-ambiguous";
    for agent in ["ambiguous-one", "ambiguous-two"] {
        seed_agent_with_preference(&setup, agent, channel, json!([])).await;
    }
    let proxy = PgWireProxy::start(&fixture.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);
    let decision = try_route_intake(
        &pool,
        &ctx_for_channel(IntakeRoutingMode::Disabled, channel),
    )
    .await;
    assert_eq!(
        decision,
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::HookDisabled,
        }
    );
    let reads = policy_reads(&proxy);
    for legacy in [
        "select execution_requirements from agents",
        "select default_execution_node_id from agents",
    ] {
        assert!(
            reads
                .iter()
                .any(|sql| sql.to_ascii_lowercase().contains(legacy)),
            "ambiguous bindings retain both independent legacy policy lookups"
        );
    }
    pool.close().await;
    setup.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_policy_snapshot_error_preserves_legacy_failure_precedence_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let channel = "policy-lookup-errors";
    seed_agent_with_preference(&pool, "error-agent", channel, json!([])).await;
    sqlx::query("UPDATE agents SET execution_requirements=$1 WHERE id='error-agent'")
        .bind(json!({"os":["windows"]}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE agents DROP COLUMN default_execution_node_id")
        .execute(&pool)
        .await
        .unwrap();
    let ctx = ctx_for_channel(IntakeRoutingMode::Observe, channel);
    assert_eq!(
        dependency_failure(try_route_intake(&pool, &ctx).await),
        "hard execution requirements require enforce routing"
    );
    sqlx::query("UPDATE agents SET execution_requirements='{}'::jsonb WHERE id='error-agent'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        dependency_failure(try_route_intake(&pool, &ctx).await)
            .starts_with("agent execution node lookup failed:")
    );
    sqlx::query("ALTER TABLE agents DROP COLUMN execution_requirements")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        dependency_failure(try_route_intake(&pool, &ctx).await)
            .starts_with("execution policy lookup failed:")
    );
    pool.close().await;
    fixture.drop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn admission_policy_keeps_o_and_legacy_sibling_routes_pg() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};

    let fixture = TestPostgresDb::create().await;
    let setup = fixture.connect_and_migrate().await;
    let channels = ["82073001", "82073002"];
    for channel in channels {
        seed_agent_with_preference(&setup, &format!("policy-{channel}"), channel, json!([])).await;
        seed_session_owner(
            &setup,
            &format!("claude:policy-{channel}"),
            "claude",
            channel,
            "leader-1",
            "idle",
        )
        .await;
    }
    let _selected = test_override::force_channels(&[(82_073_001, ClaudeTui)]);
    let _ready = test_probe::answer_with(|_| true);
    let proxy = PgWireProxy::start(&fixture.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);
    for channel in channels {
        proxy.reset_executions();
        assert_eq!(
            try_route_intake(&pool, &ctx_for_channel(IntakeRoutingMode::Enforce, channel)).await,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::LiveSessionOwnerIsLocal,
            }
        );
        assert_eq!(policy_reads(&proxy).len(), 1);
    }
    pool.close().await;
    setup.close().await;
    fixture.drop().await;
}
