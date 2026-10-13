//! T-H7: the holder's real O actor publishes what it owes, the real drain port reads it, the real
//! boot watch drains and the PG leave lands only after the tail went out; the gateway reclaims.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::harness::*;
use crate::db::o_channel_activation;
use crate::db::o_channel_homes::{self, HomeState};
use crate::services::cluster::channel_home::{HomeIntake, HomeOwnership};
use crate::services::cluster::channel_home_drain::{DrainPort, Owed, ResetRefused};
use crate::services::cluster::channel_home_port::ChannelHomePort;
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::store::rotation::{BOUNDARY_FILE, Boundary, Rotation};
use crate::services::tui_o::store::{
    ChannelStore, InitSource, Initialized, OStore, STORE_DIR_NAME, StoreConfig,
};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::confirm::{FIRST_LOOK, LAST_LOOK};
use crate::services::tui_o::writer::deliver::{self, ChannelWriter, Step};
use crate::services::tui_o::writer::host::test_io::Alarms;
use crate::services::tui_o::writer::host::{self, HostIo};
use crate::services::tui_o::writer::pieces::{Derived, PieceWork};
use crate::services::tui_o::writer::{DiscordPort, PostOutcome, WriterAlarm};

const WITHIN: Duration = Duration::from_secs(60);
/// Two drain rounds of the boot watch (`RENEW_EVERY` apart) and a margin.
const TWO_ROUNDS: Duration = Duration::from_secs(12);

fn epoch_of(env: &Env) -> i64 {
    get(&env.dir, "epoch").unwrap().parse().unwrap()
}

/// The parent's side of every reclaiming case: a worker home on MINI, the node's scenario, and
/// the gateway's reclaim once the node says its responsibility is in place.
async fn reclaim_while(scenario: &str, within: Duration) -> (Scene, i64) {
    reclaim_after(Scene::new().await, None, scenario, within).await
}

/// As [`reclaim_while`] on `scene`, after `seed` ran to its end as an earlier generation on
/// MINI's root.
async fn reclaim_after(
    scene: Scene,
    seed: Option<&str>,
    scenario: &str,
    within: Duration,
) -> (Scene, i64) {
    let (node, epoch) = reclaim_begun(&scene, seed, scenario).await;
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

/// The parent's side up to the gateway's reclaim, with the node that holds the channel still running.
async fn reclaim_begun(scene: &Scene, seed: Option<&str>, scenario: &str) -> (Node, i64) {
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    if let Some(seed) = seed {
        Node::spawn(scene, seed, MINI).finish(WITHIN).await;
    }
    let mut node = Node::spawn(scene, scenario, MINI);
    node.wait_signal("owed_seen", WITHIN).await;
    let channel = C.to_string();
    applied(o_channel_homes::begin_reclaim(&scene.pool, &channel, epoch, GW).await);
    // The gateway's router takes nothing new for the channel while it drains.
    let routed = route_as(&scene.pool, GW, scene.provider, "2001").await;
    assert!(
        matches!(routed, IntakeRouterDecision::Blocked { .. }),
        "{routed:?}"
    );
    signal(scene.dir(), "reclaim_begun");
    (node, epoch)
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

/// T-H7 POST guard alone: a counted POST still running keeps the row draining while the actor's
/// fresh projection owes nothing, and the row leaves only after that request ended.
#[tokio::test(flavor = "current_thread")]
async fn th7_running_post_blocks_leave_with_projection_zero_pg() {
    let (scene, _) = reclaim_while("th7_running_post", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

/// A second real writer of `C` on its own store root, posting through the home's gate.
fn second_writer(
    root: &Path,
    node: &Booted,
    discord: Arc<Discord>,
) -> (OStore, ChannelWriter<Discord, Lease, Alarms>) {
    std::fs::create_dir_all(root).unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root);
    let store = store.unwrap().expect("store enabled");
    let at = chrono::Utc::now();
    let init = |channel| {
        let build_digest = "s3".to_string();
        let (sources, initial_anchor) = (Vec::new(), 100);
        Ok(Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    let era = store.begin_era(&[C], at, init).unwrap();
    let channel = store.open_channel(&era, C).unwrap().expect("era channel");
    let gate = node.gate().expect("registered").gate();
    let alarms = node.host.alarms();
    let writer = ChannelWriter::new(channel, gate, discord, Lease::default(), alarms);
    (store, writer)
}

// Independent guard evidence: a second writer of the holder's channel posts; not single-actor E2E.
pub(super) async fn running_post_node(env: &Env) {
    let (node, epoch) = holder_with_baseline(env).await;
    let gate = node.gate().expect("registered");
    let HomeOwnership::Owned {
        home_epoch,
        gate_epoch,
        ..
    } = gate.ownership()
    else {
        panic!("the lease holds the home");
    };
    assert_eq!(home_epoch, epoch);
    let discord = Arc::new(Discord::default());
    discord.hold();
    let root = env.dir.join("second-writer");
    let (_store, mut writer) = second_writer(&root, &node, Arc::clone(&discord));
    let unit = UnitKey {
        channel_id: C,
        provider: ShadowProvider::Claude,
        native_key: "second".into(),
        kind: UnitKind::Body,
    };
    let piece = Derived::Piece(PieceWork {
        unit_key: unit.clone(),
        index: 0,
        payload: "held".into(),
    });
    let delivering = tokio::spawn(async move { (writer.deliver(&piece).await, writer) });
    let counted = || deliver::posts_in_flight(C) == Some(1);
    until("the second writer's POST counts", WITHIN, counted).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the actor's fresh projection owes nothing", WITHIN, clear).await;
    assert_eq!(deliver::posts_in_flight(C), Some(1), "still running");
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    until("the drain's final close", WITHIN, || {
        gate.final_closed(epoch)
    })
    .await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let (blocker, running) = (node.drain_blocker(), deliver::posts_in_flight(C));
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "the row left while the POST ran: blocker {blocker:?}, in flight {running:?}"
    );
    assert_eq!(blocker.as_deref(), Some("posts_in_flight"));
    assert_eq!(running, Some(1));
    assert_eq!(discord.posts(), Vec::<String>::new());
    discord.release();
    let (step, mut writer) = delivering.await.unwrap();
    assert_eq!(step, Step::Done);
    until_left(&node).await;
    assert_eq!(discord.posts(), ["held"]);
    let (_, record) = writer.store().ledger().latest_piece(&unit, 0).unwrap();
    assert_eq!(record.epoch, gate_epoch, "admitted at the held gate epoch");
    assert!(matches!(record.outcome, Some(PieceOutcome::Posted(_))));
    assert_eq!(
        node.discord().posts(),
        ["base"],
        "the actor posted nothing more"
    );
    node.stop().await;
}

/// The payload of the piece whose POST Discord accepted while its reply was lost.
const MAYBE_SENT: &str = "maybe sent";
const HISTORY: &str = "discord-history.jsonl";

/// The unit `row("m1", MAYBE_SENT)` derives on `C`.
fn maybe_sent_unit() -> UnitKey {
    UnitKey {
        channel_id: C,
        provider: ShadowProvider::Claude,
        native_key: "m1:0".into(),
        kind: UnitKind::Body,
    }
}

/// The store a first activation writes over the still-empty transcript `s1`, as the host makes it.
fn activated_store(root: &Path) -> (OStore, ChannelStore) {
    let transcript = root.join("c.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let source_id = source_id_for("s1", &transcript).unwrap();
    let prefix_hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b""));
    let sources = vec![InitSource {
        source_id,
        delivery_start: 0,
        prefix_hash,
    }];
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root);
    let store = store.unwrap().expect("store enabled");
    let at = chrono::Utc::now();
    let init = |channel| {
        let build_digest = env!("CARGO_PKG_VERSION").into();
        let (sources, initial_anchor) = (sources.clone(), 0);
        Ok(Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    let era = store.begin_era(&[C], at, init).unwrap();
    let channel = store.open_channel(&era, C).unwrap().expect("era channel");
    (store, channel)
}

/// T-H7 Prepared alone: an open Prepared left by a crash keeps the reclaim draining; the replayed
/// source settles it as Posted from the history Discord kept, with no new POST, and only then leaves.
#[tokio::test(flavor = "current_thread")]
async fn th7_prepared_alone_blocks_reclaim_pg() {
    let (scene, seed) = (Scene::new().await, Some("th7_prepared_seed"));
    let (scene, _) = reclaim_after(scene, seed, "th7_prepared", Duration::from_secs(180)).await;
    let history = std::fs::read_to_string(scene.dir().join(HISTORY)).unwrap();
    let lines: Vec<&str> = history.lines().collect();
    assert_eq!(lines.len(), 1, "Discord accepted no POST after the restart");
    let accepted: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let root = scene.dir().join(format!("root-{MINI}"));
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel = store.open_channel(&era, C).unwrap().unwrap();
    assert_eq!(
        channel.ledger().next_serial(),
        1,
        "no Prepared after the restart"
    );
    let (_, piece) = channel
        .ledger()
        .latest_piece(&maybe_sent_unit(), 0)
        .unwrap();
    let posted = PieceOutcome::Posted(accepted["id"].as_u64().unwrap());
    assert_eq!(piece.outcome, Some(posted));
    scene.drop_db().await;
}

/// The activated store with the Prepared a writer appends before its POST, at serial 0.
fn prepared_store() -> (OStore, ChannelStore) {
    let root = crate::config::runtime_root().expect("node root");
    let (store, mut channel) = activated_store(&root);
    let ledger = channel.ledger();
    let prepared = LedgerEntry::Prepared {
        serial: ledger.next_serial(),
        unit_key: maybe_sent_unit(),
        piece_index: 0,
        payload: MAYBE_SENT.into(),
        anchor_id: ledger.anchor(),
        epoch: 1,
    };
    channel.append_ledger(prepared).unwrap();
    (store, channel)
}

/// First generation: the Prepared and Discord accepting its POST; the reply is lost as this
/// generation ends.
pub(super) async fn prepared_seed_node(env: &Env) {
    let (_store, channel) = prepared_store();
    let discord = Discord::kept_in(&env.dir.join(HISTORY));
    let outcome = discord.post(C, MAYBE_SENT.into()).await;
    assert!(matches!(outcome, PostOutcome::Created(_)), "{outcome:?}");
    let (serial, open) = channel.ledger().unresolved().expect("the Prepared is open");
    assert_eq!((serial, open.payload.as_str()), (0, MAYBE_SENT));
}

pub(super) async fn prepared_node(env: &Env) {
    let discord = Discord::kept_in(&env.dir.join(HISTORY));
    let kept = discord.kept();
    assert_eq!(kept.len(), 1, "the history Discord kept across the restart");
    let ours = (discord.bot_id(), MAYBE_SENT);
    assert_eq!((kept[0].author_id, kept[0].content.as_str()), ours);
    let node = holds_on_prepared(env, discord).await;
    append(&node.transcript, &row("m1", MAYBE_SENT));
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the replayed piece settles the Prepared", WITHIN, clear).await;
    until_left(&node).await;
    assert_eq!(
        node.discord().posts(),
        Vec::<String>::new(),
        "no POST after the restart"
    );
    assert_eq!(node.discord().seen(), Vec::<Seen>::new());
    node.stop().await;
}

/// The restarted holder on the seed's root: through two drain rounds after the reclaim, its open
/// Prepared alone keeps the row Reclaiming, before any row derives its unit again.
async fn holds_on_prepared(env: &Env, discord: Discord) -> Booted {
    let epoch = epoch_of(env);
    let node = Booted::boot_on(env, discord).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    // The seed proved the Prepared; a projection wait here would let a mutant time out instead.
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    let gate = node.gate().expect("registered");
    let ran = || node.drain_blocker().is_some() || gate.final_closed(epoch);
    until("the drain runs a round", WITHIN, ran).await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let (blocker, running) = (node.drain_blocker(), deliver::posts_in_flight(C));
    let owed = node.owed().await;
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "the row left while the Prepared stayed open: blocker {blocker:?}, projection {owed:?}, \
         in flight {running:?}"
    );
    let alone = Owed {
        prepared: 1,
        ..Owed::default()
    };
    assert_eq!(owed, Some(alone), "the Prepared is the only responsibility");
    assert_eq!(blocker.as_deref(), Some("owed"));
    assert_eq!(running, Some(0));
    node
}

/// An unsent Prepared alone: Discord's history keeps no candidate for it.
pub(super) async fn unsent_seed_node(_: &Env) {
    let (_store, channel) = prepared_store();
    let (serial, open) = channel.ledger().unresolved().expect("the Prepared is open");
    assert_eq!((serial, open.payload.as_str()), (0, MAYBE_SENT));
}

/// T-H7 Prepared alone, NotFound: an unsent Prepared keeps the reclaim draining until the replayed
/// row settles it NotFound from a readable history with no candidate; nothing is posted.
#[tokio::test(flavor = "current_thread")]
async fn th7_prepared_alone_settles_not_found_pg() {
    unsent_settles("th7_settles_not_found")
        .await
        .drop_db()
        .await;
}

/// T-H7 Prepared alone, Unresolved: as the NotFound case with every history read failing; the
/// Prepared settles Unresolved for that reason and nothing is posted or retried.
#[tokio::test(flavor = "current_thread")]
async fn th7_prepared_alone_settles_unresolved_pg() {
    unsent_settles("th7_settles_unresolved")
        .await
        .drop_db()
        .await;
}

/// The parent's side of an unsent Prepared: the row leaves, no generation posted and the ledger
/// keeps the one Prepared, settled.
async fn unsent_settles(scenario: &str) -> Scene {
    let (scene, seed) = (Scene::new().await, Some("th7_unsent_seed"));
    let (scene, _) = reclaim_after(scene, seed, scenario, Duration::from_secs(180)).await;
    let history = std::fs::read_to_string(scene.dir().join(HISTORY)).unwrap();
    assert_eq!(history, "", "Discord accepted no POST in any generation");
    let root = scene.dir().join(format!("root-{MINI}"));
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel = store.open_channel(&era, C).unwrap().unwrap();
    assert_eq!(
        channel.ledger().next_serial(),
        1,
        "no Prepared after the seed"
    );
    assert!(channel.ledger().unresolved().is_none());
    let (_, piece) = channel
        .ledger()
        .latest_piece(&maybe_sent_unit(), 0)
        .unwrap();
    let took = get(scene.dir(), "settled_after_ms").unwrap();
    eprintln!(
        "[s3] serial 0 settled {:?} {took} ms after its row came again",
        piece.outcome
    );
    scene
}

pub(super) async fn settles_not_found_node(env: &Env) {
    let discord = Discord::kept_in(&env.dir.join(HISTORY));
    let (node, replayed) = settled_on_replay(env, discord).await;
    let reads = node.discord().history_reads();
    let after = reads.first().map(|r| r.after);
    let answers: Vec<_> = (reads.iter())
        .map(|r| (r.channel, Some(r.after), r.answer.clone()))
        .collect();
    assert_eq!(
        answers,
        [(C, after, Ok(0)), (C, after, Ok(0))],
        "both looks, one anchor"
    );
    let first = reads[0].at.duration_since(replayed);
    let last = reads[1].at.duration_since(reads[0].at);
    assert!(first >= FIRST_LOOK, "first read {first:?} after the row");
    assert!(
        last >= LAST_LOOK - FIRST_LOOK,
        "second read {last:?} after the first"
    );
    assert_eq!(settled(&node), [LedgerEntry::NotFound { serial: 0 }]);
    assert_eq!(settle_alarms(&node), [WriterAlarm::NotFound { serial: 0 }]);
    node.stop().await;
}

pub(super) async fn settles_unresolved_node(env: &Env) {
    let discord = Discord::kept_in(&env.dir.join(HISTORY)).history_unavailable();
    let (node, replayed) = settled_on_replay(env, discord).await;
    let reads = node.discord().history_reads();
    let answers: Vec<_> = reads
        .iter()
        .map(|r| (r.channel, r.answer.clone()))
        .collect();
    let failed = (C, Err(HISTORY_UNAVAILABLE.to_string()));
    assert_eq!(answers, [failed], "read again after an error");
    let first = reads[0].at.duration_since(replayed);
    assert!(first >= FIRST_LOOK, "first read {first:?} after the row");
    let outcomes = settled(&node);
    let [LedgerEntry::Unresolved { serial: 0, reason }] = &outcomes[..] else {
        panic!("settled {outcomes:?}");
    };
    assert!(reason.contains(HISTORY_UNAVAILABLE), "{reason}");
    let alarm = WriterAlarm::Unresolved {
        serial: 0,
        reason: reason.clone(),
    };
    assert_eq!(settle_alarms(&node), [alarm]);
    node.stop().await;
}

/// Holds on the unsent Prepared, then appends the row that derives its unit again, which is what
/// starts the settlement; waits for it and the leave, with nothing posted. Returns when the row
/// was appended, on the clock history reads are stamped with.
async fn settled_on_replay(env: &Env, discord: Discord) -> (Booted, std::time::Instant) {
    let node = holds_on_prepared(env, discord).await;
    assert_eq!(settled(&node), [], "settled before the row came again");
    assert_eq!(settle_alarms(&node), []);
    let reads = node.discord().history_reads();
    assert!(
        reads.is_empty(),
        "history read before the row came again: {reads:?}"
    );
    let replayed = std::time::Instant::now();
    append(&node.transcript, &row("m1", MAYBE_SENT));
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the replayed row settles the Prepared", WITHIN, clear).await;
    let took = replayed.elapsed();
    eprintln!("[s3] settled {took:?} after the row came again");
    put(&env.dir, "settled_after_ms", took.as_millis());
    until_left(&node).await;
    assert_eq!(node.discord().seen(), Vec::<Seen>::new(), "no POST started");
    assert!(!node.halted(), "{:?}", node.raised());
    (node, replayed)
}

/// The ledger's entries other than Prepared, read from its file while the actor holds the store.
fn settled(node: &Booted) -> Vec<LedgerEntry> {
    let path = node.root.join(STORE_DIR_NAME).join(C.to_string());
    let ledger = std::fs::read_to_string(path.join("ledger.jsonl")).unwrap();
    let entries = ledger.lines().map(|line| {
        let line: serde_json::Value = serde_json::from_str(line).unwrap();
        serde_json::from_value::<LedgerEntry>(line["entry"].clone()).unwrap()
    });
    let settled = entries.filter(|entry| !matches!(entry, LedgerEntry::Prepared { .. }));
    settled.collect()
}

/// The raw alarms that settle a Prepared or halt the writer.
fn settle_alarms(node: &Booted) -> Vec<WriterAlarm> {
    let raised = node.raised().into_iter();
    let settling = |alarm: &WriterAlarm| {
        matches!(
            alarm,
            WriterAlarm::NotFound { .. }
                | WriterAlarm::Unresolved { .. }
                | WriterAlarm::Ambiguous { .. }
                | WriterAlarm::Halted { .. }
        )
    };
    raised.filter(settling).collect()
}

/// The Codex rollout line announcing `msg_c1` before its text is written, and the one sealing it.
const ANNOUNCED: &str = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"msg_c1"}}}"#;
const SEALED: &str = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","id":"msg_c1","content":[{"type":"output_text","text":"hello"}]}}"#;

/// T-H7 Codex unsealed alone: an announced, unsealed Codex message keeps the reclaim draining;
/// once it seals, the real actor posts it once and only then the row leaves.
#[tokio::test(flavor = "current_thread")]
async fn th7_codex_unsealed_alone_blocks_reclaim_pg() {
    let scene = Scene::for_provider(ShadowProvider::Codex).await;
    let (scene, _) = reclaim_after(scene, None, "th7_unsealed", Duration::from_secs(180)).await;
    let root = scene.dir().join(format!("root-{MINI}"));
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel = store.open_channel(&era, C).unwrap().unwrap();
    let unit = UnitKey {
        channel_id: C,
        provider: ShadowProvider::Codex,
        native_key: "msg_c1".into(),
        kind: UnitKind::Body,
    };
    let (_, piece) = channel
        .ledger()
        .latest_piece(&unit, 0)
        .expect("the sealed piece");
    let accepted: u64 = get(scene.dir(), "hello_id").unwrap().parse().unwrap();
    assert_eq!(piece.outcome, Some(PieceOutcome::Posted(accepted)));
    scene.drop_db().await;
}

pub(super) async fn unsealed_node(env: &Env) {
    let epoch = epoch_of(env);
    let node = Booted::boot_as(env, ShadowProvider::Codex).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    append(&node.transcript, format!("{ANNOUNCED}\n").as_bytes());
    // Captured through the file's end: a cursor fact the unsealed count does not feed.
    let captured = || async { node.owed().await.is_some_and(|owed| owed.uncaptured == 0) };
    until_async("the actor captured the announcement", WITHIN, captured).await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    let gate = node.gate().expect("registered");
    let ran = || node.drain_blocker().is_some() || gate.final_closed(epoch);
    until("the drain runs a round", WITHIN, ran).await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let (blocker, running) = (node.drain_blocker(), deliver::posts_in_flight(C));
    let owed = node.owed().await;
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "the row left while the message was unsealed: blocker {blocker:?}, projection {owed:?}, \
         in flight {running:?}"
    );
    let alone = Owed {
        unsealed: 1,
        ..Owed::default()
    };
    assert_eq!(
        owed,
        Some(alone),
        "the unsealed message is the only responsibility"
    );
    assert_eq!(blocker.as_deref(), Some("owed"));
    assert_eq!(running, Some(0));
    assert_eq!(
        node.discord().seen(),
        Vec::<Seen>::new(),
        "nothing posted before the seal"
    );
    append(&node.transcript, format!("{SEALED}\n").as_bytes());
    node.until_posts(&["hello"]).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the sealed message is delivered", WITHIN, clear).await;
    until_left(&node).await;
    let posted = node.discord().history_after(C, 0).await.unwrap();
    assert_eq!(posted.len(), 1);
    put(&env.dir, "hello_id", posted[0].id);
    let started = Seen::Started {
        content: "hello".into(),
        home_epoch: Some(epoch),
    };
    let once = [
        started,
        Seen::Accepted("hello".into()),
        Seen::Ended("hello".into()),
    ];
    assert_eq!(node.discord().seen(), once);
    node.stop().await;
}

/// T-H7 pending bind alone: a Clear bind whose transcript does not exist yet keeps the reclaim
/// draining; once it resolves to a file with a row, the real actor posts that row once and leaves.
#[tokio::test(flavor = "current_thread")]
async fn th7_pending_bind_alone_blocks_reclaim_pg() {
    let (scene, _) = reclaim_while("th7_binding", Duration::from_secs(180)).await;
    let root = scene.dir().join(format!("root-{MINI}"));
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel = store.open_channel(&era, C).unwrap().unwrap();
    let unit = UnitKey {
        channel_id: C,
        provider: ShadowProvider::Claude,
        native_key: "n1:0".into(),
        kind: UnitKind::Body,
    };
    let (_, piece) = channel
        .ledger()
        .latest_piece(&unit, 0)
        .expect("the bound source's piece");
    let accepted: u64 = get(scene.dir(), "bound_id").unwrap().parse().unwrap();
    assert_eq!(piece.outcome, Some(PieceOutcome::Posted(accepted)));
    scene.drop_db().await;
}

pub(super) async fn binding_node(env: &Env) {
    let (node, epoch) = holder_with_baseline(env).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the baseline source is drained", WITHIN, clear).await;
    let log = node.host.bindings(C, ShadowProvider::Claude);
    let BindingRecord::Bound {
        new: BindingTarget::Source(first),
        ..
    } = log.first().record
    else {
        panic!("seq 1 binds the baseline source");
    };
    let path = node.root.join("s2.jsonl");
    assert!(
        !path.exists(),
        "the cleared session's transcript is not written yet"
    );
    let evidence = BindingEvidence {
        hook_event: "SessionStart".into(),
        received_at: chrono::Utc::now(),
        reclaims: false,
    };
    let pending = BindingTarget::Pending {
        payload_session_id: "s2".into(),
        payload_transcript_path: path.clone(),
    };
    let bound = BindingRecord::Bound {
        old: Some(first),
        new: pending,
        cause: BindingCause::Clear,
        parent_hint: None,
        evidence,
    };
    assert_eq!(log.commit(bound), 2);
    // A read of the log returned seq 2: a fact the projection's pending count does not feed.
    until("the actor read the pending bind", WITHIN, || {
        log.served() >= 2
    })
    .await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    let gate = node.gate().expect("registered");
    let ran = || node.drain_blocker().is_some() || gate.final_closed(epoch);
    until("the drain runs a round", WITHIN, ran).await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let (blocker, running) = (node.drain_blocker(), deliver::posts_in_flight(C));
    let owed = node.owed().await;
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "the row left while the bind was pending: blocker {blocker:?}, projection {owed:?}, \
         in flight {running:?}, posts {:?}",
        node.discord().posts()
    );
    let alone = Owed {
        binding_pending: 1,
        ..Owed::default()
    };
    assert_eq!(
        owed,
        Some(alone),
        "the pending bind is the only responsibility"
    );
    assert_eq!(blocker.as_deref(), Some("owed"));
    assert_eq!(running, Some(0));
    assert_eq!(
        node.discord().posts(),
        ["base"],
        "nothing posted before it resolved"
    );
    assert!(!node.halted());
    assert_eq!(
        checkpoint(&node),
        Some(1),
        "the pending bind is not applied"
    );
    // The file comes first: the source id reads its metadata.
    std::fs::write(&path, row("n1", "bound-new")).unwrap();
    let source = source_id_for("s2", &path).unwrap();
    let resolved = BindingRecord::Resolved {
        resolves_seq: 2,
        source: source.clone(),
    };
    assert_eq!(log.commit(resolved), 3);
    node.until_posts(&["base", "bound-new"]).await;
    until_async("the bound row is delivered", WITHIN, clear).await;
    assert_eq!(
        checkpoint(&node),
        Some(3),
        "the bind and its resolution are applied"
    );
    let link = rotation(&node)
        .link(&source)
        .cloned()
        .expect("the bound source's link");
    assert_eq!(link.boundary, Boundary::Owed { from: 0 });
    let rows = o_channel_activation::activation_rows(&node.pool, &C.to_string(), MINI).await;
    assert_eq!(rows.unwrap().open_intake, 0);
    until_left(&node).await;
    let posted = node.discord().history_after(C, 0).await.unwrap();
    let bound: Vec<_> = posted.iter().filter(|m| m.content == "bound-new").collect();
    assert_eq!(bound.len(), 1);
    put(&env.dir, "bound_id", bound[0].id);
    let seen = node.discord().seen();
    let started = Seen::Started {
        content: "bound-new".into(),
        home_epoch: Some(epoch),
    };
    let once = [
        started,
        Seen::Accepted("bound-new".into()),
        Seen::Ended("bound-new".into()),
    ];
    assert_eq!(seen[seen.len() - 3..], once, "{seen:?}");
    assert_eq!(
        seen.len(),
        6,
        "base and bound-new, each once, none dropped: {seen:?}"
    );
    none_dropped(&node);
    assert!(!node.halted());
    node.stop().await;
}

/// T-H7 waiting boundary alone: a bind applied to a fresh source whose start nothing decides waits
/// on the operator, and that wait alone keeps the reclaim draining.
#[tokio::test(flavor = "current_thread")]
async fn th7_waiting_boundary_alone_blocks_reclaim_pg() {
    let scene = Scene::new().await;
    let (node, epoch) = reclaim_begun(&scene, None, "th7_waiting_boundary").await;
    node.finish(Duration::from_secs(120)).await;
    let row = scene.row().await.unwrap();
    let kept = (row.state, row.holder.as_deref(), row.target.as_deref());
    assert_eq!(kept, (HomeState::Reclaiming, Some(MINI), Some(GW)));
    assert!(row.epoch >= epoch);
    scene.drop_db().await;
}

pub(super) async fn waiting_boundary_node(env: &Env) {
    let (node, epoch) = holder_with_baseline(env).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the baseline source is drained", WITHIN, clear).await;
    let log = node.host.bindings(C, ShadowProvider::Claude);
    let BindingRecord::Bound {
        new: BindingTarget::Source(first),
        ..
    } = log.first().record
    else {
        panic!("seq 1 binds the baseline source");
    };
    // Empty before the bind: its id reads file metadata, and no row leaves anything to capture.
    let path = node.root.join("s2.jsonl");
    std::fs::write(&path, b"").unwrap();
    let second = source_id_for("s2", &path).unwrap();
    let evidence = BindingEvidence {
        hook_event: "SessionStart".into(),
        received_at: chrono::Utc::now(),
        reclaims: false,
    };
    let bound = BindingRecord::Bound {
        old: Some(first),
        new: BindingTarget::Source(second.clone()),
        cause: BindingCause::Unknown,
        parent_hint: None,
        evidence,
    };
    assert_eq!(log.commit(bound), 2);
    until("the actor read the bind", WITHIN, || log.served() >= 2).await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    let gate = node.gate().expect("registered");
    let ran = || node.drain_blocker().is_some() || gate.final_closed(epoch);
    until("the drain runs a round", WITHIN, ran).await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let (blocker, running) = (node.drain_blocker(), deliver::posts_in_flight(C));
    let owed = node.owed().await;
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "the row left while the boundary waited: blocker {blocker:?}, projection {owed:?}"
    );
    assert_eq!(checkpoint(&node), Some(2), "the bind is applied");
    let link = rotation(&node).link(&second).cloned().expect("s2's link");
    let waiting = Boundary::Pending {
        candidates: vec![0],
    };
    assert_eq!(link.boundary, waiting);
    let alone = Owed {
        binding_pending: 1,
        ..Owed::default()
    };
    assert_eq!(
        owed,
        Some(alone),
        "the waiting boundary is the only responsibility"
    );
    assert_eq!(blocker.as_deref(), Some("owed"));
    assert_eq!(running, Some(0));
    assert_eq!(node.discord().posts(), ["base"]);
    assert!(!node.halted(), "{:?}", node.raised());
    node.stop().await;
}

/// The applied binding seq as the store holds it, read without opening the live actor's channel.
fn checkpoint(node: &Booted) -> Option<u64> {
    let store = OStore::existing(&node.root).expect("the node's store");
    store.peek_binding_checkpoint(C).unwrap()
}

/// The channel's boundary file read as bytes; opening the channel would sweep and recover it.
fn rotation(node: &Booted) -> Rotation {
    let path = node.root.join(STORE_DIR_NAME).join(C.to_string());
    let bytes = std::fs::read(path.join(BOUNDARY_FILE)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
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

/// Reads through the real port, except that the running POST count reads as unknown while
/// `blind` is set.
struct BlindPosts {
    reads: ChannelHomePort,
    blind: Arc<AtomicBool>,
}

impl DrainPort for BlindPosts {
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        self.reads.turn_running()
    }

    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send {
        self.reads.owed()
    }

    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send {
        let blind = self.blind.load(Ordering::SeqCst);
        let real = self.reads.posts_in_flight();
        async move { if blind { None } else { real.await } }
    }

    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send {
        self.reads.reset_legacy_source()
    }
}

/// T-H7: a running POST count the drain cannot read is not zero: the row stays draining while it
/// is unknown, and leaves once it reads zero.
#[tokio::test(flavor = "current_thread")]
async fn th7_unreadable_post_count_blocks_leave_pg() {
    let (scene, _) = reclaim_while("th7_posts_unreadable", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn posts_unreadable_node(env: &Env) {
    let epoch = epoch_of(env);
    // Only the drain reads this port, so the count is unknown from boot without holding delivery.
    let blind = Arc::new(AtomicBool::new(true));
    let switch = Arc::clone(&blind);
    let node = Booted::boot_with(env, move |id| BlindPosts {
        reads: ChannelHomePort::new(id, host::process_readiness(), worker_restored()),
        blind: Arc::clone(&switch),
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
    let gate = node.gate().expect("registered");
    until("the drain's final close", WITHIN, || {
        gate.final_closed(epoch)
    })
    .await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let (blocker, running) = (node.drain_blocker(), deliver::posts_in_flight(C));
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "the row left while the POST count was unreadable: blocker {blocker:?}, in flight {running:?}"
    );
    assert_eq!(blocker.as_deref(), Some("posts_unreadable"));
    assert_eq!(
        running,
        Some(0),
        "no POST runs; only the unreadable count holds the row"
    );
    assert_eq!(node.owed().await, Some(Owed::default()));
    blind.store(false, Ordering::SeqCst);
    until_left(&node).await;
    assert_eq!(node.discord().posts(), ["base"], "no POST while it drained");
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

/// Reads through the real port; the drain's first owed read waits until `go`, after `entered`.
struct FirstCheck {
    reads: ChannelHomePort,
    entered: Arc<AtomicBool>,
    go: Arc<AtomicBool>,
}

impl DrainPort for FirstCheck {
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        self.reads.turn_running()
    }

    async fn owed(&self) -> Option<Owed> {
        if !self.entered.swap(true, Ordering::SeqCst) {
            while !self.go.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        self.reads.owed().await
    }

    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send {
        self.reads.posts_in_flight()
    }

    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send {
        self.reads.reset_legacy_source()
    }
}

/// T-H7 first check: the drain's first responsibility check runs with intake closed and delivery
/// still admitted, so the owed tail posts during it and the row leaves only after.
#[tokio::test(flavor = "current_thread")]
async fn th7_first_check_still_admits_delivery_pg() {
    let (scene, _) = reclaim_while("th7_first_check", Duration::from_secs(180)).await;
    scene.drop_db().await;
}

pub(super) async fn first_check_node(env: &Env) {
    let epoch = epoch_of(env);
    let (entered, go) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let latch = (Arc::clone(&entered), Arc::clone(&go));
    let node = Booted::boot_with(env, move |id| FirstCheck {
        reads: ChannelHomePort::new(id, host::process_readiness(), worker_restored()),
        entered: Arc::clone(&latch.0),
        go: Arc::clone(&latch.1),
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
    until_async("nothing owed before the tail", WITHIN, clear).await;
    node.lease().withhold();
    append(&node.transcript, &row("t1", "tail-1"));
    let tail = Owed {
        owed: 1,
        ..Owed::default()
    };
    until_async("the actor owes the tail", WITHIN, || async {
        node.owed().await == Some(tail)
    })
    .await;
    signal(&env.dir, "owed_seen");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    let checking = || entered.load(Ordering::SeqCst);
    until("the drain's first owed check", WITHIN, checking).await;
    let gate = node.gate().expect("registered");
    let closed = matches!(gate.ownership(), HomeOwnership::Owned { home_epoch, intake: HomeIntake::Closed, .. }
        if home_epoch == epoch);
    assert!(closed, "intake closes first: {:?}", gate.ownership());
    let admitted = gate.admit_post(|_| ()).is_some();
    assert!(
        admitted,
        "delivery is still admitted while the drain checks"
    );
    assert_eq!(node.state().await, Some(HomeState::Reclaiming));
    node.lease().grant();
    node.until_posts(&["base", "tail-1"]).await;
    assert_eq!(
        node.state().await,
        Some(HomeState::Reclaiming),
        "left mid-check"
    );
    go.store(true, Ordering::SeqCst);
    until_left(&node).await;
    assert_eq!(tail_epochs(&node, "tail"), [Some(epoch)]);
    none_dropped(&node);
    node.stop().await;
}
