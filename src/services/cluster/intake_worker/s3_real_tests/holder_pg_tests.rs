//! T-H6: the worker holder keeps the channel across its own restart on the same root and across
//! a move of the gateway role; only the PG home row decides who holds it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::harness::*;
use super::route_pg::{lock_home_row, owner, replies, replies_tagged, routed, tick};
use crate::db::o_channel_homes::{self, HomeState};
use crate::services::cluster::channel_home::HomeOwnership;
use crate::services::cluster::channel_home_drain::Owed;
use crate::services::cluster::channel_home_port::ChannelHomePort;
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::cluster::intake_worker::TickOutcome;
use crate::services::tui_o::channel_policy::{self, Site};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::store::spool::{Cursor, source_key};
use crate::services::tui_o::store::{CURSOR_DIR, OStore, SPOOL_DIR, STORE_DIR_NAME, StoreConfig};
use crate::services::tui_o::writer::DiscordPort;
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::host;

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

/// Every POST of the node's process started at `epoch` and ran to its end, `expected` in order
/// and nothing else.
fn each_once(node: &Booted, expected: &[&str], epoch: i64) {
    let once = expected.iter().flat_map(|content| {
        let started = Seen::Started {
            content: content.to_string(),
            home_epoch: Some(epoch),
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
    let mut first = Node::spawn(&scene, "th6_restart_first", MINI);
    first.wait_signal("tail_spooled", WITHIN).await;
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
    let _node = holder_with_owed_tail(env).await;
    signal(&env.dir, "tail_spooled");
    // Killed by the parent from here; it never ends on its own.
    std::future::pending::<()>().await;
}

/// The holder at its epoch after the baseline posted: lease withheld, both tail rows spooled past
/// the cursor and owed, none prepared or posted; the store files are read, never opened.
pub(super) async fn holder_with_owed_tail(env: &Env) -> Booted {
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
    node
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
    each_once(&node, &TAIL, epoch);
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
    let mut mini = Node::spawn(&scene, "th6_role_holder", MINI);
    mini.wait_signal("holder_ready", WITHIN).await;
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
/// and its router forwards the message to MINI; a row in its own transcript is never posted.
pub(super) async fn role_gateway_node(env: &Env) {
    let node = Booted::boot_with_gateway(env, GW).await;
    append(&node.transcript, &row("g1", "gw-must-not-post"));
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
    // Past several actor polls since the probe row was written.
    tokio::time::sleep(POLL_INTERVAL * 3).await;
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

/// The window both nodes watch while the holder's POST is in flight: two renewal periods and a
/// margin.
const TWO_ROUNDS: Duration = Duration::from_secs(12);

/// The PG row as `(state, holder, target, epoch)`.
async fn home_row(scene: &Scene) -> (HomeState, Option<String>, Option<String>, i64) {
    let row = scene.row().await.expect("the home row");
    (row.state, row.holder, row.target, row.epoch)
}

/// Waits for this generation's `name` from each node.
async fn from_both(nodes: [&mut Node; 2], name: &str, generation: u32) {
    for node in nodes {
        let signal = format!("{name}.g{generation}.{}", node.role());
        node.wait_signal(&signal, WITHIN).await;
    }
}

/// T-H6 both restart: both nodes boot together on one PG, then again on the same roots after a
/// kill; each time only the PG holder opens and posts, and no two processes' POSTs overlap.
#[tokio::test(flavor = "current_thread")]
async fn th6_both_restart_only_one_owner_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let held = (HomeState::Worker, Some(MINI.to_string()), None, epoch);
    for generation in [1, 2] {
        put(scene.dir(), "generation", generation);
        let to_both = |name: &str| signal(scene.dir(), &format!("{name}.g{generation}"));
        let lock = lock_home_row(&scene.pool).await;
        let mut mini = Node::spawn(&scene, "th6_both_holder", MINI);
        let mut gw = Node::spawn(&scene, "th6_both_gateway", GW);
        let (m, g) = ((mini.name(), mini.pid()), (gw.name(), gw.pid()));
        eprintln!("[s3] generation {generation}: {m:?} and {g:?}");
        from_both([&mut mini, &mut gw], "ready_to_boot", generation).await;
        to_both("boot_go");
        from_both([&mut mini, &mut gw], "boot_observed", generation).await;
        assert_eq!(home_row(&scene).await, held, "booted under the row lock");
        lock.rollback().await.unwrap();
        from_both([&mut mini, &mut gw], "in_flight", generation).await;
        to_both("window");
        from_both([&mut mini, &mut gw], "window_done", generation).await;
        to_both("release");
        from_both([&mut mini, &mut gw], "quiescent", generation).await;
        for node in [&mini, &gw] {
            let seen = get(
                scene.dir(),
                &format!("window.g{generation}.{}", node.role()),
            );
            eprintln!("[s3] {} window: {}", node.name(), seen.unwrap());
        }
        let events = lifecycle(scene.dir()).unwrap();
        if let Err(error) = post_spans(&events) {
            panic!("a POST open or broken before the kill: {error}\n{events:#?}");
        }
        assert_eq!(
            home_row(&scene).await,
            held,
            "generation {generation} moved PG"
        );
        if generation == 1 {
            mini.kill();
            gw.kill();
        } else {
            mini.finish(WITHIN).await;
            gw.finish(WITHIN).await;
        }
    }
    let events = lifecycle(scene.dir()).unwrap();
    for event in &events {
        eprintln!("[s3] lifecycle {}", serde_json::to_string(event).unwrap());
    }
    let spans = post_spans(&events).unwrap_or_else(|error| panic!("{error}\n{events:#?}"));
    let posted: Vec<_> = spans
        .iter()
        .map(|span| (span.node.as_str(), span.content.as_str(), span.accepted))
        .collect();
    let replies = [(MINI, "g1-reply-1", true), (MINI, "g2-reply-1", true)];
    assert_eq!(posted, replies, "one new POST per generation, all MINI's");
    let spawns: Vec<_> = spans.iter().map(|span| span.spawn.as_str()).collect();
    assert_ne!(spawns[0], spawns[1], "each generation is its own process");
    for span in &spans {
        assert_eq!(
            span.observed,
            Some((Some(epoch), EpochSource::Gate)),
            "{span:?} posted outside the held epoch"
        );
    }
    let started = events.iter().filter(|e| e.step == LifeStep::Started);
    for event in started {
        let fixed = (event.epoch, event.epoch_source);
        assert_eq!(fixed, (Some(epoch), Some(EpochSource::Scene)));
    }
    scene.drop_db().await;
}

fn generation_of(env: &Env) -> String {
    get(&env.dir, "generation").unwrap()
}

/// This node's signal `name` in the running generation.
fn step(env: &Env, name: &str) -> String {
    format!("{name}.g{}.{}", generation_of(env), env.role)
}

/// The parent's signal `name` to both nodes in the running generation.
fn both_step(env: &Env, name: &str) -> String {
    format!("{name}.g{}", generation_of(env))
}

/// Boots with every other node of the generation once the parent says so; the row lock keeps
/// the holder's renewal waiting until both report what they booted into.
async fn boot_together(env: &Env) -> Booted {
    signal(&env.dir, &step(env, "ready_to_boot"));
    wait_signal(&env.dir, &both_step(env, "boot_go"), WITHIN).await;
    Booted::boot_on(env, Discord::default().recording(env)).await
}

/// The holder in either generation: Lost until its renewal, then it posts the reply to what the
/// gateway routed, held in flight through the window, and ends with nothing open or owed.
pub(super) async fn both_holder_node(env: &Env) {
    let (epoch, generation) = (epoch_of(env), generation_of(env));
    let node = boot_together(env).await;
    let gate = node.gate().expect("the row names MINI its holder");
    assert_eq!(gate.ownership(), HomeOwnership::Lost, "before any renewal");
    assert!(!node.accepts());
    signal(&env.dir, &step(env, "boot_observed"));
    until("the renewal opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    let shared = owner().await;
    let (_hook, recorder) = replies_tagged(&node, &format!("g{generation}-"));
    signal(&env.dir, &step(env, "holder_ready"));
    let routed = format!("gw_routed.g{generation}.{GW}");
    wait_signal(&env.dir, &routed, WITHIN).await;
    node.discord().hold();
    assert_eq!(tick(&node, &shared, MINI).await, TickOutcome::Processed);
    assert_eq!(recorder.channels(), [C]);
    let reply = format!("g{generation}-reply-1");
    until("the reply's POST starts", WITHIN, || {
        node.discord().started(&reply)
    })
    .await;
    signal(&env.dir, &step(env, "in_flight"));
    wait_signal(&env.dir, &both_step(env, "window"), WITHIN).await;
    let flying = || node.discord().running() == 1 && node.home_epoch() == Some(epoch);
    let window = watch_window(env, flying).await;
    assert_eq!(window, None, "the held POST left the window");
    signal(&env.dir, &step(env, "window_done"));
    wait_signal(&env.dir, &both_step(env, "release"), WITHIN).await;
    node.discord().release();
    node.until_posts(&[reply.as_str()]).await;
    until("the request ends", WITHIN, || node.discord().running() == 0).await;
    let posting = crate::services::tui_o::writer::deliver::posts_in_flight;
    until("no POST in flight", WITHIN, || posting(C) == Some(0)).await;
    let clear = || async { node.owed().await == Some(Owed::default()) };
    until_async("nothing owed", WITHIN, clear).await;
    each_once(&node, &[reply.as_str()], epoch);
    if generation == "2" {
        assert!(node.accepts(), "the restarted holder takes work");
        let readiness = crate::services::tui_o::writer::host::process_readiness();
        // No await until the refusal, so this current-thread runtime cannot run the publisher.
        gate.close();
        assert!(
            readiness.is_ready(C),
            "the publisher has not seen the close"
        );
        assert!(
            !node.accepts(),
            "a restarted ready writer re-reads its closed home"
        );
    }
    signal(&env.dir, &step(env, "quiescent"));
    finish_generation(env, node).await;
}

/// The gateway in either generation: no gate and no writer, a probe row it must never post, and
/// the message it routes to MINI; it stays shut through the window MINI's POST is in flight.
pub(super) async fn both_gateway_node(env: &Env) {
    let generation = generation_of(env);
    let node = boot_together(env).await;
    assert!(node.gates.is_empty(), "the row names GW nowhere");
    assert!(node.writers.is_empty(), "no delegated writer");
    assert!(!node.accepts());
    signal(&env.dir, &step(env, "boot_observed"));
    let probe = format!("gw-must-not-post-g{generation}");
    append(&node.transcript, &row(&format!("gw{generation}"), &probe));
    let ready = format!("holder_ready.g{generation}.{MINI}");
    wait_signal(&env.dir, &ready, WITHIN).await;
    let message = format!("70{generation}1");
    let routed = route_as(&node.pool, GW, ShadowProvider::Claude, &message).await;
    assert!(
        matches!(&routed, IntakeRouterDecision::Forwarded { target_instance_id, .. }
            if target_instance_id == MINI),
        "{routed:?}"
    );
    signal(&env.dir, &step(env, "gw_routed"));
    signal(&env.dir, &step(env, "in_flight"));
    wait_signal(&env.dir, &both_step(env, "window"), WITHIN).await;
    let shut = || node.gate().is_none() && !node.accepts() && node.discord().seen().is_empty();
    let window = watch_window(env, shut).await;
    assert_eq!(window, None, "the gateway opened or posted");
    signal(&env.dir, &step(env, "window_done"));
    wait_signal(&env.dir, &both_step(env, "release"), WITHIN).await;
    assert!(node.gate().is_none() && !node.accepts());
    assert_eq!(node.discord().seen(), Vec::<Seen>::new(), "no POST started");
    signal(&env.dir, &step(env, "quiescent"));
    finish_generation(env, node).await;
}

/// Samples `holds` every 100 ms through [`TWO_ROUNDS`], the last sample at or after its end; the
/// first failed sample sticks, and so does too thin a watch. The samples go to the parent.
async fn watch_window(env: &Env, holds: impl Fn() -> bool) -> Option<String> {
    let start = tokio::time::Instant::now();
    let (mut offsets, mut failed) = (Vec::new(), None);
    loop {
        let at = start.elapsed();
        offsets.push(at);
        if !holds() && failed.is_none() {
            failed = Some(format!("sample {} at {at:?}", offsets.len()));
        }
        if at >= TWO_ROUNDS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let watched = observed_throughout(&offsets, TWO_ROUNDS);
    let seen = format!("{watched:?}: {} samples", offsets.len());
    put(
        &env.dir,
        &format!("window.g{}.{}", generation_of(env), env.role),
        seen,
    );
    failed.or(watched.err())
}

/// Generation 1 waits to be killed; generation 2 stops and ends.
async fn finish_generation(env: &Env, node: Booted) {
    if generation_of(env) == "1" {
        std::future::pending::<()>().await;
    }
    node.stop().await;
}

/// T-H6 releasing restart: the GW holder dies inside its reset call; the next generation leaves only
/// after an allowed retry. Applied is the injected callback's `Ok(())`, not a real session reset.
#[tokio::test(flavor = "current_thread")]
async fn th6_releasing_holder_restart_retries_reset_pg() {
    let scene = Scene::new().await;
    seed_outbox_samples(&scene.pool).await;
    let samples = outbox_samples(&scene.pool).await;
    let channel = C.to_string();
    let home = applied(o_channel_homes::delegate(&scene.pool, &channel, "claude", GW, MINI).await);
    put(scene.dir(), "epoch", home.epoch);
    let releasing = home_facts(&scene.pool).await.expect("the home row");
    assert_eq!(
        (releasing.state, releasing.detail.as_deref()),
        (HomeState::Releasing, None)
    );
    let mut first = Node::spawn(&scene, "th6_release_first", GW);
    let tag = first.name().to_string();
    let (dir, entered) = (scene.dir(), format!("reset.{tag}.1.entered"));
    // The child judges its own readiness; here its exit before the call fails with its log.
    let alive = || {
        first.alive_before(&entered);
        false
    };
    until_reset_entered(&scene.pool, dir, (&tag, 1), &releasing, alive).await;
    // The call's entry alone is not the child's verdict; it signals once its own checks passed.
    first
        .wait_signal(&format!("checked.{tag}"), Duration::from_secs(180))
        .await;
    quiet_at_reset(&scene.pool, &samples).await;
    first.kill();
    assert_eq!(reset_returned(dir, &tag, 1), None, "killed inside the call");
    let kept = home_facts(&scene.pool).await;
    assert_eq!(kept.as_ref(), Some(&releasing), "the crash moved the row");
    let second = Node::spawn(&scene, "th6_release_second", GW);
    let again = second.name().to_string();
    second.finish(Duration::from_secs(180)).await;
    let left = home_facts(&scene.pool).await.expect("the home row");
    assert_eq!(released_from(&releasing, &left), Ok(()), "{left:?}");
    assert_eq!(
        reset_calls(dir, &tag),
        1,
        "the first generation called once"
    );
    let calls: Vec<_> = (1..=reset_calls(dir, &again))
        .map(|n| reset_returned(dir, &again, n))
        .collect();
    let refused = Some(format!("refused: {REFUSAL}"));
    assert_eq!(calls, [refused, Some("applied".into())], "{again}");
    assert_eq!(outbox_samples(&scene.pool).await, samples);
    scene.drop_db().await;
}

/// What the second generation's first reset call refuses with.
const REFUSAL: &str = "the gateway session is busy";

/// `left` is `releasing` after its drain's leave: Released, no holder, the same target, channel,
/// provider and detail, at a later epoch from the shared sequence.
pub(super) fn released_from(releasing: &HomeFacts, left: &HomeFacts) -> Result<(), String> {
    let kept = HomeFacts {
        state: HomeState::Released,
        holder: None,
        epoch: left.epoch,
        ..releasing.clone()
    };
    if *left != kept || left.epoch <= releasing.epoch {
        return Err(format!("{releasing:?} did not leave as released"));
    }
    Ok(())
}

/// A node holding the Releasing row through a lease at its epoch, with its writer taking work.
fn holding(node: &Booted, epoch: i64) -> bool {
    node.home_epoch() == Some(epoch) && node.accepts()
}

/// First generation: it holds the channel, then its reset holds inside call 1 until the parent
/// kills it; it fails if it stays holding without a break and tries no reset.
pub(super) async fn release_first_node(env: &Env) {
    let (epoch, dir, tag) = (epoch_of(env), env.dir.as_path(), env.spawn.as_str());
    let reset = ScriptedReset::in_child(env, vec![ResetAnswer::Apply]);
    let reads = |id| ChannelHomePort::new(id, host::process_readiness(), worker_restored());
    let node = Booted::boot_with_reset(env, reads, reset).await;
    let releasing = home_facts(&node.pool).await.expect("the home row");
    assert_eq!(
        (releasing.state, releasing.epoch),
        (HomeState::Releasing, epoch)
    );
    until("the holder takes the channel", WITHIN, || {
        holding(&node, epoch)
    })
    .await;
    let held = || holding(&node, epoch);
    until_reset_entered(&node.pool, dir, (tag, 1), &releasing, held).await;
    signal(dir, &format!("checked.{tag}"));
    std::future::pending::<()>().await;
}

/// Second generation on the same root: its boot drains the row again, call 1 refuses with the row
/// unmoved, and call 2's `Ok(())` lets the drain write the leave.
pub(super) async fn release_second_node(env: &Env) {
    let (epoch, dir, tag) = (epoch_of(env), env.dir.as_path(), env.spawn.as_str());
    let script = vec![ResetAnswer::Refuse(REFUSAL), ResetAnswer::Apply];
    let reset = ScriptedReset::in_child(env, script);
    let reads = |id| ChannelHomePort::new(id, host::process_readiness(), worker_restored());
    let node = Booted::boot_with_reset(env, reads, reset).await;
    let pool = &node.pool;
    let releasing = home_facts(pool).await.expect("the home row");
    assert_eq!(
        (releasing.state, releasing.epoch),
        (HomeState::Releasing, epoch)
    );
    let samples = outbox_samples(pool).await;
    until("the holder takes the channel", WITHIN, || {
        holding(&node, epoch)
    })
    .await;
    until_reset_entered(pool, dir, (tag, 1), &releasing, || holding(&node, epoch)).await;
    let gate = node.gate().expect("registered");
    assert!(
        !gate.final_closed(epoch),
        "closed before the reset answered"
    );
    quiet_at_reset(pool, &samples).await;
    refused_unmoved(&node, (dir, tag), REFUSAL, &releasing).await;
    until_reset_entered(pool, dir, (tag, 2), &releasing, || holding(&node, epoch)).await;
    quiet_at_reset(pool, &samples).await;
    applied_then_released(&node, dir, tag, &releasing).await;
    assert_eq!(outbox_samples(pool).await, samples);
    node.stop().await;
}

/// Lets call 1 refuse with `why`; the drain then waits on `source_reset` with the row unmoved.
pub(super) async fn refused_unmoved(
    node: &Booted,
    (dir, tag): (&Path, &str),
    why: &str,
    releasing: &HomeFacts,
) {
    reset_go(dir, tag, 1);
    until("call 1 answers", WITHIN, || {
        reset_returned(dir, tag, 1).is_some()
    })
    .await;
    assert_eq!(reset_returned(dir, tag, 1), Some(format!("refused: {why}")));
    let shown = || async {
        let row = home_facts(&node.pool).await;
        node.drain_blocker().as_deref() == Some("source_reset") || row.as_ref() != Some(releasing)
    };
    until_async("the refusal shows or the row moves", WITHIN, shown).await;
    let row = home_facts(&node.pool).await;
    assert_eq!(
        row.as_ref(),
        Some(releasing),
        "a refused reset moved the row"
    );
    assert_eq!(node.drain_blocker().as_deref(), Some("source_reset"));
}

/// Holds call 2 before it returns `Ok(())`: still Releasing and not finally closed; once it
/// returns the drain leaves as released, and no third call came.
pub(super) async fn applied_then_released(
    node: &Booted,
    dir: &Path,
    tag: &str,
    releasing: &HomeFacts,
) {
    let gate = node.gate().expect("registered");
    assert!(
        !gate.final_closed(releasing.epoch),
        "closed before the reset applied"
    );
    assert_eq!(node.state().await, Some(HomeState::Releasing));
    reset_go(dir, tag, 2);
    let left = || async { node.state().await == Some(HomeState::Released) };
    until_async("the release lands", WITHIN, left).await;
    assert_eq!(reset_returned(dir, tag, 2).as_deref(), Some("applied"));
    let row = home_facts(&node.pool).await.expect("the home row");
    assert_eq!(released_from(releasing, &row), Ok(()), "{row:?}");
    assert!(
        gate.final_closed(releasing.epoch),
        "left without the final close"
    );
    assert_eq!(reset_calls(dir, tag), 2, "a call after the leave");
}

/// T-H6 released target: booted through the runtime's `BootPort::reading`, MINI's watch leaves the
/// released row alone until restored, then adopts it and the lease opens the home; no test adopt.
#[tokio::test(flavor = "current_thread")]
async fn th6_released_target_adopts_through_its_boot_watch_pg() {
    let scene = Scene::new().await;
    let (channel, provider) = (C.to_string(), provider_name(scene.provider));
    let delegated = o_channel_homes::delegate(&scene.pool, &channel, provider, GW, MINI).await;
    let delegated = applied(delegated);
    let released =
        o_channel_homes::finish_release(&scene.pool, &channel, GW, delegated.epoch).await;
    let epoch = applied(released).epoch;
    put(scene.dir(), "epoch", epoch);
    let released = home_facts(&scene.pool).await.expect("the home row");
    let mut target = Node::spawn(&scene, "th6_adopt_target", MINI);
    target.wait_signal("booted", WITHIN).await;
    tokio::time::sleep(TWO_ROUNDS).await;
    let unrestored = home_facts(&scene.pool).await;
    assert_eq!(unrestored.as_ref(), Some(&released), "adopted unrestored");
    signal(scene.dir(), "restored");
    target.finish(Duration::from_secs(120)).await;
    let held = (HomeState::Worker, Some(MINI.to_string()), epoch);
    assert_eq!(home(&scene).await, held, "the target holds the channel");
    assert_eq!(scene.row().await.expect("the home row").target, None);
    scene.drop_db().await;
}

pub(super) async fn adopt_target_node(env: &Env) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let epoch = epoch_of(env);
    let restored = Arc::new(AtomicBool::new(false));
    let node = Booted::boot_reading(env, Arc::clone(&restored)).await;
    assert_eq!(node.gates.len(), 1, "the released row names this node");
    signal(&env.dir, "booted");
    wait_signal(&env.dir, "restored", WITHIN).await;
    assert_eq!(node.state().await, Some(HomeState::Released));
    let opened = (node.home_epoch(), node.accepts());
    assert_eq!(opened, (None, false), "unrestored: the home opened");
    restored.store(true, Ordering::Release);
    let adopted = || async { node.state().await == Some(HomeState::Worker) };
    until_async("the watch adopts the row", WITHIN, adopted).await;
    until("the lease opens the home", WITHIN, || {
        node.home_epoch() == Some(epoch)
    })
    .await;
    until("the writer takes work", WITHIN, || node.accepts()).await;
    node.stop().await;
}
