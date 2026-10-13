//! T-H9: the operator's CLI force on a holder that died, and what each node's real boot, watch,
//! router, claim, readiness and writer do with the orphaned row afterwards.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use super::harness::*;
use super::holder_pg::holder_with_owed_tail;
use crate::cli::channel_home::{ChannelHomeCommand, execute};
use crate::db::o_channel_homes::{self, HomeState, HomeWrite};
use crate::services::cluster::channel_home::HomeOwnership;
use crate::services::cluster::intake_router_hook::IntakeRouterDecision;
use crate::services::cluster::intake_worker::{TickOutcome, run_intake_worker_tick};
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::{OStore, STORE_DIR_NAME, StoreConfig};

const WITHIN: Duration = Duration::from_secs(60);
/// Two periods of each node's watch and lease, and a margin.
const TWO_ROUNDS: Duration = Duration::from_secs(12);
/// How long one good watch read may take to clear the probe; a bound, not an observation window.
const WATCH_READ_WITHIN: Duration = Duration::from_secs(30);

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
    let mut holder = Node::spawn(&scene, "th9_holder", MINI);
    holder.wait_signal("held", WITHIN).await;
    let fresh = force(&scene).await;
    assert!(
        fresh.is_err_and(|e| e.contains("within F")),
        "a live holder"
    );
    holder.kill();
    age_lease(&scene).await;
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
    assert_eq!(claim_state(&scene.pool, stranded).await, unclaimed());
    scene.drop_db().await;
}

/// Fast boundary only: the lease is aged in SQL, not waited out in wall-clock time.
async fn age_lease(scene: &Scene) {
    sqlx::query("UPDATE o_channel_homes SET renewed_at = NOW() - INTERVAL '201 seconds'")
        .execute(&scene.pool)
        .await
        .unwrap();
}

/// Status and claim owner of outbox row `id`.
async fn claim_state(pool: &PgPool, id: i64) -> (String, Option<String>) {
    sqlx::query_as("SELECT status::TEXT, claim_owner FROM intake_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn unclaimed() -> (String, Option<String>) {
    ("pending".to_string(), None)
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

/// What a target marks its gate with; only the watch's next good read of the row clears it.
const PROBE: &str = "s3-watch-probe";
/// Every payload the old holder's root owes or prepared; no target may complete one.
const OLD_PAYLOADS: [&str; 4] = ["base", "tail-1", "tail-2", PREPARED_ONLY];
const PREPARED_ONLY: &str = "h9-prepared-only";

fn epoch_of(env: &Env) -> i64 {
    get(&env.dir, "epoch").unwrap().parse().unwrap()
}

/// MINI holds a Worker home with an owed tail, then the gateway's reclaim makes its real drain
/// wait on that tail, so the row stays Reclaiming until the holder dies.
async fn reclaiming_with_owed_tail(scene: &Scene, epoch: i64) -> Node {
    let mut holder = Node::spawn(scene, "th9_owed_holder", MINI);
    holder.wait_signal("tail_spooled", WITHIN).await;
    let row = scene.row().await.unwrap();
    let owing = (row.state, row.holder.as_deref());
    assert_eq!(
        owing,
        (HomeState::Worker, Some(MINI)),
        "owed before the reclaim"
    );
    applied(o_channel_homes::begin_reclaim(&scene.pool, &C.to_string(), epoch, GW).await);
    signal(scene.dir(), "reclaim_begun");
    holder.wait_signal("draining", WITHIN).await;
    let row = scene.row().await.unwrap();
    let draining = (row.state, row.holder.as_deref(), row.target.as_deref());
    assert_eq!(draining, (HomeState::Reclaiming, Some(MINI), Some(GW)));
    eprintln!("[s3] reclaiming at epoch {} from {epoch}", row.epoch);
    holder
}

/// With the holder dead, ages its lease in SQL and forces: the row is orphaned at a new epoch.
async fn force_orphan(scene: &Scene, epoch: i64) -> i64 {
    age_lease(scene).await;
    let forced: serde_json::Value = serde_json::from_str(&force(scene).await.unwrap()).unwrap();
    assert_eq!(forced["state"], "orphaned");
    let orphan = scene.row().await.unwrap();
    let held = (
        orphan.state,
        orphan.holder.as_deref(),
        orphan.target.as_deref(),
    );
    assert_eq!(held, (HomeState::Orphaned, None, Some(GW)));
    assert!(orphan.epoch > epoch);
    eprintln!("[s3] orphaned at epoch {} from {epoch}", orphan.epoch);
    put(scene.dir(), "orphan_epoch", orphan.epoch);
    orphan.epoch
}

/// T-H6 force with a live target: a target booted while the holder drained stays the same process
/// through the force and never gains the channel; nor does either node rebooted after it.
#[tokio::test(flavor = "current_thread")]
async fn th6_force_live_target_then_reboot_orphan_holds_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let holder = reclaiming_with_owed_tail(&scene, epoch).await;
    let mut target = Node::spawn(&scene, "th6_live_target", GW);
    target.wait_signal("target_alive", WITHIN).await;
    let pid = get(scene.dir(), "target_pid").unwrap();
    assert_eq!(
        pid,
        target.pid().to_string(),
        "the target child wrote its own pid"
    );
    holder.kill();
    let orphan = force_orphan(&scene, epoch).await;
    let stranded = seed_for(&scene.pool, C, GW, "6001", Some(orphan)).await;
    put(scene.dir(), "stranded", stranded);
    signal(scene.dir(), "force_done");
    target.finish(Duration::from_secs(120)).await;
    let samples = get(scene.dir(), &format!("samples.{GW}")).unwrap();
    eprintln!(
        "[s3] live target {pid} shut over {samples} samples, from before it was alive to its end"
    );
    let gw = Node::spawn(&scene, "th9_consumer", GW);
    let mini = Node::spawn(&scene, "th9_consumer", MINI);
    gw.finish(Duration::from_secs(120)).await;
    mini.finish(Duration::from_secs(120)).await;
    let after = scene.row().await.unwrap();
    assert_eq!(
        (after.state, after.holder, after.epoch),
        (HomeState::Orphaned, None, orphan)
    );
    assert_eq!(claim_state(&scene.pool, stranded).await, unclaimed());
    scene.drop_db().await;
}

/// The old holder: owes its tail, then waits on it in its real drain once the reclaim begins.
pub(super) async fn owed_holder_node(env: &Env) {
    let node = holder_with_owed_tail(env).await;
    signal(&env.dir, "tail_spooled");
    wait_signal(&env.dir, "reclaim_begun", WITHIN).await;
    let owed = || node.drain_blocker().as_deref() == Some("owed");
    until("the drain waits on the owed tail", WITHIN, owed).await;
    assert_eq!(node.state().await, Some(HomeState::Reclaiming));
    signal(&env.dir, "draining");
    // Killed by the parent from here; it never ends on its own.
    std::future::pending::<()>().await;
}

/// The live target: booted while the holder drains, its watch reads the row before it says it is
/// alive; one sticky observation runs from then through the force to the end in the same process.
pub(super) async fn live_target_node(env: &Env) {
    let node = Booted::boot(env).await;
    let pid = std::process::id();
    put(&env.dir, "target_pid", pid);
    let gate = node.gate().expect("the row names GW its target");
    assert_eq!(gate.ownership(), HomeOwnership::Lost);
    assert!(!node.accepts());
    let mut seen = Observed::default();
    seen.watch_read(&node).await;
    assert_eq!(seen.opened, None, "shut while the holder drained");
    signal(&env.dir, "target_alive");
    let forced = || signalled(&env.dir, "force_done");
    seen.until(&node, "the force", WITHIN, forced).await;
    assert_eq!(seen.opened, None, "opened while the force was pending");
    holds_orphan(&node, env, "6004", seen).await;
    assert_eq!(
        std::process::id(),
        pid,
        "the same process through the force"
    );
    node.stop().await;
}

/// A target's samples since its observation began, and its first opening, which sticks.
#[derive(Default)]
struct Observed {
    samples: usize,
    opened: Option<String>,
}

impl Observed {
    /// Samples readiness, the gate and the route every 100 ms until `done`, failing after `within`.
    async fn until(
        &mut self,
        node: &Booted,
        what: &str,
        within: Duration,
        mut done: impl FnMut() -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + within;
        while !done() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never reached: {what}"
            );
            self.sample(node);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn sample(&mut self, node: &Booted) {
        self.samples += 1;
        let gate = node.gate().map(|gate| gate.ownership());
        let route = intake_route::route("claude", C);
        let shut = !node.accepts()
            && gate.as_ref().is_none_or(|g| *g == HomeOwnership::Lost)
            && matches!(route, IntakeRoute::Hold(_));
        if !shut && self.opened.is_none() {
            let accepts = node.accepts();
            let sample = self.samples;
            self.opened = Some(format!(
                "sample {sample}: accepts {accepts}, gate {gate:?}, route {route:?}"
            ));
        }
    }

    /// Marks the registered gate and waits until a good watch read of the row clears the mark on
    /// that same gate; a read error marks it again and never counts.
    async fn watch_read(&mut self, node: &Booted) {
        let gate = node.gate().expect("a registered gate");
        gate.note_drain(Some(PROBE));
        let read = || {
            let same = node.gate().is_some_and(|now| Arc::ptr_eq(&now, &gate));
            same && node.drain_blocker().is_none()
        };
        self.until(
            node,
            "a watch read clears the probe",
            WATCH_READ_WITHIN,
            read,
        )
        .await;
    }
}

/// After the force: two good watch reads of the orphaned row and two rounds observed, the target
/// stays unready with its gate Lost, holds and blocks intake, claims nothing and posts nothing.
async fn holds_orphan(node: &Booted, env: &Env, message: &str, mut seen: Observed) {
    let started = tokio::time::Instant::now();
    // The first clear may finish a read begun before the force; the second began after it.
    seen.watch_read(node).await;
    seen.watch_read(node).await;
    let over = move || started.elapsed() >= TWO_ROUNDS;
    seen.until(node, "two rounds after the force", TWO_ROUNDS, over)
        .await;
    assert!(!node.accepts(), "the target took the orphaned channel");
    let gate = node
        .gate()
        .expect("the orphaned row still names GW its target");
    assert_eq!(gate.ownership(), HomeOwnership::Lost);
    let routed_here = intake_route::route("claude", C);
    assert!(
        matches!(routed_here, IntakeRoute::Hold(_)),
        "{routed_here:?}"
    );
    let routed = route(&node.pool, GW, message).await;
    assert!(
        matches!(routed, IntakeRouterDecision::Blocked { .. }),
        "{routed:?}"
    );
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (registry, shared) = owner.await;
    let tick = run_intake_worker_tick(&node.pool, &shared, GW, "claude", "o-gw", &|| false);
    assert_eq!(tick.await.unwrap(), TickOutcome::QueueEmpty);
    let stranded: i64 = get(&env.dir, "stranded").unwrap().parse().unwrap();
    assert_eq!(claim_state(&node.pool, stranded).await, unclaimed());
    assert_eq!(node.discord().seen(), Vec::<Seen>::new(), "no POST started");
    assert!(seen.samples > 50, "sampled {} times", seen.samples);
    assert_eq!(seen.opened, None, "opened after the force");
    eprintln!("[s3] target shut over {} samples", seen.samples);
    put(&env.dir, &format!("samples.{}", env.role), seen.samples);
    drop(registry);
}

/// T-H9 durable debt: force leaves the dead holder's spooled tail and open Prepared byte for byte,
/// and a target booted after it takes none of them. Recovery of each is H6-1's and H7-P's proof.
#[tokio::test(flavor = "current_thread")]
async fn th9_force_preserves_old_durable_debt_pg() {
    let scene = Scene::new().await;
    let epoch = scene.worker_home().await;
    put(scene.dir(), "epoch", epoch);
    let holder = reclaiming_with_owed_tail(&scene, epoch).await;
    holder.kill();
    Node::spawn(&scene, "th9_seed_prepared", MINI)
        .finish(WITHIN)
        .await;
    let store = scene
        .dir()
        .join(format!("root-{MINI}"))
        .join(STORE_DIR_NAME);
    let before = snapshot(&store);
    let channel = C.to_string();
    let spooled = |dir: &str, ext: &str| {
        let prefix = format!("{channel}/{dir}/");
        let named = before.keys().filter(|path| path.starts_with(&prefix));
        named.filter(|path| path.ends_with(ext)).count()
    };
    assert!(before.contains_key("o_era") && before.contains_key(&format!("{channel}/init")));
    assert_eq!(
        (spooled("spool", ".seg"), spooled("cursor", ".json")),
        (1, 1)
    );
    let ledger = std::fs::read_to_string(store.join(&channel).join("ledger.jsonl")).unwrap();
    let open = ledger_open_payloads(&ledger);
    assert_eq!(
        open,
        [PREPARED_ONLY],
        "the seed's Prepared is the one open piece"
    );
    eprintln!(
        "[s3] old root snapshot: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    let orphan = force_orphan(&scene, epoch).await;
    let stranded = seed_for(&scene.pool, C, GW, "6001", Some(orphan)).await;
    put(scene.dir(), "stranded", stranded);
    Node::spawn(&scene, "th9_new_target", GW)
        .finish(Duration::from_secs(120))
        .await;
    let samples = get(scene.dir(), &format!("samples.{GW}")).unwrap();
    eprintln!("[s3] new target shut over {samples} samples");
    assert_eq!(
        snapshot(&store),
        before,
        "the old root is byte for byte as the force found it"
    );
    let after = scene.row().await.unwrap();
    assert_eq!(
        (after.state, after.holder, after.epoch),
        (HomeState::Orphaned, None, orphan)
    );
    assert_eq!(claim_state(&scene.pool, stranded).await, unclaimed());
    scene.drop_db().await;
}

/// Payloads of the ledger's Prepared entries that no later outcome closed.
fn ledger_open_payloads(ledger: &str) -> Vec<String> {
    let mut open = BTreeMap::new();
    for line in ledger.lines() {
        let line: serde_json::Value = serde_json::from_str(line).unwrap();
        let entry = &line["entry"];
        let Some(serial) = entry["serial"].as_u64() else {
            continue;
        };
        match entry["type"].as_str().unwrap() {
            "prepared" => open.insert(serial, entry["payload"].as_str().unwrap().to_string()),
            _ => open.remove(&serial),
        };
    }
    open.into_values().collect()
}

/// Every file under `store` by relative path, with the sha256 of its bytes.
fn snapshot(store: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![store.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
            let relative = path
                .strip_prefix(store)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            files.insert(relative, hash);
        }
    }
    files
}

/// A process with no actor on the dead holder's root: it recovers the store and leaves one
/// Prepared for a unit no transcript row derives, without posting it.
pub(super) async fn seed_prepared_node(env: &Env) {
    let epoch = u64::try_from(epoch_of(env)).unwrap();
    let root = crate::config::runtime_root().expect("node root");
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
    let store = store.unwrap().expect("the holder's store");
    let era = store.read_era().unwrap().expect("the holder's era");
    let mut channel = store.open_channel(&era, C).unwrap().expect("the channel");
    let ledger = channel.ledger();
    assert!(
        ledger.unresolved().is_none(),
        "the holder left nothing prepared"
    );
    let unit_key = UnitKey {
        channel_id: C,
        provider: ShadowProvider::Claude,
        native_key: "h9-seed-only:0".into(),
        kind: UnitKind::Body,
    };
    let prepared = LedgerEntry::Prepared {
        serial: ledger.next_serial(),
        unit_key,
        piece_index: 0,
        payload: PREPARED_ONLY.into(),
        anchor_id: ledger.anchor(),
        epoch,
    };
    channel.append_ledger(prepared).unwrap();
    let (_, open) = channel
        .ledger()
        .unresolved()
        .expect("the seeded Prepared is open");
    assert_eq!(open.payload, PREPARED_ONLY);
}

/// A target booted after the force: its writer host opened its own store and waits on the Lost
/// gate, it holds the orphaned row shut and completes none of the old root's payloads.
pub(super) async fn new_target_node(env: &Env) {
    let node = Booted::boot(env).await;
    holds_orphan(&node, env, "6005", Observed::default()).await;
    assert!(
        node.host.bound(C),
        "the writer host never reached its binding log"
    );
    let writer = node.writers.first().expect("a writer host for the channel");
    assert!(!writer.is_finished(), "the writer host ended");
    assert!(!node.halted(), "{:?}", node.raised());
    let ledger = node
        .root
        .join(STORE_DIR_NAME)
        .join(C.to_string())
        .join("ledger.jsonl");
    // A Lost target need not have a ledger yet; any other read failure is not an empty one.
    let ledger = match std::fs::read_to_string(&ledger) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.unwrap(),
    };
    for payload in OLD_PAYLOADS {
        assert!(
            !ledger.contains(payload),
            "the target's ledger names {payload}"
        );
    }
    node.stop().await;
}
