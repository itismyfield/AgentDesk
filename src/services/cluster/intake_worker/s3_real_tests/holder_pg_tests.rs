//! T-H6: the worker holder keeps the channel across its own restart on the same root and across
//! a move of the gateway role; only the PG home row decides who holds it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::harness::*;
use super::route_pg::{owner, replies, routed, tick};
use crate::db::o_channel_homes::HomeState;
use crate::services::cluster::channel_home::HomeOwnership;
use crate::services::cluster::channel_home_drain::Owed;
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::cluster::intake_worker::TickOutcome;
use crate::services::tui_o::channel_policy::{self, Site};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::store::spool::{Cursor, source_key};
use crate::services::tui_o::store::{CURSOR_DIR, OStore, SPOOL_DIR, STORE_DIR_NAME, StoreConfig};
use crate::services::tui_o::writer::DiscordPort;

const WITHIN: Duration = Duration::from_secs(60);
const HISTORY: &str = "discord-history.jsonl";
const TAIL: [&str; 2] = ["tail-1", "tail-2"];

fn epoch_of(env: &Env) -> i64 {
    get(&env.dir, "epoch").unwrap().parse().unwrap()
}

/// The PG row as `(state, holder, epoch)`.
async fn home(scene: &Scene) -> (HomeState, Option<String>, i64) {
    let row = scene.row().await.expect("the home row");
    (row.state, row.holder, row.epoch)
}

fn channel_dir(root: &Path) -> PathBuf {
    root.join(STORE_DIR_NAME).join(C.to_string())
}

/// Every POST of the node's process ran to its end, `expected` in order and nothing else.
fn each_once(node: &Booted, expected: &[&str]) {
    let once = expected.iter().flat_map(|content| {
        let started = Seen::Started {
            content: content.to_string(),
            home_epoch: node.home_epoch(),
        };
        [
            started,
            Seen::Accepted(content.to_string()),
            Seen::Ended(content.to_string()),
        ]
    });
    assert_eq!(node.discord().seen(), once.collect::<Vec<_>>());
}

/// T-H6 restart: a worker holder killed after its actor spooled two owed pieces, before any of
/// them was prepared or posted, comes back on the same root and posts each from its store once.
#[tokio::test(flavor = "current_thread")]
async fn th6_worker_holder_restart_rehydrates_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let held = (HomeState::Worker, Some(MINI.to_string()), epoch);
    let first = Node::spawn(&scene, "th6_restart_first", MINI);
    wait_signal(scene.dir(), "tail_spooled", WITHIN).await;
    first.kill();
    eprintln!(
        "[s3] first generation {}",
        get(scene.dir(), "spooled").unwrap()
    );
    assert_eq!(home(&scene).await, held, "the crash moved nothing in PG");
    let second = Node::spawn(&scene, "th6_restart_second", MINI);
    second.finish(Duration::from_secs(120)).await;
    assert_eq!(
        home(&scene).await,
        held,
        "the restart kept holder and epoch"
    );
    let history = std::fs::read_to_string(scene.dir().join(HISTORY)).unwrap();
    let accepted: Vec<serde_json::Value> = (history.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let contents: Vec<_> = accepted
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        contents,
        ["base", TAIL[0], TAIL[1]],
        "each payload accepted once"
    );
    // Every node has exited, so the store is opened only now.
    let root = scene.dir().join(format!("root-{MINI}"));
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel = store.open_channel(&era, C).unwrap().unwrap();
    assert_eq!(
        channel.ledger().next_serial(),
        3,
        "one Prepared per payload"
    );
    for (native, message) in ["m0:0", "t1:0", "t2:0"].into_iter().zip(&accepted) {
        let unit = UnitKey {
            channel_id: C,
            provider: ShadowProvider::Claude,
            native_key: native.into(),
            kind: UnitKind::Body,
        };
        let (_, piece) = channel.ledger().latest_piece(&unit, 0).expect(native);
        let id = message["id"].as_u64().unwrap();
        assert_eq!(piece.outcome, Some(PieceOutcome::Posted(id)), "{native}");
    }
    scene.drop_db().await;
}

/// First generation: the baseline goes out, then with the lease withheld the actor spools the
/// two tail rows and its cursor passes them; the parent kills it before any is prepared.
pub(super) async fn first_generation_node(env: &Env) {
    let epoch = epoch_of(env);
    let node = Booted::boot_on(env, Discord::kept_in(&env.dir.join(HISTORY))).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    append(&node.transcript, &row("m0", "base"));
    node.until_posts(&["base"]).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the baseline settles", WITHIN, clear).await;
    node.lease().withhold();
    let tail = [row("t1", TAIL[0]), row("t2", TAIL[1])].concat();
    append(&node.transcript, &tail);
    let owes_tail = || async {
        let tail = Owed {
            owed: 2,
            ..Owed::default()
        };
        node.owed().await == Some(tail)
    };
    until_async("the actor owes both tail pieces", WITHIN, owes_tail).await;
    let dir = channel_dir(&node.root);
    let key = source_key(&source_id_for("s1", &node.transcript).unwrap());
    let cursor = std::fs::read(dir.join(CURSOR_DIR).join(format!("{key}.json"))).unwrap();
    let cursor: Cursor = serde_json::from_slice(&cursor).unwrap();
    let length = std::fs::metadata(&node.transcript).unwrap().len();
    assert_eq!(
        cursor.captured_through, length,
        "the cursor passed both tail rows"
    );
    let segments: Vec<_> = (std::fs::read_dir(dir.join(SPOOL_DIR)).unwrap())
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .collect();
    let spooled: String = (segments.iter())
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    for content in TAIL {
        let framed = spooled
            .lines()
            .filter(|l| l.starts_with("r ") && l.contains(content));
        assert_eq!(framed.count(), 1, "{content} is one spooled record");
    }
    let ledger = std::fs::read_to_string(dir.join("ledger.jsonl")).unwrap();
    let entries: Vec<serde_json::Value> = (ledger.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<_> = entries
        .iter()
        .map(|e| e["entry"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["prepared", "posted"],
        "only the baseline was prepared"
    );
    assert_eq!(node.discord().posts(), ["base"], "no tail POST started");
    assert_eq!(deliver_running(), Some(0));
    let names: Vec<_> = (segments.iter())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    put(
        &env.dir,
        "spooled",
        format!("{names:?} cursor {key}.json through {length}"),
    );
    signal(&env.dir, "tail_spooled");
    // Killed by the parent from here; it never ends on its own.
    std::future::pending::<()>().await;
}

fn deliver_running() -> Option<usize> {
    crate::services::tui_o::writer::deliver::posts_in_flight(C)
}

/// Second generation on the same root: the store's spool and ledger come back and the actor
/// posts the two tail pieces once each, the baseline never again.
pub(super) async fn second_generation_node(env: &Env) {
    let epoch = epoch_of(env);
    let discord = Discord::kept_in(&env.dir.join(HISTORY));
    let kept: Vec<_> = discord.kept().into_iter().map(|m| m.content).collect();
    assert_eq!(kept, ["base"], "the history the first generation left");
    let node = Booted::boot_on(env, discord).await;
    until("the lease opens the home again", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    node.until_posts(&TAIL).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("the rehydrated tail settles", WITHIN, clear).await;
    each_once(&node, &TAIL);
    let history = node.discord().history_after(C, 0).await.unwrap();
    let contents: Vec<_> = history.into_iter().map(|m| m.content).collect();
    assert_eq!(contents, ["base", TAIL[0], TAIL[1]]);
    assert!(!node.halted());
    node.stop().await;
}

/// T-H6 gateway role: moving the role from MINI to a new GW keeps PG holder and epoch and MINI posts
/// GW's routed message; this is role input against PG home authority, not a lease election.
#[tokio::test(flavor = "current_thread")]
async fn th6_gateway_role_move_keeps_holder_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let held = (HomeState::Worker, Some(MINI.to_string()), epoch);
    assert_eq!(home(&scene).await, held);
    let mini = Node::spawn(&scene, "th6_role_holder", MINI);
    wait_signal(scene.dir(), "holder_ready", WITHIN).await;
    assert_eq!(home(&scene).await, held, "with the gateway role on MINI");
    let gw = Node::spawn(&scene, "th6_role_gateway", GW);
    mini.finish(Duration::from_secs(180)).await;
    gw.finish(Duration::from_secs(60)).await;
    assert_eq!(
        home(&scene).await,
        held,
        "with the gateway role moved to GW"
    );
    let done = ("done".to_string(), MINI.to_string(), Some(epoch));
    assert_eq!(routed(&scene.pool).await, [done]);
    scene.drop_db().await;
}

/// The holder booted while it is itself the gateway role: it opens the home, then posts the reply
/// to what the new gateway routed, still at its epoch.
pub(super) async fn role_holder_node(env: &Env) {
    let epoch = epoch_of(env);
    let node = Booted::boot_with_gateway(env, MINI).await;
    let boot = channel_policy::boot().expect("boot snapshot");
    assert_eq!(
        boot.site(),
        &Site::Home,
        "the boot read MINI as the gateway"
    );
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    let shared = owner().await;
    let (_hook, recorder) = replies(&node);
    signal(&env.dir, "holder_ready");
    wait_signal(&env.dir, "gw_routed", WITHIN).await;
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Processed);
    assert_eq!(recorder.channels(), [C]);
    node.until_posts(&["reply-1"]).await;
    let epochs = node.discord().started_epochs();
    assert_eq!(epochs, [("reply-1".to_string(), Some(epoch))]);
    let gate = node.gate().expect("registered");
    assert!(
        matches!(gate.ownership(), HomeOwnership::Owned { home_epoch, .. } if home_epoch == epoch)
    );
    assert!(node.accepts());
    signal(&env.dir, "holder_done");
    node.stop().await;
}

/// The new gateway: its real boot names it the gateway role, its home boot finds no row for it
/// and its router forwards the message to MINI; its delegated writer starts no POST.
pub(super) async fn role_gateway_node(env: &Env) {
    let node = Booted::boot_with_gateway(env, GW).await;
    let boot = channel_policy::boot().expect("boot snapshot");
    assert_eq!(boot.site(), &Site::Home, "the boot read GW as the gateway");
    assert!(
        node.gates.is_empty(),
        "the row names the new gateway nowhere"
    );
    let shared = owner().await;
    wait_signal(&env.dir, "holder_ready", WITHIN).await;
    let routed = route_as(&node.pool, GW, ShadowProvider::Claude, "1001").await;
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
        "the new gateway's writer never takes the channel"
    );
    assert_eq!(
        node.discord().seen(),
        Vec::<Seen>::new(),
        "no delegated POST started"
    );
    node.stop().await;
}
