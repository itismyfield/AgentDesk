//! Two nodes over one database: a delegated row reaches accept only on the holder of its epoch.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{
    self, ChannelHome, HeldHome, HomeError, HomeState, HomeWrite, adopt, begin_reclaim, delegate,
    finish_reclaim, finish_release,
};
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::cluster::channel_home::{self as home_gate, HomeGate};
use crate::services::cluster::intake_router_hook::{
    IntakeRouterContext, IntakeRouterDecision, try_route_intake,
};
use crate::services::cluster::intake_routing_config::{
    IntakeRoutingMode, OwnerAuthorityChannelOptIn,
};
use crate::services::tui_o::channel_policy::Adoption;
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};

const C: u64 = 4_380_401;
const PLAIN: u64 = 4_380_402;
const GW: &str = "gw-4380";
const MINI: &str = "mini-4380";

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write.expect("home write") {
        HomeWrite::Applied(home) => home,
        HomeWrite::Stale => panic!("expected the home write to apply"),
    }
}

/// A worker-owned home of `C` held by MINI, released by the gateway first.
async fn delegated_to_mini(pool: &PgPool) -> i64 {
    let channel = C.to_string();
    let home = applied(delegate(pool, &channel, "claude", GW, MINI).await);
    let home = applied(finish_release(pool, &channel, GW, home.epoch).await);
    applied(adopt(pool, &channel, MINI, home.epoch).await).epoch
}

/// The gateway or the holder routing one message of `channel`.
async fn route(pool: &PgPool, leader: &str, channel: u64, message: &str) -> IntakeRouterDecision {
    let channel = channel.to_string();
    let ctx = IntakeRouterContext {
        mode: IntakeRoutingMode::Enforce,
        leader_instance_id: leader,
        provider: "claude",
        channel_id: &channel,
        policy_channel_id: &channel,
        user_msg_id: message,
        request_owner_id: "100",
        request_owner_name: Some("Tester"),
        user_text: "hello",
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground",
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        node_override_instance_id: None,
        owner_authority: OwnerAuthorityChannelOptIn::NotOptedIn,
        has_nonportable_uploads: false,
        attachment_refs: &[],
    };
    try_route_intake(pool, &ctx).await
}

/// Status and home epoch of the open row of `channel`.
async fn open_row(pool: &PgPool, channel: u64) -> (i64, String, Option<i64>) {
    sqlx::query_as(
        "SELECT id, status::TEXT, home_epoch FROM intake_outbox
         WHERE channel_id = $1 AND status IN ('pending', 'claimed')",
    )
    .bind(channel.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Refuses any move to `accepted`, so a tick that reaches accept fails before a turn starts.
async fn refuse_accepts(pool: &PgPool) {
    for statement in [
        "CREATE FUNCTION refuse_accept() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'accept attempted'; END $$ LANGUAGE plpgsql",
        "CREATE TRIGGER refuse_accept BEFORE UPDATE ON intake_outbox FOR EACH ROW
         WHEN (NEW.status = 'accepted') EXECUTE FUNCTION refuse_accept()",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

fn reached_accept(outcome: Result<TickOutcome, sqlx::Error>) -> bool {
    matches!(outcome, Err(error) if error.to_string().contains("accept attempted"))
}

async fn unclaim(pool: &PgPool, channel: u64) {
    let (id, ..) = open_row(pool, channel).await;
    assert!(return_claimed_to_pending(pool, id, "o").await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn a_delegated_row_reaches_accept_only_on_the_holder_of_its_epoch_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    refuse_accepts(&pool).await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, role, effective_role, labels, capabilities,
         last_heartbeat_at, started_at, updated_at)
         VALUES ($1, 'online', 'worker', 'worker', '[]', $2, NOW(), NOW(), NOW())",
    )
    .bind(MINI)
    .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": ["claude"]}}))
    .execute(&pool)
    .await
    .unwrap();
    for channel in [C, PLAIN] {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, 'Test', 'claude', $2)",
        )
        .bind(format!("agent-{channel}"))
        .bind(channel.to_string())
        .execute(&pool)
        .await
        .unwrap();
    }
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (_registry, shared) = owner.await;
    let not_cancelled = || false;
    let tick = || run_intake_worker_tick(&pool, &shared, MINI, "claude", "o", &not_cancelled);
    // The holder's boot: C is selected here off the O home and kept as standby.
    let _standby = test_override::force_standby(&[(C, ClaudeTui)], GW, Adoption::Committed);
    let _ready = test_probe::answer_with(|_| true);

    // A channel without a home row: the gateway routes it here and it reaches accept as before.
    let plain = route(&pool, GW, PLAIN, "1").await;
    assert!(
        matches!(plain, IntakeRouterDecision::RanLocal { .. }),
        "{plain:?}"
    );
    let payload = crate::db::intake_outbox::InsertPendingPayload {
        target_instance_id: MINI.into(),
        forwarded_by_instance_id: GW.into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: PLAIN.to_string(),
        user_msg_id: "1".into(),
        request_owner_id: "100".into(),
        request_owner_name: None,
        user_text: "hello".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: format!("agent-{PLAIN}"),
        provider: "claude".into(),
        home_epoch: None,
    };
    crate::db::intake_outbox::insert_pending(&pool, &payload, 1, None)
        .await
        .unwrap();
    assert!(reached_accept(tick().await), "a gateway-rule row");
    sqlx::query("DELETE FROM intake_outbox")
        .execute(&pool)
        .await
        .unwrap();

    // Both routers send the delegated channel to its holder, stamped with the home epoch.
    let epoch = delegated_to_mini(&pool).await;
    let forwarded = route(&pool, GW, C, "2").await;
    assert!(
        matches!(&forwarded, IntakeRouterDecision::Forwarded { target_instance_id, .. }
            if target_instance_id == MINI),
        "{forwarded:?}"
    );
    let again = route(&pool, MINI, C, "2").await;
    assert!(
        matches!(again, IntakeRouterDecision::SkippedDuplicate { .. }),
        "{again:?}"
    );
    let (row, status, stamped) = open_row(&pool, C).await;
    assert_eq!((status.as_str(), stamped), ("pending", Some(epoch)));

    // The holder's gate is still Lost: the row is held, never claimed.
    let gate = std::sync::Arc::new(HomeGate::new(&C.to_string(), MINI));
    home_gate::register(std::sync::Arc::clone(&gate));
    assert_eq!(tick().await.unwrap(), TickOutcome::QueueEmpty);
    assert_eq!(open_row(&pool, C).await.1, "pending");

    let renewal = |epoch| HeldHome::for_test(&C.to_string(), MINI, epoch, HomeState::Worker);
    let now = tokio::time::Instant::now;
    gate.confirm(&renewal(epoch), now())
        .expect("the holder opens");
    assert!(
        reached_accept(tick().await),
        "the holder at the row's epoch"
    );
    unclaim(&pool, C).await;

    // The gate moves to another epoch between claim and accept: the row goes back to pending.
    let moved = std::sync::Arc::clone(&gate);
    let mut reads = 0;
    let _moving = test_probe::answer_with(move |_| {
        reads += 1;
        if reads == 2 {
            moved.confirm(&renewal(epoch + 100), now()).unwrap();
        }
        true
    });
    assert_eq!(tick().await.unwrap(), TickOutcome::Held);
    assert_eq!(
        open_row(&pool, C).await,
        (row, "pending".into(), Some(epoch))
    );
    drop(_moving);

    // The database moves to a later lifecycle of the same holder: the old row is never claimed.
    let channel = C.to_string();
    let home = applied(begin_reclaim(&pool, &channel, epoch, GW).await);
    let home = applied(finish_reclaim(&pool, &channel, MINI, home.epoch).await);
    let removed = o_channel_homes::remove_reclaimed(&pool, &channel, GW, home.epoch).await;
    assert_eq!(removed.unwrap(), HomeWrite::Applied(()));
    let next = delegated_to_mini(&pool).await;
    let gate = std::sync::Arc::new(HomeGate::new(&channel, MINI));
    home_gate::register(std::sync::Arc::clone(&gate));
    gate.confirm(&renewal(next), now())
        .expect("the next epoch opens");
    assert_eq!(tick().await.unwrap(), TickOutcome::QueueEmpty);
    assert_eq!(
        open_row(&pool, C).await,
        (row, "pending".into(), Some(epoch))
    );

    home_gate::unregister(&channel);
    pool.close().await;
    fixture.drop().await;
}

#[cfg(unix)]
#[path = "s3_real_tests/mod.rs"]
mod s3_real;
