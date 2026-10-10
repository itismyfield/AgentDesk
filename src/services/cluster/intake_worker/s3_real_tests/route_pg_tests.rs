//! T-H8: two nodes' real routers, outbox, claim, readiness and accept, with the reply posted by
//! the real actor; each claim fence is caught by its own effect.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::PgPool;

use super::harness::*;
use crate::db::intake_outbox::claim_pending_for_target;
use crate::db::o_channel_homes::{self, HomeWrite};
use crate::services::cluster::channel_home::{self, HomeGate, HomeOwnership};
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::cluster::intake_worker::test_executor::{self, Checkpoint};
use crate::services::cluster::intake_worker::{TickOutcome, run_intake_worker_tick};
use crate::services::discord::SharedData;
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};

const WITHIN: Duration = Duration::from_secs(60);

fn epoch_of(env: &Env) -> i64 {
    get(&env.dir, "epoch").unwrap().parse().unwrap()
}

/// Status, claim owner and whether it was accepted, of row `id`.
async fn stored(pool: &PgPool, id: i64) -> (String, Option<String>, bool) {
    sqlx::query_as(
        "SELECT status::TEXT, claim_owner, accepted_at IS NOT NULL FROM intake_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Every outbox row as JSON, for a failure message.
async fn rows(pool: &PgPool) -> String {
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT row_to_json(io)::TEXT FROM intake_outbox io")
            .fetch_all(pool)
            .await
            .unwrap();
    rows.join("\n")
}

/// The holder's executor stand-in: each turn it runs writes one assistant row to the channel's
/// transcript, which only the real actor turns into a POST.
fn replies(node: &Booted) -> (test_executor::Hook, test_executor::Recorder) {
    let transcript = node.transcript.clone();
    let turns = Arc::new(AtomicUsize::new(0));
    let hook = test_executor::hook(Box::new(move |seen| {
        let (transcript, turns) = (transcript.clone(), Arc::clone(&turns));
        Box::pin(async move {
            if seen == Checkpoint::FinalDb {
                let n = turns.fetch_add(1, Ordering::SeqCst) + 1;
                append(&transcript, &row(&format!("r{n}"), &format!("reply-{n}")));
            }
        })
    }));
    (hook, test_executor::record())
}

async fn owner() -> Arc<SharedData> {
    eprintln!("[s3] registering the owner runtime");
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (registry, shared) = owner.await;
    std::mem::forget(registry);
    shared
}

async fn tick(node: &Booted, shared: &Arc<SharedData>, target: &str) -> TickOutcome {
    let owner = format!("o-{target}");
    eprintln!("[s3] tick on {target}");
    run_intake_worker_tick(&node.pool, shared, target, "claude", &owner, &|| false)
        .await
        .expect("tick")
}

/// A booted holder whose lease opened `epoch` and whose writer takes work.
async fn open_holder(env: &Env) -> (Booted, i64) {
    let epoch = epoch_of(env);
    let node = Booted::boot(env).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    (node, epoch)
}

/// T-H8: both routers send the message to the one holder at its epoch, once; only the holder
/// claims and accepts it, its real readiness lets the turn run and the reply posts once.
#[tokio::test(flavor = "current_thread")]
async fn th8_two_routers_one_holder_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let mini = Node::spawn(&scene, "th8_holder", MINI);
    let gw = Node::spawn(&scene, "th8_gateway", GW);
    mini.finish(Duration::from_secs(180)).await;
    gw.finish(Duration::from_secs(60)).await;
    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT status::TEXT, target_instance_id, home_epoch FROM intake_outbox
         WHERE channel_id = $1",
    )
    .bind(C.to_string())
    .fetch_all(&scene.pool)
    .await
    .unwrap();
    assert_eq!(rows, [("done".to_string(), MINI.to_string(), Some(epoch))]);
    scene.drop_db().await;
}

pub(super) async fn holder_node(env: &Env) {
    let (node, epoch) = open_holder(env).await;
    let shared = owner().await;
    let (_hook, recorder) = replies(&node);
    signal(&env.dir, "holder_ready");
    wait_signal(&env.dir, "gw_routed", WITHIN).await;
    let again = route(&node.pool, MINI, "1001").await;
    assert!(
        matches!(again, IntakeRouterDecision::SkippedDuplicate { .. }),
        "{again:?}"
    );
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Processed);
    assert_eq!(recorder.channels(), [C], "{}", rows(&node.pool).await);
    node.until_posts(&["reply-1"]).await;
    let epochs = node.discord().started_epochs();
    assert_eq!(epochs, [("reply-1".to_string(), Some(epoch))]);
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::QueueEmpty);
    signal(&env.dir, "holder_done");
    node.stop().await;
}

pub(super) async fn gateway_node(env: &Env) {
    let node = Booted::boot(env).await;
    assert!(node.gates.is_empty(), "the row names the gateway nowhere");
    let shared = owner().await;
    wait_signal(&env.dir, "holder_ready", WITHIN).await;
    let routed = route(&node.pool, GW, "1001").await;
    assert!(
        matches!(&routed, IntakeRouterDecision::Forwarded { target_instance_id, .. }
            if target_instance_id == MINI),
        "{routed:?}"
    );
    signal(&env.dir, "gw_routed");
    assert_eq!(tick(&node, &shared, GW).await, TickOutcome::QueueEmpty);
    wait_signal(&env.dir, "holder_done", Duration::from_secs(120)).await;
    assert!(
        !node.accepts(),
        "the gateway's writer never takes the channel"
    );
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    node.stop().await;
}

/// T-H8: each claim SELECT branch (nothing held, an unrelated channel held) passes over an older
/// row routed at an epoch no home row has and claims the eligible row behind it in the same tick.
#[tokio::test(flavor = "current_thread")]
async fn th8_both_select_branches_make_progress_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    // One open row per channel: the stale rows sit on channels that have no home row.
    let stale = [
        seed_on(&scene.pool, STALE[0], "3001", Some(epoch)).await,
        seed_on(&scene.pool, STALE[1], "3002", Some(epoch)).await,
    ];
    seed(&scene.pool, "3003", Some(epoch)).await;
    let mini = Node::spawn(&scene, "th8_branches", MINI);
    mini.finish(Duration::from_secs(180)).await;
    for id in stale {
        let pending = ("pending".to_string(), None, false);
        assert_eq!(stored(&scene.pool, id).await, pending);
    }
    let done: Vec<(String, Option<String>, bool)> = sqlx::query_as(
        "SELECT status::TEXT, claim_owner, accepted_at IS NOT NULL FROM intake_outbox
         WHERE channel_id = $1 ORDER BY id",
    )
    .bind(C.to_string())
    .fetch_all(&scene.pool)
    .await
    .unwrap();
    let done_row = ("done".to_string(), Some("o-mini-s3".to_string()), true);
    assert_eq!(done, [done_row.clone(), done_row]);
    scene.drop_db().await;
}

const STALE: [u64; 2] = [4_380_911, 4_380_912];

pub(super) async fn branches_node(env: &Env) {
    let (node, epoch) = open_holder(env).await;
    let shared = owner().await;
    let (_hook, recorder) = replies(&node);
    assert!(intake_route::held_channels("claude").is_empty());
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Processed);
    // The gateway routes the next message once the first one's route closed.
    seed(&node.pool, "3004", Some(epoch)).await;
    // An unrelated delegated channel whose gate never opened is held here.
    let unrelated = Arc::new(HomeGate::new("4380999", MINI));
    channel_home::register(Arc::clone(&unrelated));
    assert_eq!(intake_route::held_channels("claude"), ["4380999"]);
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Processed);
    assert_eq!(recorder.channels(), [C, C], "{}", rows(&node.pool).await);
    node.until_posts(&["reply-1", "reply-2"]).await;
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::QueueEmpty);
    channel_home::unregister("4380999");
    node.stop().await;
}

/// T-H8: the gateway's reclaim lands after the worker's last check and before accept; the accept
/// fence refuses it, the row returns to pending and no turn or POST follows.
#[tokio::test(flavor = "current_thread")]
async fn th8_home_moves_before_accept_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let id = seed(&scene.pool, "4001", Some(epoch)).await;
    let mini = Node::spawn(&scene, "th8_moves", MINI);
    mini.finish(Duration::from_secs(120)).await;
    assert_eq!(
        stored(&scene.pool, id).await,
        ("pending".to_string(), None, false)
    );
    scene.drop_db().await;
}

pub(super) async fn moves_node(env: &Env) {
    let (node, epoch) = open_holder(env).await;
    let shared = owner().await;
    let pool = node.pool.clone();
    let _hook = test_executor::hook(Box::new(move |seen| {
        let pool = pool.clone();
        Box::pin(async move {
            if seen == Checkpoint::PreAccept {
                let channel = C.to_string();
                applied(o_channel_homes::begin_reclaim(&pool, &channel, epoch, GW).await);
            }
        })
    }));
    let recorder = test_executor::record();
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Held);
    assert_eq!(recorder.channels(), Vec::<u64>::new(), "no turn ran");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    node.stop().await;
}

/// T-H8: the gateway's reclaim commits after the claim picked the row and before it confirms;
/// the confirming UPDATE refuses it, so the row is never claimed and no turn or POST follows.
#[tokio::test(flavor = "current_thread")]
async fn th8_home_moves_between_select_and_confirm_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let id = seed(&scene.pool, "4101", Some(epoch)).await;
    let touched = |pool: &PgPool| {
        let query = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
            "SELECT updated_at FROM intake_outbox WHERE id = $1",
        );
        let pool = pool.clone();
        async move { query.bind(id).fetch_one(&pool).await.unwrap() }
    };
    let before = touched(&scene.pool).await;
    let mini = Node::spawn(&scene, "th8_select_moves", MINI);
    mini.finish(Duration::from_secs(120)).await;
    assert_eq!(
        stored(&scene.pool, id).await,
        ("pending".to_string(), None, false)
    );
    assert_eq!(
        touched(&scene.pool).await,
        before,
        "the row was never written"
    );
    scene.drop_db().await;
}

pub(super) async fn select_moves_node(env: &Env) {
    let (node, epoch) = open_holder(env).await;
    let shared = owner().await;
    let pool = node.pool.clone();
    let moved = Arc::new(AtomicUsize::new(0));
    let seen_move = Arc::clone(&moved);
    let _hook = test_executor::hook(Box::new(move |seen| {
        let (pool, moved) = (pool.clone(), Arc::clone(&seen_move));
        Box::pin(async move {
            if seen == Checkpoint::ClaimSelected && moved.fetch_add(1, Ordering::SeqCst) == 0 {
                let channel = C.to_string();
                applied(o_channel_homes::begin_reclaim(&pool, &channel, epoch, GW).await);
            }
        })
    }));
    let recorder = test_executor::record();
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::QueueEmpty);
    assert_eq!(
        moved.load(Ordering::SeqCst),
        1,
        "the claim selected the row once"
    );
    assert_eq!(recorder.channels(), Vec::<u64>::new(), "no turn ran");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    node.stop().await;
}

/// T-H8: a node whose own O store is committed but whose home gate is not open takes no claim,
/// readiness or POST; its lease's renewal alone opens it.
#[tokio::test(flavor = "current_thread")]
async fn th8_candidate_is_not_home_authority_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    put(scene.dir(), "phase", 1);
    Node::spawn(&scene, "th8_candidate", MINI)
        .finish(Duration::from_secs(120))
        .await;
    // The row lock keeps the restarted holder's renewal waiting, so its gate stays Lost.
    let mut lock = scene.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM o_channel_homes WHERE channel_id = $1 FOR UPDATE")
        .bind(C.to_string())
        .execute(&mut *lock)
        .await
        .unwrap();
    let id = seed(&scene.pool, "4001", Some(epoch)).await;
    put(scene.dir(), "phase", 2);
    let mini = Node::spawn(&scene, "th8_candidate", MINI);
    wait_signal(scene.dir(), "lost_checked", WITHIN).await;
    lock.rollback().await.unwrap();
    mini.finish(Duration::from_secs(120)).await;
    let done = ("done".to_string(), Some("o-mini-s3".to_string()), true);
    assert_eq!(stored(&scene.pool, id).await, done);
    scene.drop_db().await;
}

pub(super) async fn candidate_node(env: &Env) {
    if get(&env.dir, "phase").as_deref() == Some("1") {
        let (node, _) = open_holder(env).await;
        append(&node.transcript, &row("m0", "base"));
        node.until_posts(&["base"]).await;
        node.stop().await;
        return;
    }
    let epoch = epoch_of(env);
    let node = Booted::boot(env).await;
    let shared = owner().await;
    let gate = node.gate().expect("the holder's row registers its gate");
    let snapshot = crate::services::tui_o::cutover::test_override::with_channels(|boot| {
        boot.and_then(|boot| boot.standby(C)).map(|c| c.peek())
    });
    let committed = crate::services::tui_o::channel_policy::Adoption::Committed;
    assert_eq!(
        snapshot,
        Some(committed),
        "the boot read its committed store"
    );
    append(&node.transcript, &row("m1", "while-lost"));
    for _ in 0..4 {
        assert_eq!(gate.ownership(), HomeOwnership::Lost);
        assert!(!node.accepts());
        assert!(matches!(
            intake_route::route("claude", C),
            IntakeRoute::Hold(_)
        ));
        assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::QueueEmpty);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(node.discord().posts(), Vec::<String>::new());
    signal(&env.dir, "lost_checked");
    let (_hook, recorder) = replies(&node);
    until("the renewal opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Processed);
    assert_eq!(recorder.channels(), [C]);
    node.until_posts(&["while-lost", "reply-1"]).await;
    node.stop().await;
}

/// T-H8: without a home row a NULL-stamped row is claimed as before; with one it never is, and
/// a row routed at a home epoch is never claimed again once that home row is gone.
#[tokio::test(flavor = "current_thread")]
async fn th8_no_row_matches_legacy_pg() {
    let scene = Scene::new().await;
    let pool = &scene.pool;
    let clear = || async {
        sqlx::query("DELETE FROM intake_outbox")
            .execute(pool)
            .await
            .unwrap();
    };
    let claim = || async {
        let row = claim_pending_for_target(pool, MINI, "claude", "o")
            .await
            .unwrap();
        row.map(|row| row.id)
    };
    let plain = seed(pool, "5001", None).await;
    assert_eq!(claim().await, Some(plain), "no row: as before");
    clear().await;

    let epoch = scene.worker_home().await;
    seed(pool, "5002", None).await;
    assert_eq!(
        claim().await,
        None,
        "an unstamped row waits while a home row exists"
    );
    clear().await;

    let routed = seed(pool, "5003", Some(epoch)).await;
    assert_eq!(claim().await, Some(routed), "the holder at its epoch");
    sqlx::query(
        "UPDATE intake_outbox SET status = 'pending', claim_owner = NULL, claimed_at = NULL",
    )
    .execute(pool)
    .await
    .unwrap();
    let channel = C.to_string();
    applied(o_channel_homes::begin_reclaim(pool, &channel, epoch, GW).await);
    let left = applied(o_channel_homes::finish_reclaim(pool, &channel, MINI, epoch).await);
    assert_eq!(claim().await, None, "a reclaimed home claims nothing");
    let removed = o_channel_homes::remove_reclaimed(pool, &channel, GW, left.epoch).await;
    assert_eq!(removed.unwrap(), HomeWrite::Applied(()));
    assert_eq!(
        claim().await,
        None,
        "the epoch row stays dead without its home row"
    );
    assert_eq!(
        stored(pool, routed).await,
        ("pending".to_string(), None, false)
    );
    scene.drop_db().await;
}
