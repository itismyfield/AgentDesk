//! A candidate channel against its real Legacy sink and the real writer host: whichever of the
//! first Legacy body and the first `init` comes first owns the channel, and each unit posts once.

use super::*;
use crate::services::discord::SharedData;
use crate::services::discord::outbound::o_writer_legacy::LegacyRelay;
use crate::services::tui_o::channel_policy::{self, Adoption, BootChannels, Candidate};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::activation::{self, test_hook};
use crate::services::tui_o::writer::adoption::{self as held, LegacyView};
use crate::services::tui_o::writer::binding::BindingEvents;
use crate::services::tui_o::writer::host::{self, HostIo, HostParts, Readiness, test_io::TestHost};
use std::time::{Duration, Instant};

const A: u64 = 640030;
const B: u64 = 640031;

/// Candidate `A` beside Legacy neighbour `B`, each with an empty transcript and an open turn.
struct Pair {
    root: PathBuf,
    shared: Arc<SharedData>,
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
            shared,
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

/// Seals the era over another channel, so the candidate's first `init` is written by itself.
fn seal_era(root: &Path) {
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root);
    let store = store.unwrap().unwrap();
    let other = 640099;
    let init = |channel| {
        Ok(Initialized {
            channel,
            sources: Vec::new(),
            initial_anchor: 0,
            build_digest: "e2e".into(),
            at: chrono::Utc::now(),
        })
    };
    store.begin_era(&[other], chrono::Utc::now(), init).unwrap();
}

async fn failure_after_publication() {
    let pair = Pair::new().await;
    seal_era(&pair.root);
    let candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let dir = pair.root.join("o_store").join(A.to_string());
    // The init is linked and synced, then unlinking its temp name fails and leaves that alias.
    test_hook::set(A, test_hook::Step::AfterWrite, move || {
        std::fs::hard_link(dir.join("init"), dir.join("init.e2e.tmp")).unwrap();
        Err("injected temp unlink failure".into())
    });
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(
        adoption(A),
        Adoption::Committed,
        "a failure after a readable init keeps the channel with O"
    );
    assert!(
        pair.init_exists(),
        "the init was published before the failure"
    );
    finish(&pair.legs[0]).await;
    settle().await;
    assert_eq!(
        pair.posts(&pair.io, 0),
        (1, 0),
        "O posts the unit in this process and Legacy only consumes it"
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
    assert_eq!(pair.posts(&io, 0), (0, 0), "the restart posts nothing more");
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_store_failure_after_the_init_is_readable_commits_the_channel_and_a_restart_posts_nothing_more()
 {
    if isolated(
        "adoption::a_store_failure_after_the_init_is_readable_commits_the_channel_and_a_restart_posts_nothing_more",
    ) {
        failure_after_publication().await;
    }
}

const SECOND_STARTED: &str = "2026-09-30T00:05:00Z";

/// The rows of the leg's first turn as `Leg::finish_turn` writes them.
fn first_turn(body: &str) -> String {
    let rows = [
        serde_json::json!({"type":"assistant", "uuid":"row-answer", "apiBlockIndex":0,
            "message":{"id":"answer", "content":[{"type":"text", "text":body}]}}),
        serde_json::json!({"type":"result", "result":body}),
    ];
    rows.iter().map(|row| format!("{row}\n")).collect()
}

/// A first turn as the Claude TUI writes it: the stop hook closes the reply, then the turn's
/// duration and the TUI's own bookkeeping follow.
fn warm_up(body: &str) -> String {
    let rows = [
        serde_json::json!({"type":"user", "uuid":"row-warm-q", "message":{"content":"hello"}}),
        serde_json::json!({"type":"assistant", "uuid":"row-answer", "apiBlockIndex":0,
            "message":{"id":"answer", "content":[{"type":"text", "text":body}]}}),
        serde_json::json!({"type":"system", "subtype":"stop_hook_summary", "hookCount":1}),
        serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":1200}),
        serde_json::json!({"type":"last-prompt", "leafUuid":"row-warm-q", "sessionId":"e2e"}),
        serde_json::json!({"type":"ai-title", "aiTitle":"warm-up", "sessionId":"e2e"}),
        serde_json::json!({"type":"permission-mode", "permissionMode":"default", "sessionId":"e2e"}),
    ];
    rows.iter().map(|row| format!("{row}\n")).collect()
}

/// Where Legacy's own transcript reader ends the turn read from 0, which its delivery commits.
fn legacy_turn_end(path: &str) -> u64 {
    let (tx, _frames) = std::sync::mpsc::channel();
    let probe = crate::services::provider::SessionProbe::new(|| true, || false);
    let read = crate::services::session_backend::read_output_file_until_result_with_harvest(
        path, 0, tx, None, probe,
    );
    match read.map(|(result, stats)| (result, stats.decoded_terminal)) {
        Ok((crate::services::provider::ReadOutputResult::Completed { offset }, true)) => offset,
        other => panic!(
            "the reader ends the turn: {:?}",
            other.map(|(result, _)| result)
        ),
    }
}

/// A's rows for its second turn, carrying a unit no other turn carries.
fn second_turn(body: &str) -> String {
    let text = format!("{body} second");
    let rows = [
        serde_json::json!({"type":"user", "uuid":"row-second-q", "message":{"content":"again"}}),
        serde_json::json!({"type":"assistant", "uuid":"row-second", "apiBlockIndex":0,
            "message":{"id":"answer-second", "content":[{"type":"text", "text":text}]}}),
        serde_json::json!({"type":"result", "result":text}),
    ];
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn transcript_len(path: &str) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// Appends the second turn as the TUI writes it, without any Legacy judgement; returns its start.
fn write_second(path: &str, body: &str) -> (u64, String) {
    use std::io::Write;
    let (start, rows) = (transcript_len(path), second_turn(body));
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(rows.as_bytes()).unwrap();
    (start, rows)
}

/// Hands the second turn to A's Legacy sink over its own open turn, as its tail would.
async fn deliver_second(leg: &Leg, start: u64, rows: &str) {
    let session = &leg.binding.expected_session_name;
    let mut row =
        inflight_with_identity_offset(leg.channel, session, 711, SECOND_STARTED, Some(start));
    row.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    row.current_msg_id = 88011;
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    let end = start + rows.len() as u64;
    let mut frame =
        terminal_frame_offset(&leg.binding, rows, 1, end, 711, SECOND_STARTED, Some(start));
    frame.relay_generation_mtime_ns = Some(dr::current_generation_mtime_ns(session));
    let outcome = leg.sink.deliver(&frame).await;
    assert!(
        matches!(outcome, Ok(RelaySinkOutcome::TerminalDelivered)),
        "{outcome:?}"
    );
}

/// Legacy's cursor where a rehydrate pass of this process leaves it: the transcript's length.
fn rehydrated(leg: &Leg) -> u64 {
    let end = transcript_len(&leg.binding.expected_rollout_path);
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &leg.binding.expected_session_name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: leg.binding.expected_rollout_path.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("e2e".into()),
            last_offset: end,
            relay_last_offset: None,
        },
    );
    crate::services::claude_tui::hook_server::mark_boot_discovery_complete();
    end
}

fn legacy_cursor(leg: &Leg) -> u64 {
    let session = &leg.binding.expected_session_name;
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session);
    binding.unwrap().last_offset
}

impl Pair {
    /// O reads this process's real Legacy relay state for A, under A's own tmux session.
    fn read_legacy(&self) -> Arc<dyn LegacyView> {
        let legacy: Arc<dyn LegacyView> = Arc::new(LegacyRelay::new(self.shared.clone()));
        *self.io.legacy.lock().unwrap() = Some(Arc::clone(&legacy));
        let session = self.legs[0].binding.expected_session_name.clone();
        self.io.sessions.lock().unwrap().insert(A, session);
        legacy
    }

    /// A's warm-up turn, delivered by Legacy up to where its own reader ends the turn, before A
    /// was selected; returns Legacy's cursor after a restart's rehydrate pass.
    async fn delivered_history(&self) -> u64 {
        let unselected = cutover::test_override::force_channels(&[]);
        let leg = &self.legs[0];
        let path = &leg.binding.expected_rollout_path;
        std::fs::write(path, warm_up(&leg.body)).unwrap();
        let end = legacy_turn_end(path);
        eprintln!(
            "warm-up: Legacy's reader ends the turn at {end} of {}",
            transcript_len(path)
        );
        // The sink posts the body its terminal row carries and commits the reader's end.
        let payload = first_turn(&leg.body);
        let mut frame =
            terminal_frame_offset(&leg.binding, &payload, 1, end, 710, STARTED, Some(0));
        let session = &leg.binding.expected_session_name;
        frame.relay_generation_mtime_ns = Some(dr::current_generation_mtime_ns(session));
        let outcome = leg.sink.deliver(&frame).await;
        assert!(
            matches!(outcome, Ok(RelaySinkOutcome::TerminalDelivered)),
            "{outcome:?}"
        );
        drop(unselected);
        assert_eq!(self.legs[0].legacy_posts(), 1);
        self.read_legacy();
        rehydrated(&self.legs[0])
    }

    fn o_start(&self) -> u64 {
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &self.root);
        let init = store.unwrap().unwrap().read_init(A).unwrap().unwrap();
        let current = self.legs[0].source();
        let start = init.sources.iter().find(|s| s.source_id == current);
        start
            .expect("A's current source is attached")
            .delivery_start
    }

    fn released_for(&self, reason: &str) {
        let alarms = self.io.alarms.0.lock().unwrap();
        let released = |(channel, alarm): &(u64, _)| {
            *channel == A
                && matches!(alarm, crate::services::tui_o::writer::WriterAlarm::Released { detail } if detail.contains(reason))
        };
        assert!(
            alarms.iter().any(released),
            "released for {reason}: {alarms:?}"
        );
    }
}

async fn unit_after_the_pin() {
    let pair = Pair::new().await;
    let cursor = pair.delivered_history().await;
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let written = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&written);
    let path = pair.legs[0].binding.expected_rollout_path.clone();
    let body = pair.legs[0].body.clone();
    test_hook::set(A, test_hook::Step::BeforeLock, move || {
        *slot.lock().unwrap() = Some(write_second(&path, &body));
        Ok(())
    });
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(adoption(A), Adoption::Released);
    assert!(!pair.init_exists(), "a moved transcript writes no init");
    pair.released_for("length moved");
    let (start, rows) = written
        .lock()
        .unwrap()
        .take()
        .expect("the unit was written");
    assert_eq!(start, cursor, "the unit starts at Legacy's cursor");
    deliver_second(&pair.legs[0], start, &rows).await;
    settle().await;
    assert_eq!(
        pair.posts(&pair.io, 0),
        (0, 2),
        "Legacy posts the unit from its cursor"
    );
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_unit_written_after_the_pin_keeps_the_channel_on_legacy_from_its_cursor() {
    if isolated(
        "adoption::a_unit_written_after_the_pin_keeps_the_channel_on_legacy_from_its_cursor",
    ) {
        unit_after_the_pin().await;
    }
}

async fn undelivered_closed_turn() {
    let pair = Pair::new().await;
    // A's turn reached the transcript but no Legacy delivery covered it yet: its inflight row
    // still stands, and the gateway reads that as Legacy's custody.
    let leg = &pair.legs[0];
    std::fs::write(&leg.binding.expected_rollout_path, first_turn(&leg.body)).unwrap();
    pair.read_legacy();
    rehydrated(&pair.legs[0]);
    *pair.io.custody.lock().unwrap() = Some(|channel| {
        crate::services::discord::inflight::inflight_state_file_exists(
            &ProviderKind::Claude,
            channel,
        )
    });
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(adoption(A), Adoption::Released);
    pair.released_for("Legacy retains delivery custody");
    // The late delivery of that turn goes through Legacy, as a tail started below the cursor would.
    finish(&pair.legs[0]).await;
    settle().await;
    assert_eq!(
        pair.posts(&pair.io, 0),
        (0, 1),
        "Legacy posts the turn once"
    );
    assert!(!pair.init_exists());
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_closed_turn_legacy_has_not_delivered_keeps_the_channel_on_legacy() {
    if isolated("adoption::a_closed_turn_legacy_has_not_delivered_keeps_the_channel_on_legacy") {
        undelivered_closed_turn().await;
    }
}

async fn adopted_by_the_host() {
    let pair = Pair::new().await;
    let cursor = pair.delivered_history().await;
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let written = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&written);
    let path = pair.legs[0].binding.expected_rollout_path.clone();
    let body = pair.legs[0].body.clone();
    // The second turn lands after the recheck passed, before the init is written.
    test_hook::set(A, test_hook::Step::BeforeWrite, move || {
        *slot.lock().unwrap() = Some(write_second(&path, &body));
        Ok(())
    });
    let hosts = pair.host(&pair.io);
    settle().await;
    assert_eq!(adoption(A), Adoption::Committed);
    assert_eq!(
        (pair.o_start(), legacy_cursor(&pair.legs[0])),
        (cursor, cursor),
        "the host starts O at the cursor Legacy reads from"
    );
    let (start, rows) = written
        .lock()
        .unwrap()
        .take()
        .expect("the unit was written");
    deliver_second(&pair.legs[0], start, &rows).await;
    settle().await;
    let seconds = pair.io.posts.to(A);
    assert_eq!(
        seconds.len(),
        1,
        "O posts the second turn's unit only: {seconds:?}"
    );
    assert!(seconds[0].ends_with("second"), "{seconds:?}");
    assert_eq!(
        pair.legs[0].legacy_posts(),
        1,
        "Legacy consumed the second turn"
    );
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn the_host_adopts_a_channel_with_delivered_history_at_legacys_cursor() {
    if isolated("adoption::the_host_adopts_a_channel_with_delivered_history_at_legacys_cursor") {
        adopted_by_the_host().await;
    }
}

async fn unit_after_the_recheck() {
    let pair = Pair::new().await;
    let cursor = pair.delivered_history().await;
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
    let (log, adopting) = (pair.io.bindings(A, ShadowProvider::Claude), candidate(A));
    let legacy = pair.io.legacy();
    let activation = std::thread::spawn(move || {
        let events = log.binding_events_since(A, 0)?;
        let snapshot = held::pin(&*legacy, &events, A)?;
        let sources = || snapshot.recheck(&*legacy, &*log, A);
        let facts = Ok(Default::default());
        let asked = Instant::now();
        let result = activation::activate_with(&store, A, facts, || Ok(false), &adopting, sources);
        result.map(|()| asked.elapsed())
    });
    let locked_at = paused.recv().unwrap();
    let leg = &pair.legs[0];
    let (start, rows) = write_second(&leg.binding.expected_rollout_path, &leg.body);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        resume.send(()).unwrap();
    });
    let judged = Instant::now();
    deliver_second(&pair.legs[0], start, &rows).await;
    let waited = judged.elapsed();
    release.join().unwrap();
    let held_for = activation.join().unwrap().unwrap();
    eprintln!(
        "T3c adoption over history: lock held {held_for:?} with a 300 ms pause since {:?}; \
         A's body judgement waited {waited:?}",
        locked_at.elapsed()
    );
    assert!(
        waited >= Duration::from_millis(250),
        "the judgement waited: {waited:?}"
    );
    assert_eq!(adoption(A), Adoption::Committed);
    assert_eq!(
        (pair.o_start(), legacy_cursor(&pair.legs[0])),
        (cursor, cursor),
        "O starts at the cursor Legacy reads from"
    );
    assert_eq!(
        pair.legs[0].legacy_posts(),
        1,
        "Legacy consumed the second turn"
    );
    let hosts = pair.host(&pair.io);
    settle().await;
    let seconds = pair.io.posts.to(A);
    assert_eq!(
        seconds.len(),
        1,
        "O posts the second turn's unit only: {seconds:?}"
    );
    assert!(seconds[0].ends_with("second"), "{seconds:?}");
    hosts.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test(start_paused = true)]
async fn a_unit_written_after_the_recheck_goes_to_o_from_legacys_cursor() {
    if isolated("adoption::a_unit_written_after_the_recheck_goes_to_o_from_legacys_cursor") {
        unit_after_the_recheck().await;
    }
}

/// Another holder's lease on the range leaves the sink's controller sending nothing, so the
/// candidate stays pending for the next body.
async fn lost_lease() {
    use crate::services::discord::{LeaseHolder, lease_now_ms};
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    registry.register("claude".into(), shared.clone()).await;
    let leg = Leg::new(A, &registry);
    let _candidates =
        cutover::test_override::force_candidates(&[(A, RuntimeHandoffKind::ClaudeTui)]);
    let check = crate::services::tui_o::channel_policy::BodyCheck::watch(A, &leg.body);
    leg.gateway.check.set(check.clone()).unwrap();
    let channel = ChannelId::new(A);
    let session = &leg.binding.expected_session_name;
    let generation = shared.restart.current_generation;
    let key = crate::services::discord::tmux::pinned_delivery_lease_key_for_test(
        channel, generation, None, session, 1, 0,
    );
    let watcher = LeaseHolder::Watcher { instance_id: 7 };
    let deadline = lease_now_ms() + 60_000;
    let cell = shared.delivery_lease(channel);
    assert!(cell.try_acquire(key, watcher, 0, u64::MAX, deadline));

    let outcome = leg.finish_turn().await;
    assert!(
        matches!(outcome, Err(RelaySinkError::Transient(_))),
        "{outcome:?}"
    );
    assert_eq!(
        leg.legacy_posts(),
        0,
        "the lease holder delivers this range"
    );
    check.assert_settled();
    assert_eq!(adoption(A), Adoption::Pending);
}

#[tokio::test(start_paused = true)]
async fn a_sink_that_loses_its_delivery_lease_leaves_a_pending_adoption() {
    if isolated("adoption::a_sink_that_loses_its_delivery_lease_leaves_a_pending_adoption") {
        lost_lease().await;
    }
}
