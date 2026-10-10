//! T-H9: the operator's CLI force on a holder that died, and what each node's real boot, watch,
//! router, claim, readiness and writer do with the orphaned row afterwards.

use std::time::Duration;

use super::harness::*;
use crate::cli::channel_home::{ChannelHomeCommand, execute};
use crate::db::o_channel_homes::{self, HomeState, HomeWrite};
use crate::services::cluster::channel_home::HomeOwnership;
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::cluster::intake_worker::{TickOutcome, run_intake_worker_tick};
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};

const WITHIN: Duration = Duration::from_secs(60);
/// Two periods of each node's watch and lease, and a margin.
const TWO_ROUNDS: Duration = Duration::from_secs(12);

async fn force(scene: &Scene) -> Result<String, String> {
    let mut config = crate::config::Config::default();
    config.runtime.channel_home_delegation_enabled = Some(true);
    config.cluster.instance_id = Some(GW.into());
    let url = scene.url().to_string();
    let connect = || async move {
        crate::db::postgres::connect_test_pool_with_max_connections(&url, "s3 cli", 2).await
    };
    execute(&config, ChannelHomeCommand::Force { channel: C }, connect).await
}

/// T-H9: force refuses a live holder; once the holder is gone and silent past F it orphans the
/// row, and from then no node's boot, renewal, router, claim, readiness or writer acts on it.
#[tokio::test(flavor = "current_thread")]
async fn th9_force_orphan_holds_every_consumer_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    let channel = C.to_string();
    applied(o_channel_homes::begin_reclaim(&scene.pool, &channel, epoch, GW).await);
    put(scene.dir(), "epoch", epoch);
    let holder = Node::spawn(&scene, "th9_holder", MINI);
    wait_signal(scene.dir(), "held", WITHIN).await;
    let fresh = force(&scene).await;
    assert!(
        fresh.is_err_and(|e| e.contains("within F")),
        "a live holder"
    );
    holder.kill();
    // Fast boundary only: the lease is aged in SQL, not waited out in wall-clock time.
    sqlx::query("UPDATE o_channel_homes SET renewed_at = NOW() - INTERVAL '201 seconds'")
        .execute(&scene.pool)
        .await
        .unwrap();
    let forced: serde_json::Value = serde_json::from_str(&force(&scene).await.unwrap()).unwrap();
    assert_eq!(forced["state"], "orphaned");
    let orphan = scene.row().await.unwrap();
    assert_eq!(
        (
            orphan.state,
            orphan.holder.as_deref(),
            orphan.target.as_deref()
        ),
        (HomeState::Orphaned, None, Some(GW))
    );
    assert!(orphan.epoch > epoch);
    put(scene.dir(), "orphan_epoch", orphan.epoch);
    let stranded = seed(&scene.pool, "6001", Some(epoch)).await;

    let gw = Node::spawn(&scene, "th9_consumer", GW);
    let mini = Node::spawn(&scene, "th9_consumer", MINI);
    gw.finish(Duration::from_secs(120)).await;
    mini.finish(Duration::from_secs(120)).await;
    let after = scene.row().await.unwrap();
    assert_eq!(
        (after.state, after.holder, after.epoch),
        (HomeState::Orphaned, None, orphan.epoch),
        "nothing recovered the row on its own"
    );
    let (status, owner): (String, Option<String>) =
        sqlx::query_as("SELECT status::TEXT, claim_owner FROM intake_outbox WHERE id = $1")
            .bind(stranded)
            .fetch_one(&scene.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), owner), ("pending", None));
    scene.drop_db().await;
}

pub(super) async fn holder_node(env: &Env) {
    let epoch: i64 = get(&env.dir, "epoch").unwrap().parse().unwrap();
    let node = Booted::boot(env).await;
    until("the lease opens the draining home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    append(&node.transcript, &row("m0", "before"));
    node.until_posts(&["before"]).await;
    signal(&env.dir, "held");
    // Killed by the parent from here; it never ends on its own.
    std::future::pending::<()>().await;
}

pub(super) async fn consumer_node(env: &Env) {
    let epoch: i64 = get(&env.dir, "epoch").unwrap().parse().unwrap();
    let orphan: i64 = get(&env.dir, "orphan_epoch").unwrap().parse().unwrap();
    let node = Booted::boot(env).await;
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (registry, shared) = owner.await;
    let local = env.role.as_str();
    append(&node.transcript, &row("m1", "after-force"));
    tokio::time::sleep(TWO_ROUNDS).await;
    match node.gate() {
        // The gateway is still the row's target: its gate registers and stays Lost.
        Some(gate) => {
            assert_eq!(local, GW);
            assert_eq!(gate.ownership(), HomeOwnership::Lost);
        }
        None => assert_eq!(local, MINI, "the old holder is named nowhere"),
    }
    assert!(!node.accepts());
    // The target holds the channel by its gate; the old holder, named nowhere, never takes it.
    let routed_here = intake_route::route("claude", C);
    match local {
        GW => assert!(
            matches!(routed_here, IntakeRoute::Hold(_)),
            "{routed_here:?}"
        ),
        _ => assert_ne!(routed_here, IntakeRoute::Gateway),
    }
    let message = if local == GW { "6002" } else { "6003" };
    let routed = route(&node.pool, local, message).await;
    assert!(
        matches!(routed, IntakeRouterDecision::Blocked { .. }),
        "{routed:?}"
    );
    let owner = format!("o-{local}");
    let tick = run_intake_worker_tick(&node.pool, &shared, local, "claude", &owner, &|| false);
    assert_eq!(tick.await.unwrap(), TickOutcome::QueueEmpty);
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    let channel = C.to_string();
    for at in [epoch, orphan] {
        let renewed = o_channel_homes::renew(&node.pool, &channel, local, at).await;
        assert!(matches!(renewed, Ok(HomeWrite::Stale)), "renewal at {at}");
        let adopted = o_channel_homes::adopt(&node.pool, &channel, local, at).await;
        assert!(matches!(adopted, Ok(HomeWrite::Stale)), "adopt at {at}");
    }
    drop(registry);
    node.stop().await;
}
