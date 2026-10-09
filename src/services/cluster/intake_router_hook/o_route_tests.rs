//! An O channel is placed only on the gateway whose writer can take it; other channels as before.
use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_session_owner,
    seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};
use serde_json::json;

const O: &str = "4370001";
const LEGACY: &str = "4370002";
const RUNNER: &str = "runner-4370";

#[tokio::test]
async fn unavailable_delegation_blocks_before_a_home_read_or_local_fallback() {
    use crate::services::cluster::home_availability::{self, Unavailable};
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://postgres@127.0.0.1:1/unreachable")
        .unwrap();
    let _availability =
        home_availability::install("claude", Err(Unavailable::MissingInstanceId), || {
            [O.parse().unwrap()].into()
        });
    let decision = try_route_intake(&pool, &ctx_for_channel(IntakeRoutingMode::Disabled, O)).await;
    assert!(blocked(&decision).contains("missing_instance_id"));
    assert_eq!(
        pool.size(),
        0,
        "no database read or forwarding was attempted"
    );
}

async fn outbox_rows(pool: &PgPool, channel: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
        .bind(channel)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn blocked(decision: &IntakeRouterDecision) -> &str {
    match decision {
        IntakeRouterDecision::Blocked {
            reason: IntakeBlockedReason::RoutingDependencyFailed { detail },
        } => detail,
        other => panic!("expected a routing block, got {other:?}"),
    }
}

fn forwarded_to_runner(decision: &IntakeRouterDecision) -> bool {
    matches!(decision, IntakeRouterDecision::Forwarded { target_instance_id, .. } if target_instance_id == RUNNER)
}

#[tokio::test(flavor = "current_thread")]
async fn an_o_channel_is_placed_only_on_its_ready_gateway_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let worker = json!({"intake_worker": {"enabled": true, "providers": ["claude"],
        "features": ["preserve_on_cancel_v1"]}});
    seed_worker_node_with_capabilities(&pool, RUNNER, json!([]), "online", worker).await;
    for channel in [O, LEGACY] {
        seed_agent_with_preference(&pool, &format!("agent-{channel}"), channel, json!([])).await;
        let key = format!("claude-{channel}");
        seed_session_owner(&pool, &key, "claude", channel, RUNNER, "idle").await;
    }
    let route =
        async |mode, channel| try_route_intake(&pool, &ctx_for_channel(mode, channel)).await;
    let _selected = test_override::force_channels(&[(O.parse().unwrap(), ClaudeTui)]);

    let not_ready = test_probe::answers(&[false]);
    let held = route(IntakeRoutingMode::Disabled, O).await;
    assert!(
        blocked(&held).contains("gateway"),
        "held before the local fallback"
    );
    let local = route(IntakeRoutingMode::Disabled, LEGACY).await;
    let hook_disabled = RanLocalReason::HookDisabled;
    assert_eq!(
        local,
        IntakeRouterDecision::RanLocal {
            reason: hook_disabled
        }
    );
    drop(not_ready);

    let ready = test_probe::answer_with(|_| true);
    let refused = route(IntakeRoutingMode::Enforce, O).await;
    assert!(blocked(&refused).contains(RUNNER), "{refused:?}");
    assert_eq!(
        outbox_rows(&pool, O).await,
        0,
        "no row assigned off the gateway"
    );
    let legacy = route(IntakeRoutingMode::Enforce, LEGACY).await;
    assert!(forwarded_to_runner(&legacy), "{legacy:?}");
    drop(ready);

    let _unread = test_probe::answers(&[]);
    let _off = test_override::force_off();
    let off = route(IntakeRoutingMode::Enforce, O).await;
    assert!(forwarded_to_runner(&off), "writer off: {off:?}");

    pool.close().await;
    fixture.drop().await;
}
