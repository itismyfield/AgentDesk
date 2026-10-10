//! T-H7: the holder's real O actor publishes what it owes, the real drain port reads it, the real
//! boot watch drains and the PG leave lands only after the tail went out; the gateway reclaims.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::harness::*;
use crate::db::o_channel_homes::{self, HomeState};
use crate::services::cluster::channel_home::{HomeIntake, HomeOwnership};
use crate::services::cluster::channel_home_drain::{DrainPort, Owed, ResetRefused};
use crate::services::cluster::channel_home_port::ChannelHomePort;
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
use crate::services::tui_o::writer::host;

const WITHIN: Duration = Duration::from_secs(60);
/// Two drain rounds of the boot watch (`RENEW_EVERY` apart) and a margin.
const TWO_ROUNDS: Duration = Duration::from_secs(12);

fn epoch_of(env: &Env) -> i64 {
    get(&env.dir, "epoch").unwrap().parse().unwrap()
}

/// The parent's side of every reclaiming case: a worker home on MINI, the node's scenario, and
/// the gateway's reclaim once the node says its responsibility shows in the projection.
async fn reclaim_while(scenario: &str, within: Duration) -> (Scene, i64) {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let node = Node::spawn(&scene, scenario, MINI);
    wait_signal(scene.dir(), "owed_seen", WITHIN).await;
    let channel = C.to_string();
    applied(o_channel_homes::begin_reclaim(&scene.pool, &channel, epoch, GW).await);
    // The gateway's router takes nothing new for the channel while it drains.
    let routed = route(&scene.pool, GW, "2001").await;
    assert!(
        matches!(routed, IntakeRouterDecision::Blocked { .. }),
        "{routed:?}"
    );
    signal(scene.dir(), "reclaim_begun");
    node.finish(within).await;
    let row = scene.row().await.unwrap();
    assert_eq!(
        (row.state, row.holder.as_deref()),
        (HomeState::Reclaimed, None)
    );
    assert!(row.epoch > epoch, "the leave wrote a fresh epoch");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&scene.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "no intake was taken while it drained");
    (scene, epoch)
}

/// A booted holder at `epoch` with its writer taking work and a baseline already delivered.
async fn holder_with_baseline(env: &Env) -> (Booted, i64) {
    let epoch = epoch_of(env);
    let node = Booted::boot(env).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    append(&node.transcript, &row("m0", "base"));
    node.until_posts(&["base"]).await;
    (node, epoch)
}

/// After the gateway's reclaim: intake closes at the same epoch, the router holds new intake and
/// the drain waits on `blocker` for two rounds without leaving.
async fn drains_without_leaving(env: &Env, node: &Booted, epoch: i64, blockers: &[&str]) {
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    until("intake closes at the same epoch", WITHIN, || {
        let gate = node.gate().expect("registered");
        matches!(gate.ownership(), HomeOwnership::Owned { home_epoch, intake: HomeIntake::Closed, .. }
            if home_epoch == epoch)
    })
    .await;
    assert!(matches!(
        intake_route::route("claude", C),
        IntakeRoute::Hold(_)
    ));
    let blocked = || {
        node.drain_blocker()
            .is_some_and(|seen| blockers.contains(&seen.as_str()))
    };
    until("the drain waits on the responsibility", WITHIN, blocked).await;
    tokio::time::sleep(TWO_ROUNDS).await;
    assert!(blocked(), "{:?}", node.drain_blocker());
    assert_eq!(node.state().await, Some(HomeState::Reclaiming));
}

async fn until_left(node: &Booted) {
    let left = || async { node.state().await == Some(HomeState::Reclaimed) };
    until_async("the drain leaves", Duration::from_secs(90), left).await;
}

fn tail_epochs(node: &Booted, prefix: &str) -> Vec<Option<i64>> {
    let started = node.discord().started_epochs().into_iter();
    let tail = started.filter(|(content, _)| content.starts_with(prefix));
    tail.map(|(_, epoch)| epoch).collect()
}

fn none_dropped(node: &Booted) {
    let dropped = node.discord().seen().into_iter();
    let dropped: Vec<_> = dropped
        .filter(|seen| matches!(seen, Seen::Dropped(_)))
        .collect();
    assert_eq!(dropped, [], "every request ran to its end");
}

/// T-H7: owed pieces at the reclaim go out at the holder's epoch, each once and the baseline never
/// again, and the row leaves only after all of them.
#[tokio::test(flavor = "current_thread")]
async fn th7_tail_posts_before_leave_pg() {
    let (scene, _) = reclaim_while("th7_tail", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn tail_node(env: &Env) {
    let (node, epoch) = holder_with_baseline(env).await;
    node.lease().withhold();
    for (id, text) in [("m1", "tail-1"), ("m2", "tail-2"), ("m3", "tail-3")] {
        append(&node.transcript, &row(id, text));
    }
    let three = Owed {
        owed: 3,
        ..Owed::default()
    };
    let seen = || async { node.owed().await == Some(three) };
    until_async("the projection shows three owed", WITHIN, seen).await;
    signal(&env.dir, "owed_seen");
    drains_without_leaving(env, &node, epoch, &["owed"]).await;
    assert_eq!(node.discord().posts(), ["base"], "the tail waited");
    node.lease().grant();
    until_left(&node).await;
    assert_eq!(
        node.discord().posts(),
        ["base", "tail-1", "tail-2", "tail-3"]
    );
    assert_eq!(tail_epochs(&node, "tail-"), [Some(epoch); 3]);
    assert_eq!(node.discord().running(), 0);
    none_dropped(&node);
    node.stop().await;
}

/// T-H7 per responsibility: a torn line alone blocks the leave until it completes and posts.
/// Only this source is built here; Prepared, unsealed and pending-bind are not.
#[tokio::test(flavor = "current_thread")]
async fn th7_each_responsibility_blocks_leave_pg() {
    let (scene, _) = reclaim_while("th7_torn", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn torn_node(env: &Env) {
    let (node, epoch) = holder_with_baseline(env).await;
    let torn = row("m1", "torn");
    let (head, tail) = torn.split_at(torn.len() / 2);
    append(&node.transcript, head);
    let uncaptured = Owed {
        uncaptured: 1,
        ..Owed::default()
    };
    let seen = || async { node.owed().await == Some(uncaptured) };
    until_async("the projection shows only the torn line", WITHIN, seen).await;
    signal(&env.dir, "owed_seen");
    drains_without_leaving(env, &node, epoch, &["owed"]).await;
    assert_eq!(node.discord().posts(), ["base"]);
    append(&node.transcript, tail);
    until_left(&node).await;
    assert_eq!(node.discord().posts(), ["base", "torn"]);
    assert_eq!(tail_epochs(&node, "torn"), [Some(epoch)]);
    none_dropped(&node);
    node.stop().await;
}

/// T-H7: the last POST still running keeps the row draining; it leaves only after that request
/// ended, and no POST starts after the final close.
#[tokio::test(flavor = "current_thread")]
async fn th7_last_post_blocks_cas_pg() {
    let (scene, _) = reclaim_while("th7_last_post", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn last_post_node(env: &Env) {
    let (node, epoch) = holder_with_baseline(env).await;
    node.discord().hold();
    append(&node.transcript, &row("m1", "last"));
    until("the last POST is running", WITHIN, || {
        node.discord().started("last")
    })
    .await;
    signal(&env.dir, "owed_seen");
    // A running POST keeps its Prepared open, and the actor waiting on it publishes nothing new.
    drains_without_leaving(env, &node, epoch, &["owed", "owed_unreadable"]).await;
    assert_eq!(node.discord().running(), 1);
    node.discord().release();
    until_left(&node).await;
    let seen = node.discord().seen();
    let ended = seen.iter().any(|seen| *seen == Seen::Ended("last".into()));
    assert!(ended, "{seen:?}");
    assert_eq!(node.discord().posts(), ["base", "last"]);
    assert_eq!(tail_epochs(&node, "last"), [Some(epoch)]);
    node.stop().await;
}

/// Reads through the real port, adding one transcript row the first time the drain counts running
/// POSTs, which it does only after the final close and before its last check.
struct LateTail {
    reads: ChannelHomePort,
    transcript: PathBuf,
    fired: AtomicBool,
}

impl DrainPort for LateTail {
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        self.reads.turn_running()
    }

    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send {
        self.reads.owed()
    }

    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send {
        if !self.fired.swap(true, Ordering::SeqCst) {
            append(&self.transcript, &row("m9", "late"));
        }
        self.reads.posts_in_flight()
    }

    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send {
        self.reads.reset_legacy_source()
    }
}

use std::future::Future;

/// T-H7: a piece owed after the final close is found by a fresh read, the drain reopens the same
/// epoch through a renewal to post it, and only then leaves.
#[tokio::test(flavor = "current_thread")]
async fn th7_post_close_recheck_finds_tail_pg() {
    let (scene, _) = reclaim_while("th7_recheck", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn recheck_node(env: &Env) {
    let epoch = epoch_of(env);
    let transcript = crate::config::runtime_root().unwrap().join("c.jsonl");
    let node = Booted::boot_with(env, move |id| LateTail {
        reads: ChannelHomePort::new(id, host::process_readiness(), worker_restored()),
        transcript: transcript.clone(),
        fired: AtomicBool::new(false),
    })
    .await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    append(&node.transcript, &row("m0", "base"));
    node.until_posts(&["base"]).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("nothing owed before the reclaim", WITHIN, clear).await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    until_left(&node).await;
    assert_eq!(node.discord().posts(), ["base", "late"]);
    assert_eq!(tail_epochs(&node, "late"), [Some(epoch)]);
    none_dropped(&node);
    node.stop().await;
}

/// T-H7: a halted actor's unknown projection is not zero: the row stays draining until the resumed
/// actor's real projection clears, and only then leaves.
#[tokio::test(flavor = "current_thread")]
async fn th7_unknown_is_not_empty_pg() {
    let (scene, _) = reclaim_while("th7_unknown", Duration::from_secs(240)).await;
    scene.drop_db().await;
}

pub(super) async fn unknown_node(env: &Env) {
    use crate::services::tui_o::store::fault::{self, Keep, Step as At};
    let (node, epoch) = holder_with_baseline(env).await;
    let spool = node.root.join("o_store").join(C.to_string()).join("spool");
    let full = fault::plant(
        &spool,
        At::Append(Keep::Nothing),
        std::io::ErrorKind::StorageFull,
        None,
    );
    append(&node.transcript, &row("m1", "after-halt"));
    until("the writer halts", WITHIN, || node.halted()).await;
    let unknown = || async { node.owed().await.is_none() };
    until_async("the projection is unknown", WITHIN, unknown).await;
    signal(&env.dir, "owed_seen");
    drains_without_leaving(env, &node, epoch, &["owed_unreadable"]).await;
    drop(full);
    until_left(&node).await;
    assert_eq!(node.discord().posts(), ["base", "after-halt"]);
    node.stop().await;
}

/// T-H7 releasing direction: the gateway holder posts its tail and leaves as `released`. Its drain
/// needs the Legacy release reset, which the drain port does not reach yet.
#[tokio::test(flavor = "current_thread")]
#[ignore = "BLOCKED: s3act D — releasing drain needs the gateway Legacy release reset; ChannelHomePort::reset_legacy_source is NotWired"]
async fn th7_releasing_tail_posts_before_release_pg() {
    let scene = Scene::new().await;
    let channel = C.to_string();
    let home = applied(o_channel_homes::delegate(&scene.pool, &channel, "claude", GW, MINI).await);
    put(scene.dir(), "epoch", home.epoch);
    let node = Node::spawn(&scene, "th7_release", GW);
    node.finish(Duration::from_secs(120)).await;
    let row = scene.row().await.unwrap();
    assert_eq!(
        (row.state, row.holder.as_deref()),
        (HomeState::Released, None)
    );
    scene.drop_db().await;
}

pub(super) async fn release_node(env: &Env) {
    let epoch = epoch_of(env);
    let node = Booted::boot(env).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    append(&node.transcript, &row("m1", "tail-1"));
    let released = || async { node.state().await == Some(HomeState::Released) };
    until_async("the release lands", Duration::from_secs(90), released).await;
    assert_eq!(node.discord().posts(), ["tail-1"]);
    node.stop().await;
}
