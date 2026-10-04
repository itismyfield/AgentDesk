//! A channel with a home row routes only by that row; without one it routes as before.
use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_session_owner,
    seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{
    HomeError, HomeWrite, adopt, begin_reclaim, delegate, finish_reclaim, finish_release,
    remove_reclaimed,
};
use crate::services::cluster::intake_routing_config::OwnerAuthorityChannelOptIn;
use serde_json::json;

const C: &str = "4380301";
const GW: &str = "leader-1";
const MINI: &str = "mini-4380";

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write.expect("home write") {
        HomeWrite::Applied(home) => home,
        HomeWrite::Stale => panic!("expected the home write to apply"),
    }
}

fn block(decision: IntakeRouterDecision) -> HomeBlock {
    match decision {
        IntakeRouterDecision::Blocked {
            reason: IntakeBlockedReason::ChannelHome { block },
        } => block,
        other => panic!("expected a home block, got {other:?}"),
    }
}

fn in_transition(state: &'static str) -> HomeBlock {
    HomeBlock::InTransition { state }
}

/// Target and home epoch of every intake row of `C`.
async fn rows(pool: &PgPool) -> Vec<(String, Option<i64>)> {
    sqlx::query_as("SELECT target_instance_id, home_epoch FROM intake_outbox WHERE channel_id = $1")
        .bind(C)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_channel_routes_by_its_home_row_and_as_before_without_one_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let worker = json!({"intake_worker": {"enabled": true, "providers": ["claude"],
        "features": ["preserve_on_cancel_v1"]}});
    seed_worker_node_with_capabilities(&pool, MINI, json!([]), "online", worker).await;
    seed_agent_with_preference(&pool, "agent-home", C, json!([])).await;
    let ctx = |mode| ctx_for_channel(mode, C);
    let route = async |ctx: IntakeRouterContext<'_>| try_route_intake(&pool, &ctx).await;
    let enforce = IntakeRoutingMode::Enforce;

    let before = route(ctx(enforce)).await;
    assert!(
        matches!(before, IntakeRouterDecision::RanLocal { .. }),
        "{before:?}"
    );

    // The hand-off states take no intake, whatever else would place the message.
    let home = applied(delegate(&pool, C, "claude", GW, MINI).await);
    assert_eq!(block(route(ctx(enforce)).await), in_transition("releasing"));
    let home = applied(finish_release(&pool, C, GW, home.epoch).await);
    assert_eq!(block(route(ctx(enforce)).await), in_transition("released"));

    // Worker-owned: only enforce routes it, and only to the holder with no rival authority.
    let home = applied(adopt(&pool, C, MINI, home.epoch).await);
    for mode in [IntakeRoutingMode::Disabled, IntakeRoutingMode::Observe] {
        let decision = route(ctx(mode)).await;
        assert!(
            matches!(&decision, IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed { detail }
            } if detail.contains("enforce")),
            "{mode:?}: {decision:?}"
        );
    }
    for authority in [
        OwnerAuthorityChannelOptIn::OptedIn,
        OwnerAuthorityChannelOptIn::Unknown,
    ] {
        let opted = IntakeRouterContext {
            owner_authority: authority,
            ..ctx(enforce)
        };
        assert_eq!(block(route(opted).await), HomeBlock::DualAuthority);
    }
    seed_session_owner(&pool, "claude-home", "claude", C, GW, "idle").await;
    let stale = HomeBlock::StaleSessionOwner {
        instance_ids: vec![GW.into()],
    };
    assert_eq!(block(route(ctx(enforce)).await), stale);
    sqlx::query("DELETE FROM sessions")
        .execute(&pool)
        .await
        .unwrap();
    assert!(rows(&pool).await.is_empty(), "no refused route left a row");

    let forwarded = route(ctx(enforce)).await;
    let IntakeRouterDecision::Forwarded {
        target_instance_id,
        basis,
        ..
    } = forwarded
    else {
        panic!("expected a forward, got {forwarded:?}");
    };
    assert_eq!(
        (target_instance_id.as_str(), basis),
        (MINI, IntakeRoutingBasis::DelegatedHome)
    );
    assert_eq!(rows(&pool).await, [(MINI.to_string(), Some(home.epoch))]);
    sqlx::query("DELETE FROM intake_outbox")
        .execute(&pool)
        .await
        .unwrap();

    let home = applied(begin_reclaim(&pool, C, home.epoch, GW).await);
    assert_eq!(
        block(route(ctx(enforce)).await),
        in_transition("reclaiming")
    );
    let home = applied(finish_reclaim(&pool, C, MINI, home.epoch).await);
    assert_eq!(block(route(ctx(enforce)).await), in_transition("reclaimed"));
    sqlx::query("ALTER TABLE o_channel_homes RENAME TO o_channel_homes_away")
        .execute(&pool)
        .await
        .unwrap();
    let unreadable = block(route(ctx(enforce)).await);
    assert!(
        matches!(unreadable, HomeBlock::Unreadable { .. }),
        "{unreadable:?}"
    );
    sqlx::query("ALTER TABLE o_channel_homes_away RENAME TO o_channel_homes")
        .execute(&pool)
        .await
        .unwrap();

    // Once the row is gone the channel routes exactly as it did before delegation.
    let removed = remove_reclaimed(&pool, C, GW, home.epoch).await.unwrap();
    assert_eq!(removed, HomeWrite::Applied(()));
    assert_eq!(route(ctx(enforce)).await, before);
    assert!(rows(&pool).await.is_empty());

    pool.close().await;
    fixture.drop().await;
}
