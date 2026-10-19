//! A home's writer through the real watch, lease, drain and host over PG: who counts before a
//! leave, the join before a gate goes or renews, the late reservation, and when a home adopts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::*;
use super::events::{self, Event};
use super::{ADOPT_WAITS_WRITER, Authority};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
use crate::services::cluster::channel_home_port::ChannelHomePort;
use crate::services::cluster::home_supervisor::{HomeBundle, HomeLifecycle, Phase, StopReason};
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::store::{OStore, no_channel_store};
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::adoption::LegacyView;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents, BindingLog};
use crate::services::tui_o::writer::host::test_io::{Alarms, AnyLease, Posts, Startup, TestHost};
use crate::services::tui_o::writer::host::{Custody, FencedFacts, ManagedWriterHandle};
use crate::services::tui_prompt_dedupe::binding_events as p5;

/// A channel's binding view as the host reads it: the fixture's one bind, or this node's P5 log,
/// which is also what a never-held drain reads.
enum Log {
    Bound(Arc<Startup>),
    P5,
}

impl BindingEvents for Log {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        match self {
            Self::Bound(bound) => bound.binding_events_since(channel, after),
            Self::P5 => BindingLog.binding_events_since(channel, after),
        }
    }

    fn subscribe(&self, channel: u64) -> tokio::sync::watch::Receiver<u64> {
        match self {
            Self::Bound(bound) => bound.subscribe(channel),
            Self::P5 => BindingLog.subscribe(channel),
        }
    }
}

/// The gateway IO of the real host, with the late flag read live, facts that wait while parked,
/// and bindings from the P5 log when `p5`; the first bindings read may panic the host.
struct Io {
    host: Arc<TestHost>,
    p5: bool,
    late: AtomicBool,
    park: Mutex<Option<Arc<tokio::sync::Notify>>>,
    panic_once: AtomicBool,
}

impl Io {
    fn new(host: Arc<TestHost>, p5: bool) -> Arc<Self> {
        Arc::new(Self {
            host,
            p5,
            late: AtomicBool::new(false),
            park: Mutex::default(),
            panic_once: AtomicBool::new(false),
        })
    }

    /// First inits wait at their facts read until [`Io::unpark`].
    fn park(&self) {
        *self.park.lock().unwrap() = Some(Arc::new(tokio::sync::Notify::new()));
    }

    fn unpark(&self) {
        if let Some(park) = self.park.lock().unwrap().take() {
            park.notify_waiters();
        }
    }
}

impl HostIo for Io {
    type Port = Posts;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = Log;

    fn port(&self) -> impl Future<Output = Arc<Posts>> + Send {
        std::future::ready(Arc::clone(&self.host.posts))
    }

    fn lease(&self) -> AnyLease {
        AnyLease
    }

    fn alarms(&self) -> Alarms {
        self.host.alarms.clone()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Log> {
        if self.panic_once.swap(false, Ordering::SeqCst) {
            panic!("injected host panic");
        }
        if self.p5 {
            return Arc::new(Log::P5);
        }
        Arc::new(Log::Bound(self.host.bindings(channel, provider)))
    }

    fn late_adopt_enabled(&self) -> bool {
        self.late.load(Ordering::SeqCst)
    }

    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        let park = self.park.lock().unwrap().clone();
        let facts = self.host.activation_facts(channel, provider);
        async move {
            if let Some(park) = park {
                park.notified().await;
            }
            facts.await
        }
    }

    async fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<FencedFacts, String> {
        self.host.intake_fence(channel, provider).await
    }

    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.host.local_custody(channel, provider)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.host.legacy()
    }

    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.host.legacy_busy(channel)
    }

    fn relaying(&self, channel: u64) -> bool {
        self.host.relaying(channel)
    }
}

fn applied<T: std::fmt::Debug>(write: Result<HomeWrite<T>, HomeError>) -> T {
    match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("expected applied: {other:?}"),
    }
}

async fn eventually(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..secs * 50 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

/// Waits until the row as read satisfies `done`.
async fn row_until(
    pool: &PgPool,
    channel: &str,
    secs: u64,
    done: impl Fn(Option<&ChannelHome>) -> bool,
) -> Option<ChannelHome> {
    for _ in 0..secs * 50 {
        let row = o_channel_homes::read_home(pool, channel).await.unwrap();
        if done(row.as_ref()) {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let row = o_channel_homes::read_home(pool, channel).await.unwrap();
    assert!(done(row.as_ref()), "the row never got there: {row:?}");
    row
}

fn count(channel: u64, matches: impl Fn(&Event) -> bool) -> usize {
    events::of(channel)
        .iter()
        .filter(|event| matches(event))
        .count()
}

fn at(channel: u64, event: &Event) -> usize {
    let found = events::position(channel, |seen| seen == event);
    found.unwrap_or_else(|| panic!("{event:?} never happened: {:?}", events::of(channel)))
}

/// The first event of the channel naming a writer started under `gate`.
fn started_under(channel: u64, gate: usize) -> Option<(usize, u64)> {
    let events = events::of(channel);
    events
        .iter()
        .enumerate()
        .find_map(|(at, event)| match event {
            Event::Started(generation, authority) if *authority == gate => Some((at, *generation)),
            _ => None,
        })
}

/// Waits until the watch judged the released home twice, or once it found it adoptable; then the
/// row tells whether it was adopted.
async fn judged_twice(channel: u64) -> bool {
    let judged = || count(channel, |e| matches!(e, Event::Adoptable(_))) >= 2;
    let adoptable = || count(channel, |e| *e == Event::Adoptable(Ok(()))) >= 1;
    eventually(30, || judged() || adoptable()).await
}

fn gate_of(home: &Arc<HomeGate>) -> usize {
    gate_address(home)
}

/// A provider runtime of one node over a fresh writer root: its homes started from the rows, its
/// writers supervised, and its boot's first writers left for the test to claim and hand over.
struct Node {
    root: tempfile::TempDir,
    transcript: PathBuf,
    channel: u64,
    io: Arc<Io>,
    gate: Arc<OwnershipGate>,
    ready: Arc<Readiness>,
    writers: Arc<ChannelWriters>,
    restored: Arc<AtomicBool>,
    provider: ShadowProvider,
    delegated: bool,
    _p5: P5Root,
}

/// This thread's P5 root while it lives, for the host and the drain alike.
struct P5Root(Option<PathBuf>);

impl P5Root {
    fn install(root: &std::path::Path) -> Self {
        let saved = p5::test_root();
        p5::set_test_root(Some(root));
        Self(saved)
    }
}

impl Drop for P5Root {
    fn drop(&mut self) {
        p5::set_test_root(self.0.as_deref());
    }
}

struct Knobs {
    local: &'static str,
    delegated: bool,
    provider: ShadowProvider,
    p5: bool,
    reset: bool,
}

impl Knobs {
    fn gateway() -> Self {
        Self {
            local: "gw",
            delegated: false,
            provider: ShadowProvider::Claude,
            p5: false,
            reset: true,
        }
    }

    fn worker() -> Self {
        Self {
            local: "mini",
            delegated: true,
            ..Self::gateway()
        }
    }
}

impl Node {
    async fn boot(pool: &PgPool, channel: u64, knobs: Knobs) -> Self {
        let root = tempfile::tempdir().unwrap();
        let p5_root = P5Root::install(root.path());
        let transcript = root.path().join("o.jsonl");
        std::fs::write(&transcript, b"").unwrap();
        let source = source_id_for("s1", &transcript).unwrap();
        let io = Io::new(TestHost::new([(channel, source)]), knobs.p5);
        let (gate, ready) = (
            Arc::new(OwnershipGate::default()),
            Arc::<Readiness>::default(),
        );
        gate.acquired();
        let writers = ChannelWriters::new(Arc::clone(&gate), Arc::clone(&ready));
        let writers = Arc::new(match knobs.delegated {
            true => writers.delegated(),
            false => writers,
        });
        let restored = Arc::new(AtomicBool::new(true));
        let rows = o_channel_homes::list_homes(pool).await;
        let (reads_ready, reads_root) = (Arc::clone(&ready), root.path().to_path_buf());
        let reads_restored = Arc::clone(&restored);
        let reset_ok = knobs.reset;
        let reset: LegacyReset = Arc::new(move || {
            let answer = match reset_ok {
                true => Ok(()),
                false => Err(ResetRefused::Refused("held by the test".into())),
            };
            Box::pin(std::future::ready(answer))
        });
        let port = move |channel| {
            let restored = Arc::clone(&reads_restored);
            let reads =
                ChannelHomePort::new(channel, Arc::clone(&reads_ready), Arc::clone(&restored));
            let reads = reads.stored_under(Some(reads_root.clone()));
            BootPort::new(reads, Arc::clone(&reset)).restored_by(restored)
        };
        let boot = Boot {
            provider: provider_name(knobs.provider).into(),
            local: knobs.local.into(),
            pool: pool.clone(),
            rows: Box::pin(std::future::ready(rows)),
            candidates: vec![channel],
            port,
            writers: Some(Arc::clone(&writers)),
        };
        start(Some(true), || Some(boot)).await;
        Self {
            root,
            transcript,
            channel,
            io,
            gate,
            ready,
            writers,
            restored,
            provider: knobs.provider,
            delegated: knobs.delegated,
            _p5: p5_root,
        }
    }

    fn parts(&self) -> impl Fn() -> HostParts<Io> + Clone + Send + Sync + 'static {
        let (io, root) = (Arc::clone(&self.io), self.root.path().to_path_buf());
        let (gate, ready) = (Arc::clone(&self.gate), Arc::clone(&self.ready));
        move || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root.clone()),
            gate: Arc::clone(&gate),
            readiness: Arc::clone(&ready),
        }
    }

    /// What this runtime's boot hands its writer host: the gateway's own channels, or off the
    /// gateway the delegated channels registered here.
    fn owned(delegated: bool) -> host::OwnedChannels {
        match delegated {
            true => delegated_boot_ownership(|_| true),
            false => cutover::boot_ownership(),
        }
    }

    /// The boot's first writers, claimed and running but not yet handed to the supervisor.
    fn claim_boot(&self) -> Vec<ManagedWriterHandle> {
        let owned = Self::owned(self.delegated);
        host::start_managed(self.provider, !self.delegated, owned, self.parts())
    }

    /// Hands the boot's writers over and arms later starts, as the runtime's spawn does.
    fn hand_over(&self, started: Vec<ManagedWriterHandle>) {
        let (parts, delegated, provider) = (self.parts(), self.delegated, self.provider);
        let start: StartWriter = Arc::new(move |channel| {
            let owned = Self::owned(delegated).into_iter();
            let owned = owned.filter(|(owned, ..)| *owned == channel).collect();
            host::start_managed(provider, !delegated, owned, parts.clone()).pop()
        });
        self.writers.arm(Some(start), started);
    }

    fn first_spawn(&self) {
        self.hand_over(self.claim_boot());
    }

    fn post(&self, id: &str, text: &str) {
        use std::io::Write;
        let row = serde_json::json!({
            "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
            "message": {"id": id, "content": [{"type": "text", "text": text}]},
        });
        let mut line = serde_json::to_vec(&row).unwrap();
        line.push(b'\n');
        let mut options = std::fs::OpenOptions::new();
        let mut file = options.append(true).open(&self.transcript).unwrap();
        file.write_all(&line).unwrap();
    }

    fn posts(&self) -> Vec<String> {
        self.io.host.posts.to(self.channel)
    }

    fn halted(&self) -> bool {
        let raised = self.io.host.alarms.0.lock().unwrap();
        let halted = |(channel, alarm): &(u64, WriterAlarm)| {
            *channel == self.channel && matches!(alarm, WriterAlarm::Halted { .. })
        };
        raised.iter().any(halted)
    }

    fn generation(&self) -> Option<u64> {
        self.ready.hosted_by(self.channel)
    }

    /// What the channel's home shows it waits on in health, if anything.
    fn blocker(&self) -> Option<String> {
        let health = channel_home::health()?;
        let channel = self.channel.to_string();
        let draining = health.get("home_draining")?.as_array()?;
        let ours = draining
            .iter()
            .find(|entry| entry["channel"] == channel.as_str())?;
        Some(ours["blocker"].as_str()?.to_owned())
    }

    async fn end(self) {
        self.io.unpark();
        let _ = self.writers.stop_and_join(StopReason::Sigterm).await;
        channel_home::unregister(&self.channel.to_string());
        events::clear(self.channel);
    }
}

fn provider_name(provider: ShadowProvider) -> &'static str {
    match provider {
        ShadowProvider::Claude => "claude",
        ShadowProvider::Codex => "codex",
    }
}

/// The row gateway `gw` delegated to `mini`, drained and released, at its epoch.
async fn released(pool: &PgPool, channel: &str, provider: &str) -> ChannelHome {
    let row = applied(o_channel_homes::delegate(pool, channel, provider, "gw", "mini").await);
    applied(o_channel_homes::finish_release(pool, channel, "gw", row.epoch).await)
}

/// The row `gw` takes back from `mini`, drained and reclaimed but not yet removed.
async fn reclaimed(pool: &PgPool, channel: &str) {
    let row = released(pool, channel, "claude").await;
    let epoch = applied(o_channel_homes::adopt(pool, channel, "mini", row.epoch).await).epoch;
    applied(o_channel_homes::begin_reclaim(pool, channel, epoch, "gw").await);
    applied(o_channel_homes::finish_reclaim(pool, channel, "mini", epoch).await);
}

/// Binds `path` for the channel in this thread's P5 log, as a Claude hook would at session start.
fn bind_claude(channel: u64, path: &std::path::Path) {
    let hook =
        p5::HookSignal::from_payload("session_start", &serde_json::json!({"source": "startup"}));
    let proposal = p5::Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: "tmux-claude",
        session_id: Some("s1"),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Hook(hook.cause()),
        hook: Some(&hook),
    };
    assert_eq!(
        p5::record_source(&proposal).unwrap(),
        p5::Committed::Appended
    );
}

// ---- Leave: the boot's writer counts, and a leave joins the home's writer before its gate goes.

/// A Reclaimed boot's return waits until its boot handed its first writer over, though no slot
/// or claim exists yet; then it joins that writer, unmounts, and starts the gateway's own.
#[tokio::test]
async fn a_reclaimed_boot_returns_only_after_its_own_boot_writer_joined_pg() {
    const WC: &str = "1490141479707087101";
    const WID: u64 = 1_490_141_479_707_087_101;
    let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    reclaimed(&pool, WC).await;
    events::clear(WID);
    let gw = Node::boot(&pool, WID, Knobs::gateway()).await;
    let home = channel_home::registered(WC).expect("the reclaimed row mounts its home at boot");
    // The watch reaches the return before the boot claims its first writer.
    let reached = || count(WID, |e| matches!(e, Event::Leave(_) | Event::Unmount(_))) >= 1;
    assert!(eventually(20, reached).await, "the return was reached");
    assert_eq!(
        count(WID, |e| matches!(e, Event::Unmount(_))),
        0,
        "{:?}",
        events::of(WID)
    );
    assert!(
        count(WID, |e| *e == Event::Leave(false)) >= 1,
        "{:?}",
        events::of(WID)
    );
    assert!(
        o_channel_homes::read_home(&pool, WC)
            .await
            .unwrap()
            .is_some()
    );
    assert!(channel_home::registered(WC).is_some_and(|now| Arc::ptr_eq(&now, &home)));

    gw.first_spawn();
    assert!(
        eventually(30, || gw.ready.accepts(WID)).await,
        "the gateway's writer takes work: {:?}",
        events::of(WID)
    );
    assert_eq!(o_channel_homes::read_home(&pool, WC).await.unwrap(), None);
    let gateway = Arc::as_ptr(&gw.gate) as usize;
    let (boot_at, boot) = started_under(WID, gate_of(&home)).expect("the boot's writer");
    let joined = at(WID, &Event::Joined(boot));
    let unmount = at(WID, &Event::Unmount(gate_of(&home)));
    let (gateway_at, _) = started_under(WID, gateway).expect("the gateway's writer");
    assert!(
        boot_at < joined && joined < unmount && unmount < gateway_at,
        "{:?}",
        events::of(WID)
    );
    gw.post("r1", "returned");
    assert!(eventually(10, || gw.posts() == ["returned"]).await);
    gw.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A releasing gateway's row aborted under it: the home's writer is stopped and joined before
/// the gate goes, and only then does the gateway's own writer start.
#[tokio::test]
async fn an_aborted_release_joins_the_home_writer_before_unmount_pg() {
    const WC: &str = "1490141479707087102";
    const WID: u64 = 1_490_141_479_707_087_102;
    let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let row = applied(o_channel_homes::delegate(&pool, WC, "claude", "gw", "mini").await);
    events::clear(WID);
    let knobs = Knobs {
        reset: false,
        ..Knobs::gateway()
    };
    let gw = Node::boot(&pool, WID, knobs).await;
    let home = channel_home::registered(WC).expect("the releasing row mounts its home");
    gw.first_spawn();
    assert!(
        eventually(30, || gw.ready.accepts(WID)).await,
        "the home's writer runs: {:?}",
        events::of(WID)
    );
    let (_, under_home) = started_under(WID, gate_of(&home)).expect("the home's writer");
    applied(o_channel_homes::abort_release(&pool, WC, row.epoch, "gw").await);
    let gateway = Arc::as_ptr(&gw.gate) as usize;
    assert!(
        eventually(30, || started_under(WID, gateway).is_some()).await,
        "{:?}",
        events::of(WID)
    );
    let joined = at(WID, &Event::Joined(under_home));
    let unmount = at(WID, &Event::Unmount(gate_of(&home)));
    let (gateway_at, _) = started_under(WID, gateway).unwrap();
    assert!(
        joined < unmount && unmount < gateway_at,
        "{:?}",
        events::of(WID)
    );
    assert!(eventually(20, || gw.ready.accepts(WID)).await);
    gw.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A delegated home whose writer's end cannot be confirmed stays mounted however many rounds read
/// the row naming another node: the slot is blocked and no round reads that as no writer.
#[tokio::test]
async fn a_stuck_leave_keeps_the_home_mounted_across_rounds_pg() {
    const WC: &str = "1490141479707087103";
    const WID: u64 = 1_490_141_479_707_087_103;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let row = released(&pool, WC, "claude").await;
    let epoch = applied(o_channel_homes::adopt(&pool, WC, "mini", row.epoch).await).epoch;
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    let home = channel_home::registered(WC).expect("the worker row mounts its home");
    // Its boot writer's host panics at start, keeping its claim with no end to confirm.
    mini.io.panic_once.store(true, Ordering::SeqCst);
    mini.first_spawn();
    assert!(eventually(10, || mini.ready.hosted_by(WID).is_some()).await);
    sqlx::query("UPDATE o_channel_homes SET holder = 'other' WHERE channel_id = $1 AND epoch = $2")
        .bind(WC)
        .bind(epoch)
        .execute(&pool)
        .await
        .unwrap();
    let leaves = || count(WID, |e| matches!(e, Event::Leave(_)));
    assert!(
        eventually(30, || leaves() >= 2).await,
        "two leave rounds: {:?}",
        events::of(WID)
    );
    assert_eq!(
        count(WID, |e| matches!(e, Event::Unmount(_))),
        0,
        "{:?}",
        events::of(WID)
    );
    assert_eq!(
        count(WID, |e| *e == Event::Leave(false)),
        leaves(),
        "{:?}",
        events::of(WID)
    );
    assert!(matches!(mini.writers.phase(WID), Some(Phase::Blocked(..))));
    assert!(channel_home::registered(WC).is_some_and(|now| Arc::ptr_eq(&now, &home)));
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

// ---- Authority: a home renews only once the channel's writer is the home's.

/// A boot writer claimed under the gateway's authority but not yet handed over holds the first
/// renewal of a new home; handed over, it is replaced under the home's gate before it renews.
#[tokio::test]
async fn a_boot_writer_not_yet_supervised_holds_the_first_renewal_pg() {
    const WC: &str = "1490141479707087104";
    const WID: u64 = 1_490_141_479_707_087_104;
    let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let gw = Node::boot(&pool, WID, Knobs::gateway()).await;
    let started = gw.claim_boot();
    let boot = gw
        .generation()
        .expect("the boot's writer claims the channel");
    applied(o_channel_homes::delegate(&pool, WC, "claude", "gw", "mini").await);
    let judged = || count(WID, |e| matches!(e, Event::Authority(_)));
    assert!(
        eventually(20, || judged() >= 2).await,
        "two rounds judged the mounted home"
    );
    let home = channel_home::registered(WC).expect("mounted");
    assert_eq!(
        count(WID, |e| matches!(e, Event::Renew(..))),
        0,
        "{:?}",
        events::of(WID)
    );
    let pending = count(WID, |e| *e == Event::Authority(Authority::Pending));
    assert_eq!(pending, judged(), "{:?}", events::of(WID));
    assert!(matches!(
        home.ownership(),
        channel_home::HomeOwnership::Lost
    ));

    gw.hand_over(started);
    assert!(
        eventually(30, || count(WID, |e| matches!(e, Event::Renew(..))) >= 1).await,
        "{:?}",
        events::of(WID)
    );
    let (replaced_at, replacement) = started_under(WID, gate_of(&home)).expect("replaced");
    let joined = at(WID, &Event::Joined(boot));
    let renewed = events::position(
        WID,
        |e| matches!(e, Event::Renew(_, gate) if *gate == gate_of(&home)),
    );
    assert!(replacement > boot);
    assert!(
        joined < replaced_at && renewed.is_some_and(|renewed| replaced_at < renewed),
        "{:?}",
        events::of(WID)
    );
    gw.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A worker booting as a held row's holder hosts one writer: the watch neither starts another nor
/// replaces the boot's, round after round.
#[tokio::test]
async fn a_worker_holder_boot_hosts_one_writer_pg() {
    const WC: &str = "1490141479707087105";
    const WID: u64 = 1_490_141_479_707_087_105;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let row = released(&pool, WC, "claude").await;
    applied(o_channel_homes::adopt(&pool, WC, "mini", row.epoch).await);
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    mini.first_spawn();
    let generation = mini.generation().expect("the boot's writer");
    let before = rounds(WID);
    assert!(eventually(30, || rounds(WID) >= before + 3).await);
    assert_eq!(mini.generation(), Some(generation));
    assert_eq!(
        count(WID, |e| matches!(e, Event::Started(..))),
        1,
        "{:?}",
        events::of(WID)
    );
    assert!(!mini.halted());
    assert!(eventually(20, || mini.ready.accepts(WID)).await);
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

// ---- Same-process redelegation after a reclaim.

/// The channel delegated to `mini` again after its reclaim: the old writer is joined and its claim
/// gone before the home leaves, and the next delegation's writer starts under the new home.
async fn redelegate(node: &Node, pool: &PgPool, wc: &str, provider: &str) -> Arc<HomeGate> {
    let wid = node.channel;
    let first = row_until(pool, wc, 30, |row| {
        row.is_some_and(|row| row.state == HomeState::Worker)
    })
    .await
    .unwrap();
    let old = channel_home::registered(wc).expect("the first home");
    let generation = node.generation().expect("its writer");
    applied(o_channel_homes::begin_reclaim(pool, wc, first.epoch, "gw").await);
    let left = row_until(pool, wc, 60, |row| {
        row.is_some_and(|row| row.state == HomeState::Reclaimed)
    })
    .await
    .unwrap();
    assert!(
        eventually(30, || channel_home::registered(wc).is_none()).await,
        "{:?}",
        events::of(wid)
    );
    let joined = at(wid, &Event::Joined(generation));
    let unmount = at(wid, &Event::Unmount(gate_of(&old)));
    assert!(joined < unmount, "{:?}", events::of(wid));
    assert_eq!(node.generation(), None, "the old claim is gone");
    assert_eq!(node.writers.phase(wid), None);
    assert!(
        node.ready.transition(wid).is_none(),
        "the old run's live port is gone"
    );
    applied(o_channel_homes::remove_reclaimed(pool, wc, "gw", left.epoch).await);
    released(pool, wc, provider).await;
    let row = row_until(pool, wc, 60, |row| {
        row.is_some_and(|row| row.state == HomeState::Worker)
    })
    .await
    .unwrap();
    let new = channel_home::registered(wc).expect("the next home");
    assert!(
        !Arc::ptr_eq(&new, &old),
        "a new home, compared while the old one is still held"
    );
    let (_, next) = started_under(wid, gate_of(&new)).expect("a writer under the new home");
    assert!(next > generation);
    assert!(
        eventually(30, || matches!(new.ownership(), channel_home::HomeOwnership::Owned { home_epoch, .. } if home_epoch == row.epoch)).await
    );
    new
}

/// A delegated worker whose actor ran is reclaimed, then delegated again in the same process: the
/// next writer takes work under the new home and posts.
#[tokio::test]
async fn a_reclaim_then_redelegation_takes_work_again_pg() {
    const WC: &str = "1490141479707087106";
    const WID: u64 = 1_490_141_479_707_087_106;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    assert!(
        eventually(40, || mini.ready.accepts(WID)).await,
        "{:?}",
        events::of(WID)
    );
    mini.post("m1", "reply-1");
    assert!(eventually(10, || mini.posts() == ["reply-1"]).await);
    let _new = redelegate(&mini, &pool, WC, "claude").await;
    assert!(
        eventually(30, || mini.ready.accepts(WID)).await,
        "{:?}",
        events::of(WID)
    );
    mini.post("m2", "reply-2");
    assert!(eventually(10, || mini.posts() == ["reply-1", "reply-2"]).await);
    assert!(!mini.halted());
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A pending Codex channel with no bound source, reclaimed before its first init, drains off the
/// P5 log its precheck read; the next delegation's writer commits an empty init and posts.
#[tokio::test]
async fn a_fresh_codex_reclaim_then_redelegation_takes_work_pg() {
    const WC: &str = "1490141479707087107";
    const WID: u64 = 1_490_141_479_707_087_107;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, CodexTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let knobs = Knobs {
        provider: ShadowProvider::Codex,
        p5: true,
        ..Knobs::worker()
    };
    let mini = Node::boot(&pool, WID, knobs).await;
    let seen = mini
        .io
        .bindings(WID, ShadowProvider::Codex)
        .binding_events_since(WID, 0);
    assert_eq!(
        seen,
        BindingLog.binding_events_since(WID, 0),
        "the precheck reads the drain's log"
    );
    assert_eq!(seen, Ok(Vec::new()));
    mini.io.park();
    mini.first_spawn();
    released(&pool, WC, "codex").await;
    let _new = redelegate(&mini, &pool, WC, "codex").await;
    assert!(
        no_channel_store(mini.root.path(), WID),
        "the first writer never activated"
    );
    assert!(!mini.halted());
    mini.io.unpark();
    assert!(
        eventually(30, || mini.ready.accepts(WID)).await,
        "{:?}",
        events::of(WID)
    );
    let store = OStore::existing(mini.root.path()).expect("a store");
    let init = store.read_init(WID).unwrap().expect("an init");
    assert!(init.sources.is_empty(), "{:?}", init.sources);
    assert!(
        eventually(10, || store.peek_binding_checkpoint(WID).ok()
            == Some(Some(0)))
        .await,
        "the binding checkpoint a Herdr turn needs"
    );
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A pending Claude channel with a source bound, reclaimed before its first init: the drain reads
/// that bind and waits, owed unknown, until the first init resumes, then leaves normally.
#[tokio::test]
async fn a_bound_claude_reclaim_before_its_first_init_waits_for_it_pg() {
    const WC: &str = "1490141479707087108";
    const WID: u64 = 1_490_141_479_707_087_108;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let knobs = Knobs {
        p5: true,
        ..Knobs::worker()
    };
    let mini = Node::boot(&pool, WID, knobs).await;
    bind_claude(WID, &mini.transcript);
    let seen = mini
        .io
        .bindings(WID, ShadowProvider::Claude)
        .binding_events_since(WID, 0);
    assert_eq!(
        seen,
        BindingLog.binding_events_since(WID, 0),
        "the precheck reads the drain's log"
    );
    assert_eq!(seen.unwrap().len(), 1);
    mini.io.park();
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    let worker = row_until(&pool, WC, 40, |row| {
        row.is_some_and(|row| row.state == HomeState::Worker)
    })
    .await
    .unwrap();
    applied(o_channel_homes::begin_reclaim(&pool, WC, worker.epoch, "gw").await);
    let before = rounds(WID);
    assert!(eventually(30, || rounds(WID) >= before + 3).await);
    let row = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.state,
        HomeState::Reclaiming,
        "a bound channel is not left as never held"
    );
    assert_eq!(mini.blocker().as_deref(), Some("owed_unreadable"));
    mini.io.unpark();
    row_until(&pool, WC, 60, |row| {
        row.is_some_and(|row| row.state == HomeState::Reclaimed)
    })
    .await;
    assert!(eventually(30, || channel_home::registered(WC).is_none()).await);
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A watch that misses the leave's aftermath (the row removed and delegated again before its next
/// read) keeps its home and writer: the home confirms the new epoch and the writer takes work.
#[tokio::test]
async fn an_unobserved_leave_reuses_the_home_and_its_writer_pg() {
    const WC: &str = "1490141479707087109";
    const WID: u64 = 1_490_141_479_707_087_109;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    assert!(
        eventually(40, || mini.ready.accepts(WID)).await,
        "{:?}",
        events::of(WID)
    );
    mini.post("m1", "reply-1");
    assert!(eventually(10, || mini.posts() == ["reply-1"]).await);
    let home = channel_home::registered(WC).unwrap();
    let generation = mini.generation().unwrap();
    let worker = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    let park = events::park(WID);
    applied(o_channel_homes::begin_reclaim(&pool, WC, worker.epoch, "gw").await);
    let left = row_until(&pool, WC, 60, |row| {
        row.is_some_and(|row| row.state == HomeState::Reclaimed)
    })
    .await
    .unwrap();
    assert!(eventually(30, || count(WID, |e| *e == Event::Parked) >= 1).await);
    applied(o_channel_homes::remove_reclaimed(&pool, WC, "gw", left.epoch).await);
    released(&pool, WC, "claude").await;
    park.release();
    let row = row_until(&pool, WC, 40, |row| {
        row.is_some_and(|row| row.state == HomeState::Worker)
    })
    .await
    .unwrap();
    assert!(
        eventually(30, || matches!(home.ownership(), channel_home::HomeOwnership::Owned { home_epoch, .. } if home_epoch == row.epoch)).await,
        "the same home confirms the new epoch"
    );
    assert_eq!(
        count(WID, |e| matches!(e, Event::Unmount(_))),
        0,
        "{:?}",
        events::of(WID)
    );
    assert!(channel_home::registered(WC).is_some_and(|now| Arc::ptr_eq(&now, &home)));
    assert_eq!(mini.generation(), Some(generation));
    assert!(eventually(30, || mini.ready.accepts(WID)).await);
    mini.post("m2", "reply-2");
    assert!(eventually(10, || mini.posts() == ["reply-1", "reply-2"]).await);
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

// ---- Late reservation: a stop's reason keeps or settles it.

/// A supervised stop keeps a late reservation for the next host when it is a replacement or a
/// delegated home's leave; any other stop, or a host that ends on its own, settles it.
#[tokio::test]
async fn a_stop_keeps_or_settles_the_late_reservation_by_its_reason() {
    const WID: u64 = 1_490_141_479_707_087_110;
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let cases = [
        (StopReason::Replaced, false, Adoption::Deferred),
        (StopReason::Replaced, true, Adoption::Deferred),
        (StopReason::RowGone, true, Adoption::Deferred),
        (StopReason::RowGone, false, Adoption::Released),
        (StopReason::Sigterm, true, Adoption::Released),
        (StopReason::BackendExited, false, Adoption::Released),
        (StopReason::CommittedRestart, true, Adoption::Released),
    ];
    for (reason, delegated, left) in cases {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("o.jsonl");
        std::fs::write(&transcript, b"").unwrap();
        let io = Io::new(
            TestHost::new([(WID, source_id_for("s1", &transcript).unwrap())]),
            false,
        );
        let candidate = Candidate::reserved_late();
        let gate = Arc::new(OwnershipGate::default());
        let parts = HostParts {
            io,
            runtime_root: Some(root.path().to_path_buf()),
            gate,
            readiness: Arc::default(),
        };
        let owned = vec![(WID, Some(ClaudeTui), Some(candidate.clone()))];
        let mut started = host::start_managed(ShadowProvider::Claude, true, owned, || parts);
        let handle = started.pop().expect("a late host");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handle.live());
        let settled = WriterBundle::new(handle, delegated)
            .stop_and_join(reason)
            .await;
        assert_eq!(
            settled,
            crate::services::cluster::home_supervisor::Settled::Joined
        );
        assert_eq!(candidate.peek(), left, "{reason:?} delegated={delegated}");
    }
    // A host that ends on its own never kept it.
    let candidate = Candidate::reserved_late();
    drop(candidate.late_cleanup_guard(WID, None).unwrap());
    assert_eq!(candidate.peek(), Adoption::Released);
    events::clear(WID);
}

// ---- Adoption: a released home's writer must run and could run its first init.

/// A released target adopts only once its turns are restored, and then only with its writer: a
/// boot that starts no writer for the channel leaves the row released and says why.
#[tokio::test]
async fn a_released_target_adopts_only_restored_and_with_a_live_writer_pg() {
    const WC: &str = "1490141479707087111";
    const WID: u64 = 1_490_141_479_707_087_111;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    mini.restored.store(false, Ordering::SeqCst);
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    let before = rounds(WID);
    assert!(eventually(30, || rounds(WID) >= before + 2).await);
    let row = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HomeState::Released, "unrestored, nothing adopts");
    assert_eq!(
        count(WID, |e| matches!(e, Event::Adoptable(_))),
        0,
        "{:?}",
        events::of(WID)
    );
    mini.restored.store(true, Ordering::SeqCst);
    row_until(&pool, WC, 30, |row| {
        row.is_some_and(|row| row.state == HomeState::Worker)
    })
    .await;
    assert!(count(WID, |e| *e == Event::Adoptable(Ok(()))) >= 1);
    assert!(eventually(30, || mini.ready.accepts(WID)).await);
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// No writer can start for a released target (its boot holds no candidate for the channel): the
/// row stays released and the home says it waits on its writer.
#[tokio::test]
async fn a_released_target_without_a_writer_stays_released_pg() {
    const WC: &str = "1490141479707087112";
    const WID: u64 = 1_490_141_479_707_087_112;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _foreign = test_override::force_foreign(&[(WID, ClaudeTui)], "gw");
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    assert!(judged_twice(WID).await, "{:?}", events::of(WID));
    let row = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HomeState::Released, "{:?}", events::of(WID));
    let waits = Event::Adoptable(Err(ADOPT_WAITS_WRITER));
    assert!(count(WID, |e| *e == waits) >= 2, "{:?}", events::of(WID));
    assert_eq!(mini.blocker().as_deref(), Some(ADOPT_WAITS_WRITER));
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A released target whose writer runs but whose adoption Legacy already took stays released.
#[tokio::test]
async fn a_released_target_whose_candidate_was_released_stays_released_pg() {
    const WC: &str = "1490141479707087113";
    const WID: u64 = 1_490_141_479_707_087_113;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let mini = Node::boot(&pool, WID, Knobs::worker()).await;
    let candidate = cutover::boot_ownership()
        .into_iter()
        .find(|(c, ..)| *c == WID)
        .unwrap()
        .2
        .unwrap();
    candidate.release(WID);
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    assert!(judged_twice(WID).await, "{:?}", events::of(WID));
    let row = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HomeState::Released, "{:?}", events::of(WID));
    let refused = Event::Adoptable(Err("adopt_refused:released"));
    assert!(count(WID, |e| *e == refused) >= 2, "{:?}", events::of(WID));
    assert!(mini.generation().is_some(), "its writer runs");
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A pending Claude channel whose P5 log binds no source could not commit its first init, so its
/// released home is not adopted; once a source is bound it is, and the writer takes work.
#[tokio::test]
async fn an_unbound_claude_target_adopts_only_once_a_source_is_bound_pg() {
    const WC: &str = "1490141479707087114";
    const WID: u64 = 1_490_141_479_707_087_114;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let knobs = Knobs {
        p5: true,
        ..Knobs::worker()
    };
    let mini = Node::boot(&pool, WID, knobs).await;
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    assert!(judged_twice(WID).await, "{:?}", events::of(WID));
    let row = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HomeState::Released, "{:?}", events::of(WID));
    let refused = Event::Adoptable(Err("adopt_refused:no_bound_source"));
    assert!(count(WID, |e| *e == refused) >= 2, "{:?}", events::of(WID));
    bind_claude(WID, &mini.transcript);
    row_until(&pool, WC, 30, |row| {
        row.is_some_and(|row| row.state == HomeState::Worker)
    })
    .await;
    assert!(
        eventually(30, || mini.ready.accepts(WID)).await,
        "{:?}",
        events::of(WID)
    );
    mini.post("m1", "bound");
    assert!(eventually(10, || mini.posts() == ["bound"]).await);
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// A writer whose host ended abnormally keeps its claim but runs nothing: its released home waits.
#[tokio::test]
async fn a_released_target_whose_host_died_with_its_claim_stays_released_pg() {
    const WC: &str = "1490141479707087115";
    const WID: u64 = 1_490_141_479_707_087_115;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let knobs = Knobs {
        p5: true,
        ..Knobs::worker()
    };
    let mini = Node::boot(&pool, WID, knobs).await;
    bind_claude(WID, &mini.transcript);
    mini.io.panic_once.store(true, Ordering::SeqCst);
    mini.first_spawn();
    released(&pool, WC, "claude").await;
    assert!(judged_twice(WID).await, "{:?}", events::of(WID));
    let row = o_channel_homes::read_home(&pool, WC)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HomeState::Released, "{:?}", events::of(WID));
    let waits = Event::Adoptable(Err(ADOPT_WAITS_WRITER));
    assert!(count(WID, |e| *e == waits) >= 2, "{:?}", events::of(WID));
    assert!(mini.generation().is_some(), "the dead host kept its claim");
    mini.end().await;
    pool.close().await;
    pg_db.drop().await;
}

/// Late-reserved Claude channels over the production boot seed: late adoption off now keeps the
/// released home waiting with its source bound; on again, it adopts and its writer takes work.
mod late {
    use super::*;
    use crate::services::tui_o::channel_policy::{self, BootChannels};
    use crate::services::tui_o::writer::adoption::LegacyCursor;

    fn seed(root: &std::path::Path, channel: u64) -> test_override::ChannelsGuard {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "server": {},
            "tui_o": {"writer": {"channels": [channel], "late_adopt": true}},
            "agents": [{"id": "late", "name": "Late", "channels": {
                "claude": {"id": channel.to_string(), "runtime": "tui"}
            }}]
        }))
        .unwrap();
        let snapshot = BootChannels::validate(&config)
            .unwrap()
            .seeded(true, &config, |channels, _| {
                Ok(channel_policy::stored(Some(root), channels))
            })
            .unwrap();
        test_override::force_boot(snapshot)
    }

    /// Legacy's relay with its cursor at the end of the bound transcript, all of it delivered.
    struct Legacy(PathBuf);

    impl LegacyView for Legacy {
        fn started(&self) -> bool {
            true
        }

        fn cursor(&self, _: &str) -> LegacyCursor {
            let offset = std::fs::metadata(&self.0).map_or(0, |meta| meta.len());
            LegacyCursor::Bound {
                path: self.0.clone(),
                offset,
            }
        }

        fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
            Ok(self.cursor(tmux))
        }

        fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
            std::fs::metadata(&self.0).ok().map(|meta| meta.len())
        }

        fn tail_running(&self, _: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn a_late_target_waits_while_late_adoption_is_off_pg() {
        const WC: &str = "1490141479707087116";
        const WID: u64 = 1_490_141_479_707_087_116;
        let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        events::clear(WID);
        let seed_root = tempfile::tempdir().unwrap();
        let _seeded = seed(seed_root.path(), WID);
        let reserved = cutover::boot_ownership()
            .into_iter()
            .find(|(c, ..)| *c == WID);
        assert!(
            reserved
                .and_then(|(.., c)| c)
                .is_some_and(|c| c.late_adopt_origin())
        );
        let knobs = Knobs {
            p5: true,
            ..Knobs::worker()
        };
        let mini = Node::boot(&pool, WID, knobs).await;
        bind_claude(WID, &mini.transcript);
        let legacy = Arc::new(Legacy(mini.transcript.clone()));
        *mini.io.host.legacy.lock().unwrap() = Some(legacy);
        mini.first_spawn();
        released(&pool, WC, "claude").await;
        assert!(judged_twice(WID).await, "{:?}", events::of(WID));
        let row = o_channel_homes::read_home(&pool, WC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, HomeState::Released, "{:?}", events::of(WID));
        let refused = Event::Adoptable(Err("adopt_refused:late_disabled"));
        assert!(count(WID, |e| *e == refused) >= 2, "{:?}", events::of(WID));
        assert!(mini.generation().is_some());
        mini.io.late.store(true, Ordering::SeqCst);
        row_until(&pool, WC, 30, |row| {
            row.is_some_and(|row| row.state == HomeState::Worker)
        })
        .await;
        assert!(
            eventually(60, || mini.ready.accepts(WID)).await,
            "{:?}",
            mini.io.host.alarms.0.lock().unwrap()
        );
        mini.post("m1", "late");
        assert!(eventually(10, || mini.posts() == ["late"]).await);
        mini.end().await;
        pool.close().await;
        pg_db.drop().await;
    }

    /// A late reservation with no source bound yet waits; bound, the next round adopts it.
    #[tokio::test]
    async fn a_late_target_without_a_bound_source_waits_for_one_pg() {
        const WC: &str = "1490141479707087117";
        const WID: u64 = 1_490_141_479_707_087_117;
        let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(WID, "mini")]);
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        events::clear(WID);
        let seed_root = tempfile::tempdir().unwrap();
        let _seeded = seed(seed_root.path(), WID);
        let knobs = Knobs {
            p5: true,
            ..Knobs::worker()
        };
        let mini = Node::boot(&pool, WID, knobs).await;
        mini.io.late.store(true, Ordering::SeqCst);
        mini.first_spawn();
        released(&pool, WC, "claude").await;
        assert!(judged_twice(WID).await, "{:?}", events::of(WID));
        let row = o_channel_homes::read_home(&pool, WC)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, HomeState::Released, "{:?}", events::of(WID));
        let refused = Event::Adoptable(Err("adopt_refused:late_awaits_binding"));
        assert!(count(WID, |e| *e == refused) >= 2, "{:?}", events::of(WID));
        bind_claude(WID, &mini.transcript);
        row_until(&pool, WC, 30, |row| {
            row.is_some_and(|row| row.state == HomeState::Worker)
        })
        .await;
        mini.end().await;
        pool.close().await;
        pg_db.drop().await;
    }
}

// ---- The precheck alone, through the real host's handle.

/// The first-init precheck refuses what the first init would (output in a Codex source, an
/// unreadable log, an unbound late Codex, late adoption off) and passes an unbound pending Codex.
#[tokio::test]
async fn the_precheck_refuses_what_the_first_init_would() {
    const WID: u64 = 1_490_141_479_707_087_118;
    let root = tempfile::tempdir().unwrap();
    let _p5 = P5Root::install(root.path());
    let check = |provider, candidate: Candidate, late: bool| {
        let transcript = root.path().join("o.jsonl");
        std::fs::write(&transcript, b"").unwrap();
        let io = Io::new(
            TestHost::new([(WID, source_id_for("s1", &transcript).unwrap())]),
            true,
        );
        io.late.store(late, Ordering::SeqCst);
        let parts = HostParts {
            io,
            runtime_root: Some(root.path().to_path_buf()),
            gate: Arc::new(OwnershipGate::default()),
            readiness: Arc::default(),
        };
        let kind = match provider {
            ShadowProvider::Claude => ClaudeTui,
            ShadowProvider::Codex => CodexTui,
        };
        let owned = vec![(WID, Some(kind), Some(candidate))];
        let handle = host::start_managed(provider, true, owned, || parts)
            .pop()
            .unwrap();
        let ready = handle.ready()();
        drop(handle.stop_and_join());
        ready
    };
    let pending = || Candidate::new(Adoption::Pending);
    assert_eq!(
        check(ShadowProvider::Codex, pending(), false),
        Ok(()),
        "unbound pending Codex"
    );
    assert_eq!(
        check(ShadowProvider::Codex, Candidate::reserved_late(), true),
        Err("adopt_refused:late_awaits_turn")
    );
    assert_eq!(
        check(ShadowProvider::Claude, Candidate::reserved_late(), false),
        Err("adopt_refused:late_disabled")
    );
    // A Codex rollout that already holds an answer is more than metadata.
    let rollout = root.path().join("rollout.jsonl");
    let answer = serde_json::json!({"type": "response_item", "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": {"type": "message", "role": "assistant", "id": "m", "content": [{"type": "output_text", "text": "x"}]}});
    let mut bytes = serde_json::to_vec(&answer).unwrap();
    bytes.push(b'\n');
    std::fs::write(&rollout, bytes).unwrap();
    bind_codex(WID, &rollout);
    assert_eq!(
        check(ShadowProvider::Codex, pending(), false),
        Err("adopt_refused:source_not_metadata")
    );
    // A corrupt P5 log is no shorter list.
    let log = root
        .path()
        .join(p5::BINDING_EVENTS_DIR)
        .join(format!("{WID}.log"));
    std::fs::write(&log, b"not json\n").unwrap();
    p5::forget_channel_for_tests(WID);
    assert!(BindingLog.binding_events_since(WID, 0).is_err());
    assert_eq!(
        check(ShadowProvider::Codex, pending(), false),
        Err("adopt_refused:binding_unreadable")
    );
    assert_eq!(
        check(ShadowProvider::Claude, pending(), false),
        Err("adopt_refused:binding_unreadable")
    );
}

fn bind_codex(channel: u64, path: &std::path::Path) {
    let hook =
        p5::HookSignal::from_payload("session_start", &serde_json::json!({"source": "startup"}));
    let proposal = p5::Proposal {
        channel_id: channel,
        provider: "codex",
        tmux_session: "tmux-codex",
        session_id: Some("s-codex"),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Hook(hook.cause()),
        hook: Some(&hook),
    };
    assert_eq!(
        p5::record_source(&proposal).unwrap(),
        p5::Committed::Appended
    );
}

// ---- The fence at the first init's admission.

/// A home closed after the first init read its facts Owned, just before it asks for admission,
/// admits nothing: no init is written and the adoption stays pending.
#[tokio::test]
async fn a_home_closed_after_its_facts_were_read_owned_admits_no_first_init() {
    use crate::services::tui_o::writer::activation::test_hook::{self, Step};
    const WID: u64 = 1_490_141_479_707_087_119;
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("o.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let io = Io::new(
        TestHost::new([(WID, source_id_for("s1", &transcript).unwrap())]),
        false,
    );
    let home = channel_home::register_for_test(WID, Some(HomeState::Worker));
    let closing = Arc::clone(&home);
    test_hook::set(WID, Step::BeforeAdmit, move || {
        closing.close();
        Ok(())
    });
    let wrote = Arc::new(AtomicBool::new(false));
    let write = Arc::clone(&wrote);
    test_hook::set(WID, Step::AfterWrite, move || {
        write.store(true, Ordering::SeqCst);
        Ok(())
    });
    let parts = HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root.path().to_path_buf()),
        gate: Arc::new(OwnershipGate::default()),
        readiness: Arc::default(),
    };
    let owned = cutover::boot_ownership();
    let candidate = owned
        .iter()
        .find(|(c, ..)| *c == WID)
        .and_then(|(.., c)| c.clone())
        .unwrap();
    let handle = host::start_managed(ShadowProvider::Claude, false, owned, || parts)
        .pop()
        .unwrap();
    assert_eq!(
        handle.authority(),
        gate_of(&home),
        "the host took the home's gate"
    );
    let settled = || test_hook::refused(WID) >= 1 || wrote.load(Ordering::SeqCst);
    assert!(
        eventually(10, settled).await,
        "the first init reached its admission"
    );
    assert!(
        !wrote.load(Ordering::SeqCst),
        "nothing was written past the close"
    );
    assert!(test_hook::refused(WID) >= 1, "the admission was refused");
    assert!(no_channel_store(root.path(), WID));
    assert_eq!(candidate.peek(), Adoption::Pending);
    let halted = io
        .host
        .alarms
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|(_, alarm)| matches!(alarm, WriterAlarm::Halted { .. }));
    assert!(!halted);
    let _ = handle.stop_and_join().await;
    channel_home::unregister(&WID.to_string());
}

// ---- A gateway writer mid-admission when its channel's home mounts, on its own thread.

/// A gateway writer mid-admission when its channel is delegated, on its own thread over the same
/// registry and P5 root, is joined before one under the home's gate starts and the home renews.
#[tokio::test]
async fn a_gateway_writer_mid_admission_is_joined_before_its_home_renews_pg() {
    use crate::services::cluster::channel_home::tests::shared_homes;
    use crate::services::tui_o::writer::activation::test_hook::{self, Step};
    const WC: &str = "1490141479707087120";
    const WID: u64 = 1_490_141_479_707_087_120;
    let _hosts = crate::config::session_hosts::force_for_test(Some("gw"), &[(WID, "gw")]);
    let _selected = test_override::force_candidates(&[(WID, ClaudeTui)]);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    events::clear(WID);
    let gw = Node::boot(&pool, WID, Knobs::gateway()).await;

    // The writer's own thread, over this thread's registry, P5 root and candidates.
    let (homes, channels, p5_root) = (
        shared_homes(),
        test_override::shared_channels(),
        p5::test_root(),
    );
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let (end_tx, end_rx) = tokio::sync::oneshot::channel::<()>();
    let writer_thread = std::thread::spawn(move || {
        let _homes = homes();
        let _channels = channels();
        p5::set_test_root(p5_root.as_deref());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        handle_tx.send(runtime.handle().clone()).unwrap();
        runtime.block_on(async move {
            let _ = end_rx.await;
        });
    });
    let writer_runtime = handle_rx.recv().unwrap();

    // Inside its admission it waits, before its write, for the home to be mounted.
    let (at_lock_tx, at_lock) = (Arc::new(AtomicBool::new(false)), ());
    let _ = at_lock;
    let reached = Arc::clone(&at_lock_tx);
    test_hook::set(WID, Step::BeforeLock, move || {
        reached.store(true, Ordering::SeqCst);
        Ok(())
    });
    let (release_tx, release_rx) = std::sync::mpsc::channel::<usize>();
    let saw_home = Arc::new(Mutex::new(None::<bool>));
    let saw = Arc::clone(&saw_home);
    test_hook::set(WID, Step::BeforeWrite, move || {
        let expected = release_rx.recv().unwrap();
        let seen = channel_home::registered_channel(WID).map(|home| gate_address(&home));
        *saw.lock().unwrap() = Some(seen == Some(expected));
        Ok(())
    });
    let open_at_write = Arc::new(Mutex::new(None::<bool>));
    let open = Arc::clone(&open_at_write);
    test_hook::set(WID, Step::AfterWrite, move || {
        let home = channel_home::registered_channel(WID);
        *open.lock().unwrap() = home.map(|home| !home.withdrawn());
        events::record(WID, Event::Mark("old_write"));
        Ok(())
    });
    let started = {
        let _on = writer_runtime.enter();
        gw.claim_boot()
    };
    let old = gw
        .generation()
        .expect("the boot's writer claims the channel");
    gw.hand_over(started);
    assert!(
        eventually(10, || at_lock_tx.load(Ordering::SeqCst)).await,
        "the old writer is inside its admission"
    );
    assert_eq!(
        count(WID, |e| matches!(e, Event::Mount(_))),
        0,
        "no home yet"
    );

    applied(o_channel_homes::delegate(&pool, WC, "claude", "gw", "mini").await);
    assert!(eventually(20, || channel_home::registered(WC).is_some()).await);
    let home = channel_home::registered(WC).unwrap();
    let replacing = || matches!(gw.writers.phase(WID), Some(Phase::Starting(_)));
    let renewed = || count(WID, |e| matches!(e, Event::Renew(..))) >= 1;
    assert!(
        eventually(20, || replacing() || renewed()).await,
        "the mounted home was judged"
    );
    assert_eq!(
        count(WID, |e| matches!(e, Event::Renew(..))),
        0,
        "{:?}",
        events::of(WID)
    );
    assert!(
        replacing(),
        "the replacement waits on the old writer: {:?}",
        events::of(WID)
    );
    assert!(matches!(
        home.ownership(),
        channel_home::HomeOwnership::Lost
    ));
    release_tx.send(gate_of(&home)).unwrap();

    assert!(
        eventually(30, || count(WID, |e| matches!(e, Event::Renew(..))) >= 1).await,
        "{:?}",
        events::of(WID)
    );
    assert_eq!(
        *saw_home.lock().unwrap(),
        Some(true),
        "the writer's thread saw the same home"
    );
    assert_eq!(
        *open_at_write.lock().unwrap(),
        Some(true),
        "the home was not closed under the old write"
    );
    let mount = at(WID, &Event::Mount(gate_of(&home)));
    let write = at(WID, &Event::Mark("old_write"));
    let joined = at(WID, &Event::Joined(old));
    let (replaced, _) = started_under(WID, gate_of(&home)).expect("a writer under the home");
    let renewed = events::position(
        WID,
        |e| matches!(e, Event::Renew(_, gate) if *gate == gate_of(&home)),
    )
    .expect("renewed");
    assert!(
        mount < write && write < joined && joined < replaced && replaced < renewed,
        "{:?}",
        events::of(WID)
    );
    assert_eq!(count(WID, |e| *e == Event::Mark("old_write")), 1);
    assert_eq!(count(WID, |e| matches!(e, Event::Joined(_))), 1);
    gw.end().await;
    let _ = end_tx.send(());
    writer_thread.join().unwrap();
    pool.close().await;
    pg_db.drop().await;
}
