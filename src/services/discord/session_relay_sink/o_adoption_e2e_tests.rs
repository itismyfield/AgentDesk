//! A candidate channel against its real Legacy sink and the real writer host: whichever of the
//! first Legacy body and the first `init` comes first owns the channel, and each unit posts once.

use super::*;
use crate::services::tui_o::channel_policy::{self, Adoption, BootChannels, Candidate};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::{OStore, StoreConfig};
use crate::services::tui_o::writer::activation::{self, test_hook};
use crate::services::tui_o::writer::host::{self, HostIo, HostParts, Readiness, test_io::TestHost};
use std::time::{Duration, Instant};

const A: u64 = 640030;
const B: u64 = 640031;

/// Candidate `A` beside Legacy neighbour `B`, each with an empty transcript and an open turn.
struct Pair {
    root: PathBuf,
    legs: [Leg; 2],
    io: Arc<TestHost>,
    gate: Arc<OwnershipGate>,
}

impl Pair {
    async fn new() -> Self {
        let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .unwrap();
        let registry = Arc::new(HealthRegistry::new());
        registry.register("claude".into(), shared.clone()).await;
        let legs = [Leg::new(A, &registry), Leg::new(B, &registry)];
        let io = TestHost::new(legs.iter().map(|leg| (leg.channel, leg.source())));
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        Self {
            root,
            legs,
            io,
            gate,
        }
    }

    fn host(&self, io: &Arc<TestHost>) -> Vec<tokio::task::JoinHandle<()>> {
        let parts = || HostParts {
            io: Arc::clone(io),
            runtime_root: Some(self.root.clone()),
            gate: Arc::clone(&self.gate),
            readiness: Arc::new(Readiness::default()),
        };
        host::start(ShadowProvider::Claude, true, parts)
    }

    /// Raw O and Legacy posts carrying the leg's unit; O posts nothing else.
    fn posts(&self, io: &TestHost, leg: usize) -> (u64, u64) {
        let leg = &self.legs[leg];
        let o_posts = io.posts.to(leg.channel);
        let units = o_posts.iter().filter(|p| p.contains(&leg.body)).count() as u64;
        assert_eq!(o_posts.len() as u64, units, "{o_posts:?}");
        (units, leg.legacy_posts())
    }

    fn init_exists(&self) -> bool {
        self.root
            .join("o_store")
            .join(A.to_string())
            .join("init")
            .exists()
    }

    /// Writes A's unit to its transcript, as the TUI would, without any Legacy judgement.
    fn unit_writer(&self) -> impl FnOnce() + Send + 'static {
        let (path, body) = (
            self.legs[0].binding.expected_rollout_path.clone(),
            self.legs[0].body.clone(),
        );
        move || {
            let row = serde_json::json!({"type":"assistant", "uuid":"row-answer", "apiBlockIndex":0,
                "message":{"id":"answer", "content":[{"type":"text", "text":body}]}});
            std::fs::write(path, format!("{row}\n")).unwrap();
        }
    }
}

fn candidate(channel: u64) -> Candidate {
    let found = |boot: Option<&BootChannels>| boot.unwrap().candidate(channel).cloned();
    cutover::test_override::with_channels(found).unwrap()
}

fn adoption(channel: u64) -> Adoption {
    candidate(channel).peek()
}

/// Hands the leg's terminal frame to its Legacy sink, which must settle it without a hold.
async fn finish(leg: &Leg) {
    let outcome = leg.finish_turn().await;
    assert!(
        matches!(outcome, Ok(RelaySinkOutcome::TerminalDelivered)),
        "{outcome:?}"
    );
}

async fn settle() {
    tokio::time::sleep(Duration::from_secs(3)).await;
}

/// Asserts the rejected outcome: Legacy posted A's unit once, O nothing, and no init was written.
fn assert_released(pair: &Pair) {
    assert_eq!(adoption(A), Adoption::Released);
    assert_eq!(
        pair.posts(&pair.io, 0),
        (0, 1),
        "A goes through Legacy only"
    );
    assert!(!pair.init_exists(), "a rejected adoption writes no init");
    assert_eq!(pair.posts(&pair.io, 1), (0, 1), "B stays Legacy");
}

async fn open_intake_releases() {
    let pair = Pair::new().await;
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    pair.io.facts.lock().unwrap().open_intake = 1;
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(
        adoption(A),
        Adoption::Released,
        "rejected before any store write"
    );
    for leg in &pair.legs {
        finish(&leg).await;
    }
    settle().await;
    assert_released(&pair);
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_candidate_with_open_intake_is_released_and_its_unit_posts_once_through_legacy() {
    if isolated(
        "adoption::a_candidate_with_open_intake_is_released_and_its_unit_posts_once_through_legacy",
    ) {
        open_intake_releases().await;
    }
}

async fn legacy_body_first() {
    let pair = Pair::new().await;
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let check = crate::services::tui_o::channel_policy::BodyCheck::watch(A, &pair.legs[0].body);
    pair.legs[0].gateway.check.set(check.clone()).unwrap();
    finish(&pair.legs[0]).await;
    check.assert_settled();
    assert_eq!(
        adoption(A),
        Adoption::Released,
        "the sink's body judgement took the channel"
    );
    let hosts = pair.host(&pair.io);
    finish(&pair.legs[1]).await;
    settle().await;
    assert_released(&pair);
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_legacy_body_before_activation_keeps_the_candidate_on_legacy() {
    if isolated("adoption::a_legacy_body_before_activation_keeps_the_candidate_on_legacy") {
        legacy_body_first().await;
    }
}

async fn unit_between_facts_and_lock() {
    let pair = Pair::new().await;
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    *pair.io.on_facts.lock().unwrap() = Some(Box::new(pair.unit_writer()));
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(
        adoption(A),
        Adoption::Released,
        "the recheck under the lock saw the unit"
    );
    for leg in &pair.legs {
        finish(&leg).await;
    }
    settle().await;
    assert_released(&pair);
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_unit_written_after_the_facts_but_before_the_lock_keeps_the_channel_on_legacy() {
    if isolated(
        "adoption::a_unit_written_after_the_facts_but_before_the_lock_keeps_the_channel_on_legacy",
    ) {
        unit_between_facts_and_lock().await;
    }
}

async fn body_during_the_write() {
    let pair = Pair::new().await;
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let (paused_tx, paused) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel::<()>();
    test_hook::set(A, test_hook::Step::BeforeWrite, move || {
        paused_tx.send(Instant::now()).unwrap();
        resume_rx.recv().unwrap();
        Ok(())
    });
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &pair.root);
    let store = store.unwrap().unwrap();
    let (bindings, adopting) = (pair.io.bindings(A, ShadowProvider::Claude), candidate(A));
    let activation = std::thread::spawn(move || {
        let facts = Ok(Default::default());
        activation::activate(&store, A, facts, &*bindings, || Ok(false), &adopting)
    });
    let locked_at = paused.recv().unwrap();
    // Another channel's body judgement never takes A's lock.
    finish(&pair.legs[1]).await;
    assert_eq!(pair.posts(&pair.io, 1), (0, 1));
    let neighbour = locked_at.elapsed();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        resume.send(()).unwrap();
    });
    let judged = Instant::now();
    finish(&pair.legs[0]).await;
    let waited = judged.elapsed();
    eprintln!(
        "T3c neighbour judged {neighbour:?} into A's lock; A's body judgement waited {waited:?}"
    );
    assert!(
        waited >= Duration::from_millis(250),
        "A's judgement waited for the lock: {waited:?}"
    );
    release.join().unwrap();
    assert_eq!(activation.join().unwrap(), Ok(()));
    assert_eq!(adoption(A), Adoption::Committed);
    assert_eq!(
        pair.legs[0].legacy_posts(),
        0,
        "Legacy consumed the unit behind the commit"
    );
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(
        pair.posts(&pair.io, 0),
        (1, 0),
        "O posts the unit from offset 0"
    );
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_body_judged_while_the_first_init_is_written_waits_for_it_and_goes_to_o() {
    if isolated(
        "adoption::a_body_judged_while_the_first_init_is_written_waits_for_it_and_goes_to_o",
    ) {
        body_during_the_write().await;
    }
}

async fn failure_after_publication() {
    let pair = Pair::new().await;
    let candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    test_hook::set(A, test_hook::Step::AfterWrite, || {
        Err("injected I/O error".into())
    });
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(
        adoption(A),
        Adoption::Held,
        "a failure after a store write keeps O's hold"
    );
    assert!(
        pair.init_exists(),
        "the init was published before the failure"
    );
    finish(&pair.legs[0]).await;
    settle().await;
    assert_eq!(
        pair.posts(&pair.io, 0),
        (0, 0),
        "held: neither writer posts in this process"
    );
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
    drop(candidates);

    let selected = std::collections::BTreeSet::from([A]);
    let seeded = channel_policy::stored(Some(&pair.root), &selected);
    assert_eq!(
        seeded[&A],
        Adoption::Committed,
        "a restart seeds the published init"
    );
    let _restarted = cutover::test_override::force_channels(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let io = TestHost::new([(A, pair.legs[0].source())]);
    let hosts = pair.host(&io);
    settle().await;
    assert_eq!(
        pair.posts(&io, 0),
        (1, 0),
        "the restart posts the unit once, through O"
    );
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_store_failure_after_the_init_is_public_holds_the_channel_and_a_restart_posts_once() {
    if isolated(
        "adoption::a_store_failure_after_the_init_is_public_holds_the_channel_and_a_restart_posts_once",
    ) {
        failure_after_publication().await;
    }
}
