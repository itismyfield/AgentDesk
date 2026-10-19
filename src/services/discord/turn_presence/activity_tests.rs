use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use serde_json::json;

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvidence, BindingRecord, BindingTarget,
};

#[path = "activity_tests/large_tests.rs"]
mod large;

const CHANNEL: u64 = 6_100_001;
const SESSION: &str = "presence-parent";

struct Fake {
    seq: Mutex<Result<u64, String>>,
    events: Mutex<Result<Vec<BindingEvent>, String>>,
    present: AtomicBool,
    ready: AtomicBool,
    busy: AtomicBool,
    liveness: Mutex<SessionLiveness>,
    folds: AtomicUsize,
    jobs: Mutex<Vec<Job>>,
    spawns: AtomicUsize,
    /// The next pane read reports it started, then waits until released.
    pause: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    /// A host read pauses independently from the pane readiness read.
    host_pause: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Ports for Fake {
    fn liveness(&self, _: ShadowProvider, _: u64, _: &str) -> SessionLiveness {
        let answer = *lock(&self.liveness);
        let pause = lock(&self.host_pause).take();
        if let Some((started, release)) = pause {
            started.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        answer
    }
    fn binding_seq(&self, _: u64) -> Result<u64, String> {
        lock(&self.seq).clone()
    }
    fn binding_events(&self, _: u64, _: ShadowProvider) -> Result<Vec<BindingEvent>, String> {
        self.folds.fetch_add(1, Ordering::SeqCst);
        lock(&self.events).clone()
    }
    fn session_present(&self, _: &str) -> bool {
        self.present.load(Ordering::SeqCst)
    }
    fn final_ready(&self, _: &str) -> bool {
        // Taken before waiting, so another read meanwhile does not block on this lock.
        let pause = lock(&self.pause).take();
        if let Some((started, release)) = pause {
            started.send(()).unwrap();
            let _ = release.recv();
        }
        self.ready.load(Ordering::SeqCst)
    }
    fn pane_busy(&self, _: &str) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    fn spawn(&self, job: Job) -> bool {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        lock(&self.jobs).push(job);
        true
    }
}

/// One channel's watch with scripted ports; workers run only when the test says so.
struct Probe {
    watch: Arc<Mutex<Watch>>,
    fake: Arc<Fake>,
    provider: ShadowProvider,
    target: Target,
}

impl Probe {
    fn new(provider: ShadowProvider, events: Vec<BindingEvent>) -> Self {
        let fake = Arc::new(Fake {
            seq: Mutex::new(Ok(events.len() as u64)),
            events: Mutex::new(Ok(events)),
            present: AtomicBool::new(true),
            ready: AtomicBool::new(true),
            busy: AtomicBool::new(false),
            liveness: Mutex::new(SessionLiveness::Alive),
            folds: AtomicUsize::new(0),
            jobs: Mutex::new(Vec::new()),
            spawns: AtomicUsize::new(0),
            pause: Mutex::new(None),
            host_pause: Mutex::new(None),
        });
        let target = Target::Bound(SESSION.into());
        let watch = Arc::default();
        Self {
            watch,
            fake,
            provider,
            target,
        }
    }

    fn ask(&self) -> (Activity, &'static str) {
        let ports: Arc<dyn Ports> = self.fake.clone();
        let observed = observe(
            &self.watch,
            &ports,
            self.provider,
            CHANNEL,
            self.target.clone(),
        );
        (observed.activity, observed.reason)
    }

    fn ask_presence(&self) -> (Activity, &'static str) {
        let ports: Arc<dyn Ports> = self.fake.clone();
        let (observed, _) = judge_presence(
            &self.watch,
            &ports,
            self.provider,
            CHANNEL,
            self.target.clone(),
        );
        (observed.activity, observed.reason)
    }

    fn run_jobs(&self) -> usize {
        let jobs = std::mem::take(&mut *lock(&self.fake.jobs));
        let ran = jobs.len();
        jobs.into_iter().for_each(|job| job());
        ran
    }

    /// Asks until no worker is left to finish; a probe that keeps rebuilding stays `catching_up`.
    fn settle(&self) -> (Activity, &'static str) {
        let mut answer = self.ask();
        for _ in 0..8 {
            if answer.1 != "catching_up" || self.run_jobs() == 0 {
                break;
            }
            answer = self.ask();
        }
        answer
    }

    fn counts(&self) -> (usize, usize) {
        let fake = &self.fake;
        (
            fake.folds.load(Ordering::SeqCst),
            fake.spawns.load(Ordering::SeqCst),
        )
    }

    fn age(&self, by: Duration) {
        let mut watch = lock(&self.watch);
        watch.grew_at = Instant::now() - by;
    }
}

fn bound(seq: u64, session: &str, source: &SourceId, provider: ShadowProvider) -> BindingEvent {
    let target = BindingTarget::Source(source.clone());
    event(seq, session, provider, record(target))
}

fn record(new: BindingTarget) -> BindingRecord {
    let received_at = chrono::Utc::now();
    let hook_event = "SessionStart".into();
    BindingRecord::Bound {
        old: None,
        new,
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event,
            received_at,
            reclaims: false,
        },
    }
}

fn event(seq: u64, session: &str, provider: ShadowProvider, record: BindingRecord) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider,
        tmux_session: session.into(),
        execution_nonce: "presence".into(),
        record,
        committed_at: chrono::Utc::now(),
    }
}

fn write(path: &Path, rows: &[serde_json::Value]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

fn prompt(id: &str) -> serde_json::Value {
    json!({"type":"user","uuid":id,"message":{"content":"direct turn"}})
}

fn end() -> serde_json::Value {
    json!({"type":"system","subtype":"turn_duration"})
}

fn summary() -> serde_json::Value {
    json!({"type":"summary","summary":"earlier session"})
}

fn codex_header(root: &Path) -> serde_json::Value {
    json!({"type":"session_meta","payload":{"id":"parent","cwd":root,"source":"cli","originator":"codex-tui"}})
}

fn codex(kind: &str, id: &str) -> serde_json::Value {
    json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}})
}

/// A Claude probe bound to one transcript holding `rows`.
fn claude(root: &Path, rows: &[serde_json::Value]) -> (Probe, std::path::PathBuf) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join("parent.jsonl");
    write(&path, rows);
    let source = source_id_for("parent", &path).unwrap();
    let probe = Probe::new(
        ShadowProvider::Claude,
        vec![bound(1, SESSION, &source, ShadowProvider::Claude)],
    );
    (probe, path)
}

fn codex_probe(root: &Path, rows: &[serde_json::Value]) -> (Probe, std::path::PathBuf) {
    let path = root.join("rollout.jsonl");
    write(&path, &[codex_header(root)]);
    write(&path, rows);
    let source = source_id_for("parent", &path).unwrap();
    let probe = Probe::new(
        ShadowProvider::Codex,
        vec![bound(1, SESSION, &source, ShadowProvider::Codex)],
    );
    (probe, path)
}

/// Each row of the judgment table, read the way an effect point asks once its worker finished.
#[test]
fn every_observation_maps_to_its_activity_and_reason() {
    use Activity::{Busy, Idle, Unknown};
    let root = tempfile::tempdir().unwrap();
    let dir = |name: &str| {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let mut observed = Vec::new();
    let mut note =
        |name: &'static str, answer: (Activity, &'static str)| observed.push((name, answer));

    let (probe, _) = claude(&dir("none"), &[]);
    for (name, target, present) in [
        ("no watcher, no session", Target::Unbound(None), true),
        (
            "no watcher, session gone",
            Target::Unbound(Some("gone".into())),
            false,
        ),
        (
            "no watcher, session live",
            Target::Unbound(Some("live".into())),
            true,
        ),
    ] {
        let probe = Probe {
            target,
            ..Probe::new(ShadowProvider::Claude, Vec::new())
        };
        probe.fake.present.store(present, Ordering::SeqCst);
        note(name, probe.settle());
    }
    *lock(&probe.fake.seq) = Err("log unreadable".into());
    note("seq unreadable", probe.settle());

    let (probe, _) = claude(&dir("events"), &[prompt("a")]);
    *lock(&probe.fake.events) = Err("log unreadable".into());
    note("events unreadable", probe.settle());

    let pending = BindingTarget::Pending {
        payload_session_id: "parent".into(),
        payload_transcript_path: root.path().join("later.jsonl"),
    };
    let probe = Probe::new(
        ShadowProvider::Claude,
        vec![event(1, SESSION, ShadowProvider::Claude, record(pending))],
    );
    note("pending", probe.settle());

    for (name, ready, provider) in [
        ("no source, ready", true, ShadowProvider::Claude),
        ("no source, not ready", false, ShadowProvider::Claude),
        ("codex no source", true, ShadowProvider::Codex),
    ] {
        let probe = Probe::new(provider, Vec::new());
        probe.fake.ready.store(ready, Ordering::SeqCst);
        note(name, probe.settle());
    }
    let (probe, path) = claude(&dir("missing"), &[]);
    std::fs::remove_file(&path).unwrap();
    note("bound file missing, ready pane", probe.settle());

    let (probe, _) = claude(&dir("closed"), &[prompt("a"), end()]);
    note("closed", probe.settle());
    let (probe, _) = claude(&dir("open"), &[prompt("a")]);
    note("open", probe.settle());
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    note("stale open, pane idle", probe.ask());
    probe.fake.busy.store(true, Ordering::SeqCst);
    note("stale open, pane busy", probe.ask());
    let (probe, _) = codex_probe(&dir("codex-open"), &[codex("task_started", "A")]);
    note("codex open", probe.settle());
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    probe.fake.busy.store(true, Ordering::SeqCst);
    note("codex stale open", probe.ask());

    let (probe, _) = claude(&dir("meta"), &[summary()]);
    note("metadata only, ready", probe.settle());
    probe.fake.ready.store(false, Ordering::SeqCst);
    note("metadata only, not ready", probe.ask());
    let (probe, path) = claude(&dir("meta-open"), &[summary()]);
    note("metadata, ready", probe.settle());
    write(&path, &[prompt("a")]);
    note("metadata then open", probe.ask());
    let anonymous = json!({"type":"assistant","apiBlockIndex":0,
        "message":{"id":"m","content":[{"type":"text","text":"hi"}]}});
    let (probe, _) = claude(&dir("anonymous"), &[anonymous]);
    note("evidence without a boundary", probe.settle());
    let (probe, _) = codex_probe(&dir("codex-meta"), &[]);
    note("codex metadata only", probe.settle());

    let (probe, path) = claude(&dir("large"), &[]);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_TRANSCRIPT_BYTES + 1)
        .unwrap();
    note("too large", probe.settle());
    let (probe, path) = claude(&dir("broken"), &[prompt("a")]);
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{{\"type\":\"user\""
    )
    .unwrap();
    note("broken json", probe.settle());

    #[rustfmt::skip]
    let expected = [
        ("no watcher, no session", (Unknown, "session_identity_unresolved")),
        ("no watcher, session gone", (Unknown, "session_identity_unresolved")),
        ("no watcher, session live", (Unknown, "watcher_unbound")),
        ("seq unreadable", (Unknown, "binding_unreadable")),
        ("events unreadable", (Unknown, "binding_unreadable")),
        ("pending", (Unknown, "binding_pending")),
        ("no source, ready", (Idle, "no_turn_evidence_ready")),
        ("no source, not ready", (Unknown, "no_turn_evidence")),
        ("codex no source", (Unknown, "no_turn_evidence")),
        ("bound file missing, ready pane", (Unknown, "binding_unreadable")),
        ("closed", (Idle, "closed")),
        ("open", (Busy, "open")),
        ("stale open, pane idle", (Unknown, "open_without_progress")),
        ("stale open, pane busy", (Busy, "open_pane_busy")),
        ("codex open", (Busy, "open")),
        ("codex stale open", (Unknown, "open_without_progress")),
        ("metadata only, ready", (Idle, "no_turn_evidence_ready")),
        ("metadata only, not ready", (Unknown, "no_turn_evidence")),
        ("metadata, ready", (Idle, "no_turn_evidence_ready")),
        ("metadata then open", (Busy, "open")),
        ("evidence without a boundary", (Unknown, "no_turn_boundary")),
        ("codex metadata only", (Unknown, "no_turn_evidence")),
        ("too large", (Unknown, "catching_up")),
        ("broken json", (Unknown, "facts_halted")),
    ];
    assert_eq!(observed, expected);
}

/// A transcript last written long ago reads stale after a restart, not freshly busy.
#[test]
fn a_rebuilt_open_turn_takes_its_age_from_the_file() {
    let root = tempfile::tempdir().unwrap();
    let (probe, path) = claude(root.path(), &[prompt("a")]);
    let old = SystemTime::now() - STALE_OPEN_AFTER - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(probe.settle(), (Activity::Unknown, "open_without_progress"));
    write(
        &path,
        &[json!({"type":"assistant","uuid":"b","apiBlockIndex":0,
        "message":{"id":"m","content":[{"type":"text","text":"still going"}]}})],
    );
    assert_eq!(probe.ask(), (Activity::Busy, "open"));
}

/// Unread bytes past the chunk budget read `catching_up` ahead of every other answer, and the
/// next ask continues from where the last one stopped.
#[test]
fn bytes_past_the_chunk_budget_read_catching_up_before_any_judgment() {
    let root = tempfile::tempdir().unwrap();
    let big = json!({"type":"summary","summary":"x".repeat(CHUNK_BUDGET as usize + 1024)});
    let (probe, path) = claude(root.path(), &[summary()]);
    assert_eq!(probe.settle(), (Activity::Idle, "no_turn_evidence_ready"));
    write(&path, std::slice::from_ref(&big));
    assert_eq!(probe.ask(), (Activity::Unknown, "catching_up"));
    assert_eq!(probe.ask(), (Activity::Idle, "no_turn_evidence_ready"));
    write(&path, &[prompt("a"), big]);
    assert_eq!(probe.ask(), (Activity::Unknown, "catching_up"));
    assert_eq!(probe.ask(), (Activity::Busy, "open"));
    assert_eq!(probe.counts(), (1, 1), "reading on never rebuilds");
}

/// Presence requires a live host; the dormant observer preserves the existing input judgments.
#[test]
fn presence_requires_live_host_without_changing_existing_open_or_closed_judgments() {
    let root = tempfile::tempdir().unwrap();
    for provider in [ShadowProvider::Claude, ShadowProvider::Codex] {
        let dir = root.path().join(format!("{provider:?}"));
        std::fs::create_dir_all(&dir).unwrap();
        let (open, _) = match provider {
            ShadowProvider::Claude => claude(&dir.join("open"), &[prompt("A")]),
            ShadowProvider::Codex => codex_probe(&dir, &[codex("task_started", "A")]),
        };
        assert_eq!(open.settle(), (Activity::Busy, "open"));
        assert_eq!(open.ask_presence(), (Activity::Busy, "open"));
        let closed_dir = dir.join("closed");
        std::fs::create_dir_all(&closed_dir).unwrap();
        let (closed, _) = match provider {
            ShadowProvider::Claude => claude(&closed_dir, &[prompt("A"), end()]),
            ShadowProvider::Codex => codex_probe(
                &closed_dir,
                &[codex("task_started", "A"), codex("task_complete", "A")],
            ),
        };
        assert_eq!(closed.settle(), (Activity::Idle, "closed"));
        for (liveness, reason) in [
            (SessionLiveness::Missing, "host_dead"),
            (SessionLiveness::ProbeFailed, "host_probe_failed"),
            (SessionLiveness::Unknown, "host_unobservable"),
        ] {
            *lock(&open.fake.liveness) = liveness;
            *lock(&closed.fake.liveness) = liveness;
            assert_eq!(open.ask_presence(), (Activity::Unknown, reason));
            assert_eq!(closed.ask_presence(), (Activity::Unknown, reason));
            assert_eq!(open.ask(), (Activity::Busy, "open"));
            assert_eq!(closed.ask(), (Activity::Idle, "closed"));
        }
        *lock(&open.fake.liveness) = SessionLiveness::Alive;
        assert_eq!(open.ask_presence(), (Activity::Busy, "open"));
    }
}

/// Foreign completions and aborts cannot finish the bound parent turn.
#[test]
fn presence_keeps_codex_parent_busy_until_its_own_closer() {
    let root = tempfile::tempdir().unwrap();
    let (probe, path) = codex_probe(root.path(), &[codex("task_started", "A")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    for kind in ["task_complete", "turn_aborted"] {
        write(&path, &[codex(kind, "B")]);
        assert_eq!(probe.ask_presence(), (Activity::Busy, "open"), "{kind}");
    }
    write(&path, &[codex("task_complete", "A")]);
    assert_eq!(probe.ask_presence(), (Activity::Idle, "closed"));
}

/// A slow host answer cannot publish a judgment that a newer poll or binding superseded.
#[test]
fn presence_host_read_overtaken_by_a_poll_or_rebind_answers_unknown() {
    let root = tempfile::tempdir().unwrap();
    for case in ["closed", "rebound", "backlog", "unreadable"] {
        let (probe, path) = claude(&root.path().join(case), &[prompt("A")]);
        assert_eq!(probe.settle(), (Activity::Busy, "open"));
        let (started, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        let (overtook, overtaking) = mpsc::channel();
        *lock(&probe.fake.host_pause) = Some((started, released));
        let (later, paused) = std::thread::scope(|scope| {
            // Dropping the sender on any failed assertion releases the paused host read.
            let release = release;
            scope.spawn(|| finished.send(probe.ask_presence()).unwrap());
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            let expected = match case {
                "closed" => {
                    write(&path, &[end()]);
                    (Activity::Idle, "closed")
                }
                "rebound" => {
                    *lock(&probe.fake.seq) = Ok(9);
                    (Activity::Unknown, "catching_up")
                }
                "backlog" => {
                    write(
                        &path,
                        &[
                            json!({"type":"summary","summary":"x".repeat(CHUNK_BUDGET as usize + 1024)}),
                        ],
                    );
                    (Activity::Unknown, "catching_up")
                }
                "unreadable" => {
                    *lock(&probe.fake.seq) = Err("unreadable binding".into());
                    (Activity::Unknown, "binding_unreadable")
                }
                _ => unreachable!(),
            };
            scope.spawn(|| overtook.send(probe.ask_presence()).unwrap());
            let later = overtaking.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(later, expected, "{case}");
            release.send(()).unwrap();
            let paused = completion.recv_timeout(Duration::from_secs(5)).unwrap();
            (later, paused)
        });
        assert_eq!(
            paused,
            (Activity::Unknown, "superseded"),
            "{case}: {later:?}"
        );
    }
}

fn strict_reading(probe: &Probe) -> Reading {
    let ports: Arc<dyn Ports> = probe.fake.clone();
    let (observed, stamp) = judge_presence(
        &probe.watch,
        &ports,
        probe.provider,
        CHANNEL,
        probe.target.clone(),
    );
    Reading {
        observed,
        stamp,
        watch: Some(probe.watch.clone()),
        unbound: None,
    }
}

#[test]
fn b2_stamped_unknown_advances_revision_and_revokes_busy() {
    let root = tempfile::tempdir().unwrap();
    let (probe, _) = codex_probe(root.path(), &[codex("task_started", "A")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    let busy = strict_reading(&probe);
    assert_eq!(publish(&busy, || 1), Some(1));
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    let unknown = strict_reading(&probe);
    assert_eq!(unknown.observed.activity, Activity::Unknown);
    assert_eq!(publish(&unknown, || 2), Some(2));
    assert!(unknown.stamp.as_ref().unwrap().revision > busy.stamp.as_ref().unwrap().revision);
    assert_eq!(publish(&busy, || 3), None);
}

#[test]
fn b2_generic_observer_keeps_its_existing_unknown_policy() {
    let root = tempfile::tempdir().unwrap();
    let (probe, _) = codex_probe(root.path(), &[codex("task_started", "A")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    let ports: Arc<dyn Ports> = probe.fake.clone();
    let busy = judge(
        &probe.watch,
        &ports,
        probe.provider,
        CHANNEL,
        probe.target.clone(),
    );
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    let unknown = judge(
        &probe.watch,
        &ports,
        probe.provider,
        CHANNEL,
        probe.target.clone(),
    );
    assert_eq!(unknown.0.activity, Activity::Unknown);
    assert_eq!(busy.1, unknown.1);
}

#[test]
fn b2_superseded_host_read_cannot_borrow_successor_stamp() {
    let root = tempfile::tempdir().unwrap();
    let (probe, _) = claude(root.path(), &[prompt("A")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    let (started, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *lock(&probe.fake.host_pause) = Some((started, released));
    std::thread::scope(|scope| {
        let old = scope.spawn(|| strict_reading(&probe));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        lock(&probe.watch).revision += 1;
        let successor = strict_reading(&probe);
        assert_eq!(successor.observed.activity, Activity::Busy);
        release.send(()).unwrap();
        let old = old.join().unwrap();
        assert_eq!(old.observed.reason, "superseded");
        assert_eq!(publish(&old, || 1), None);
        assert_eq!(publish(&successor, || 2), Some(2));
    });
}

#[test]
fn b2_superseded_pane_read_cannot_borrow_successor_stamp() {
    let probe = Probe::new(ShadowProvider::Claude, vec![]);
    assert_eq!(probe.settle(), (Activity::Idle, "no_turn_evidence_ready"));
    let (started, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *lock(&probe.fake.pause) = Some((started, released));
    std::thread::scope(|scope| {
        let old = scope.spawn(|| strict_reading(&probe));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        lock(&probe.watch).revision += 1;
        let successor = strict_reading(&probe);
        release.send(()).unwrap();
        let old = old.join().unwrap();
        assert_eq!(old.observed.reason, "superseded");
        assert_eq!(publish(&old, || 1), None);
        assert_eq!(publish(&successor, || 2), Some(2));
    });
}

/// Each stored outcome answers from memory for the same key; only an unreadable one retries,
/// once, after its delay.
#[test]
fn a_finished_rebuild_is_kept_for_its_key() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(ShadowProvider::Claude, Vec::new());
    assert_eq!(probe.ask(), (Activity::Unknown, "catching_up"));
    assert_eq!(probe.run_jobs(), 1);
    for _ in 0..10 {
        assert_eq!(probe.ask(), (Activity::Idle, "no_turn_evidence_ready"));
    }
    assert_eq!(probe.counts(), (1, 1), "no source: one fold, one worker");

    let pending = BindingTarget::Pending {
        payload_session_id: "parent".into(),
        payload_transcript_path: root.path().join("later.jsonl"),
    };
    let pending = Probe::new(
        ShadowProvider::Claude,
        vec![event(1, SESSION, ShadowProvider::Claude, record(pending))],
    );
    let (broken, path) = claude(&root.path().join("broken"), &[prompt("a")]);
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "not json"
    )
    .unwrap();
    let (large, path) = claude(&root.path().join("large"), &[]);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_TRANSCRIPT_BYTES + 1)
        .unwrap();
    for (probe, reason) in [
        (&pending, "binding_pending"),
        (&broken, "facts_halted"),
        (&large, "catching_up"),
    ] {
        assert_eq!(probe.settle(), (Activity::Unknown, reason));
        let before = probe.counts();
        for _ in 0..5 {
            assert_eq!(probe.ask(), (Activity::Unknown, reason));
        }
        assert_eq!(probe.counts(), before, "{reason}");
    }

    let (unreadable, _) = claude(&root.path().join("u"), &[]);
    *lock(&unreadable.fake.events) = Err("log unreadable".into());
    assert_eq!(
        unreadable.settle(),
        (Activity::Unknown, "binding_unreadable")
    );
    assert_eq!(unreadable.ask(), (Activity::Unknown, "binding_unreadable"));
    assert_eq!(unreadable.counts(), (1, 1));
    if let Outcome::Unreadable { retry_at } = &mut lock(&unreadable.watch).outcome {
        *retry_at = Instant::now();
    }
    assert_eq!(unreadable.ask(), (Activity::Unknown, "catching_up"));
    assert_eq!(unreadable.run_jobs(), 1);
    assert_eq!(unreadable.ask(), (Activity::Unknown, "binding_unreadable"));
    assert_eq!(unreadable.counts(), (2, 2), "one retry after the delay");
}

/// The watcher's session is part of the key even when the log did not move, and records of
/// another session never name this one's source.
#[test]
fn a_watcher_session_change_rebuilds_from_that_session_s_records() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old.jsonl");
    let current = root.path().join("current.jsonl");
    write(&old, &[prompt("old")]);
    write(&current, &[prompt("cur"), end()]);
    let (old_id, current_id) = (
        source_id_for("o", &old).unwrap(),
        source_id_for("c", &current).unwrap(),
    );
    let events = vec![
        bound(1, "past-session", &old_id, ShadowProvider::Claude),
        bound(2, SESSION, &current_id, ShadowProvider::Claude),
    ];
    let probe = Probe::new(ShadowProvider::Claude, events);
    assert_eq!(probe.settle(), (Activity::Idle, "closed"));
    let moved = Probe {
        target: Target::Bound("past-session".into()),
        ..probe
    };
    assert_eq!(moved.ask(), (Activity::Unknown, "catching_up"));
    assert_eq!(moved.settle(), (Activity::Busy, "open"));
    assert_eq!(moved.counts(), (2, 2));
}

/// A bind still pending holds the channel through a rejected late bind, and its resolution
/// installs the named source.
#[test]
fn pending_rejected_and_resolved_binds_are_read_one_seq_at_a_time() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("resolved.jsonl");
    write(&path, &[prompt("a")]);
    let source = source_id_for("parent", &path).unwrap();
    let pending = BindingTarget::Pending {
        payload_session_id: "parent".into(),
        payload_transcript_path: path.clone(),
    };
    let claude = ShadowProvider::Claude;
    let steps = [
        event(1, SESSION, claude, record(pending)),
        event(
            2,
            SESSION,
            claude,
            BindingRecord::Rejected {
                detail: "late".into(),
                closes: None,
            },
        ),
        event(
            3,
            SESSION,
            claude,
            BindingRecord::Resolved {
                resolves_seq: 1,
                source,
            },
        ),
    ];
    let probe = Probe::new(claude, Vec::new());
    let mut observed = Vec::new();
    for (seq, _) in steps.iter().enumerate() {
        *lock(&probe.fake.seq) = Ok(seq as u64 + 1);
        *lock(&probe.fake.events) = Ok(steps[..=seq].to_vec());
        observed.push(probe.settle());
    }
    use Activity::{Busy, Unknown};
    assert_eq!(
        observed,
        [
            (Unknown, "binding_pending"),
            (Unknown, "binding_pending"),
            (Busy, "open")
        ]
    );
}

/// A worker finishing after its key moved on installs nothing; the newer one does.
#[test]
fn a_superseded_worker_installs_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (probe, _) = claude(root.path(), &[prompt("a")]);
    assert_eq!(probe.ask(), (Activity::Unknown, "catching_up"));
    *lock(&probe.fake.seq) = Ok(2);
    assert_eq!(probe.ask(), (Activity::Unknown, "catching_up"));
    let mut jobs = std::mem::take(&mut *lock(&probe.fake.jobs));
    assert_eq!(jobs.len(), 2);
    let newer = jobs.pop().unwrap();
    jobs.pop().unwrap()();
    assert_eq!(
        probe.ask(),
        (Activity::Unknown, "catching_up"),
        "stale worker installed"
    );
    newer();
    assert_eq!(probe.ask(), (Activity::Busy, "open"));
}

/// A paused probe, its transcript, what overtakes its pane read and what the overtaking ask reads.
type Overtaken<'a> = (
    Probe,
    std::path::PathBuf,
    &'a dyn Fn(&Probe, &Path),
    (Activity, &'static str),
);

/// A pane read that a newer generation, a halt, newly read records or bytes still unread overtake
/// answers unknown, never the idle it saw.
#[test]
fn a_pane_read_overtaken_by_a_new_generation_or_read_answers_unknown() {
    let root = tempfile::tempdir().unwrap();
    let dir = |name: &str| root.path().join(name);
    let rows = [summary()];
    let rebind = |probe: &Probe, _: &Path| *lock(&probe.fake.seq) = Ok(9);
    let opened = |_: &Probe, path: &Path| write(path, &[prompt("a")]);
    let broken = |_: &Probe, path: &Path| {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{{\"type\":\"user\"").unwrap();
    };
    let big = json!({"type":"summary","summary":"x".repeat(CHUNK_BUDGET as usize + 1024)});
    let past_budget = |_: &Probe, path: &Path| write(path, &[big.clone(), prompt("a")]);
    let unterminated = |_: &Probe, path: &Path| {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        write!(file, "{}", prompt("a")).unwrap();
    };
    let cases: [Overtaken; 6] = [
        {
            let (probe, path) = claude(&dir("rebound"), &rows);
            (probe, path, &rebind, (Activity::Unknown, "catching_up"))
        },
        {
            let probe = Probe::new(ShadowProvider::Claude, Vec::new());
            (
                probe,
                dir("none"),
                &rebind,
                (Activity::Unknown, "catching_up"),
            )
        },
        {
            let (probe, path) = claude(&dir("opened"), &rows);
            (probe, path, &opened, (Activity::Busy, "open"))
        },
        {
            let (probe, path) = claude(&dir("broken"), &rows);
            (probe, path, &broken, (Activity::Unknown, "facts_halted"))
        },
        {
            let (probe, path) = claude(&dir("past-budget"), &rows);
            (
                probe,
                path,
                &past_budget,
                (Activity::Unknown, "catching_up"),
            )
        },
        {
            let (probe, path) = claude(&dir("unterminated"), &rows);
            (
                probe,
                path,
                &unterminated,
                (Activity::Unknown, "catching_up"),
            )
        },
    ];
    let (mut observed, mut expected) = (Vec::new(), Vec::new());
    for (probe, path, overtake, later) in &cases {
        assert_eq!(probe.settle(), (Activity::Idle, "no_turn_evidence_ready"));
        let (started, entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        *lock(&probe.fake.pause) = Some((started, released));
        let (overtaking, paused) = std::thread::scope(|scope| {
            // Owned here so a failed step drops it and frees the paused read.
            let release = release;
            let first = scope.spawn(|| probe.ask());
            entered.recv().unwrap();
            overtake(probe, path);
            let overtaking = probe.ask();
            release.send(()).unwrap();
            (overtaking, first.join().unwrap())
        });
        let name = path
            .strip_prefix(root.path())
            .unwrap()
            .display()
            .to_string();
        observed.push((name.clone(), overtaking, paused));
        expected.push((name, *later, (Activity::Unknown, "superseded")));
    }
    assert_eq!(observed, expected);
}

/// A supervisor publishes a judgment only while the watch still holds what it read: a later poll
/// that read new records, or one that only found bytes left unread, voids it.
#[test]
fn a_reading_publishes_only_while_its_watch_is_unmoved() {
    let root = tempfile::tempdir().unwrap();
    let (probe, path) = claude(root.path(), &[summary()]);
    assert_eq!(probe.settle(), (Activity::Idle, "no_turn_evidence_ready"));
    let ports: Arc<dyn Ports> = probe.fake.clone();
    let read = || {
        let (observed, stamp) = judge(
            &probe.watch,
            &ports,
            probe.provider,
            CHANNEL,
            probe.target.clone(),
        );
        let watch = Some(probe.watch.clone());
        Reading {
            observed,
            stamp,
            watch,
            unbound: None,
        }
    };
    let fresh = read();
    assert_eq!(fresh.session(), Some(SESSION));
    assert_eq!(publish(&fresh, || "published"), Some("published"));
    let stale = read();
    write(&path, &[prompt("a")]);
    assert_eq!(probe.ask(), (Activity::Busy, "open"));
    assert_eq!(publish(&stale, || "published"), None);
    let stale = read();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(file, "{}", end()).unwrap();
    assert_eq!(probe.ask(), (Activity::Unknown, "catching_up"));
    assert_eq!(publish(&stale, || "published"), None);
}

/// A bound transcript gone missing reads unknown beside a ready pane, and the same file put back
/// is read again on the unreadable retry.
#[test]
fn a_bound_transcript_gone_missing_reads_unknown_until_it_is_back() {
    let root = tempfile::tempdir().unwrap();
    let (probe, path) = claude(root.path(), &[prompt("a")]);
    let aside = root.path().join("aside.jsonl");
    std::fs::rename(&path, &aside).unwrap();
    assert_eq!(probe.settle(), (Activity::Unknown, "binding_unreadable"));
    assert_eq!(probe.ask(), (Activity::Unknown, "binding_unreadable"));
    assert_eq!(probe.counts(), (1, 1), "no retry before the delay");
    std::fs::rename(&aside, &path).unwrap();
    if let Outcome::Unreadable { retry_at } = &mut lock(&probe.watch).outcome {
        *retry_at = Instant::now();
    }
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    assert_eq!(probe.counts(), (2, 2), "one retry reads the file again");
}

/// An untyped output block at an effect point resumes on a worker and keeps the carried turn; a
/// broken record halts for good.
#[test]
fn effect_point_errors_resume_typed_blocks_and_halt_broken_json() {
    let root = tempfile::tempdir().unwrap();
    let blocked = json!({"type":"response_item","payload":{"type":"unknown_item","id":"x"}});
    let (probe, path) = codex_probe(root.path(), &[codex("task_started", "A")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    write(&path, &[blocked.clone(), codex("turn_aborted", "B")]);
    assert_eq!(probe.ask(), (Activity::Unknown, "facts_resumed"));
    assert_eq!(probe.settle(), (Activity::Unknown, "facts_resumed"));
    write(&path, &[codex("turn_aborted", "A")]);
    assert_eq!(probe.ask(), (Activity::Idle, "closed"));
    let before = probe.counts();
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{{\"type\":\"event_msg\""
    )
    .unwrap();
    write(&path, &[codex("task_complete", "A")]);
    for _ in 0..3 {
        assert_eq!(probe.ask(), (Activity::Unknown, "facts_halted"));
    }
    assert_eq!(
        probe.counts().1,
        before.1,
        "a halted source never rebuilds for the same key"
    );
}

/// Keeps this thread's binding-log root for the life of a fixture.
pub(crate) struct BindingRoot(Option<std::path::PathBuf>);

impl BindingRoot {
    pub(crate) fn enter(root: &Path) -> Self {
        use crate::services::tui_prompt_dedupe::binding_events as p5;
        let saved = p5::test_root();
        p5::set_test_root(Some(root));
        Self(saved)
    }
}

impl Drop for BindingRoot {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(self.0.as_deref());
    }
}

static READY_PANES: LazyLock<Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(Default::default);

/// Live probes read this tmux session's pane as passing the final send check while it lives.
pub(crate) struct ReadyPane(String);

impl ReadyPane {
    pub(crate) fn mark(session: &str) -> Self {
        lock(&READY_PANES).insert(session.into());
        Self(session.into())
    }
}

impl Drop for ReadyPane {
    fn drop(&mut self) {
        lock(&READY_PANES).remove(&self.0);
    }
}

pub(super) fn pane_ready_for_tests(session: &str) -> bool {
    lock(&READY_PANES).contains(session)
}

/// A confirmed turn-mode channel whose live watcher and one-record binding log name `path`, as
/// the binding writer leaves them; turn mode lasts while the returned guard lives.
pub(crate) fn bind_turn_mode_transcript(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    session: &str,
    path: &Path,
) -> crate::services::tui_o::turn_mode::TestConfirmation {
    use crate::services::discord::TmuxWatcherHandle;
    shared.tmux_watchers.insert(
        ChannelId::new(channel),
        TmuxWatcherHandle {
            tmux_session_name: session.into(),
            output_path: path.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
        },
    );
    log_turn_source(provider, channel, session, "turn-presence-test", path);
    crate::services::tui_o::turn_mode::TestConfirmation::new(channel)
}

/// The channel's binding log as one record of execution `nonce` naming `path` as `session`'s source.
pub(crate) fn log_turn_source(
    provider: &ProviderKind,
    channel: u64,
    session: &str,
    nonce: &str,
    path: &Path,
) {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let source = source_id_for("parent", path).unwrap();
    let now = chrono::Utc::now();
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: channel,
        provider: provider.as_str().into(),
        tmux_session: session.into(),
        execution_nonce: Some(nonce.into()),
        old: None,
        new: p5::BindingTarget::Source(source),
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: now,
        },
        committed_at: now,
    };
    let root = p5::test_root().unwrap().join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&root).unwrap();
    let line = format!("{}\n", serde_json::to_string(&event).unwrap());
    std::fs::write(root.join(format!("{channel}.log")), line).unwrap();
}

/// The reason effect points read once the channel's rebuild finished.
pub(crate) async fn settled_reason(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: u64,
) -> &'static str {
    for _ in 0..500 {
        let observed = activity_now(shared, provider, ChannelId::new(channel)).await;
        if observed.reason != "catching_up" {
            return observed.reason;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("channel {channel} never finished catching up");
}

/// Waits for the live binding-log worker, then returns a fresh effect-point reading.
pub(crate) async fn companion_settled_activity(shared: &Arc<SharedData>, channel: u64) -> Observed {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let observed =
                activity_now(shared, &ProviderKind::Codex, ChannelId::new(channel)).await;
            if !matches!(observed.reason, "catching_up" | "facts_resumed") {
                return observed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual Codex binding-log worker did not finish")
}

/// Advances the production watch's monotonic age without a ten-minute wall-clock sleep.
pub(crate) fn companion_age_open_watch(channel: u64, age: Duration) {
    let watch = lock(&WATCHES)
        .get(&channel)
        .cloned()
        .expect("the actual activity reader must have installed its watch first");
    let mut watch = lock(&watch);
    assert!(matches!(
        watch.outcome,
        Outcome::Source { .. } | Outcome::Tail(_)
    ));
    watch.grew_at = Instant::now() - age;
}

/// A live watcher over the caller's existing log; this never creates or rewrites binding records.
pub(crate) fn companion_attach_log_watcher(
    shared: &SharedData,
    channel: u64,
    tmux: &str,
    path: &Path,
) {
    shared.tmux_watchers.insert(
        ChannelId::new(channel),
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux.into(),
            output_path: path.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
        },
    );
}

/// Lets non-Discord host tests inspect their exact settled log through the live activity reader.
pub(crate) async fn companion_activity_for_log_for_tests(
    channel: u64,
    tmux: &str,
    path: &Path,
) -> (&'static str, bool) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _confirmation = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    companion_attach_log_watcher(&shared, channel, tmux, path);
    let observed = companion_settled_activity(&shared, channel).await;
    (observed.reason, observed.activity == Activity::Idle)
}

/// The actual Unbound collector's verdict before its public reason is aggregated.
pub(in crate::services::discord) async fn unbound_verdict_for_tests(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<String, String> {
    super::unbound::live::tests::verdict_for_tests(shared, provider, channel).await
}

async fn companion_next_incarnation_keeps_terminal_meaning(channel: u64, unfinished: bool) {
    use crate::services::discord::router::message_handler::codex_resume_companion_queue_tests::Fixture;
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
    let mut fixture = Fixture::new(channel, true).await;
    fixture.recover();
    fixture.attach();
    fixture.assert_activity(Activity::Idle, "closed").await;
    let previous = fixture.context.clone();
    if unfinished {
        let claim = p5::codex::Claim {
            session_id: previous.expected_native_session_id.clone().unwrap(),
            path: Some(fixture.native.clone()),
            evidence: p5::codex::ClaimEvidence::NativeHook {
                event: "session_start".into(),
                source: Some("clear".into()),
                first_prompt_digest: None,
            },
        };
        let fold =
            p5::codex::commit_claim(&previous, &claim, p5::codex::Decision::Pending, || Ok(()))
                .unwrap();
        assert_eq!(
            fold.pending.len(),
            1,
            "the control must leave a truly unterminated old Pending"
        );
    }
    let mut next = previous.clone();
    next.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    let id = "019e660d-4859-7522-9cee-8ba7c4e7c744";
    next.expected_native_session_id = Some(id.into());
    next.created_at = chrono::Utc::now();
    let native = next
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{id}.jsonl"));
    let metadata = json!({"type":"session_meta","timestamp":next.created_at.to_rfc3339(),
        "payload":{"id":id,"timestamp":next.created_at.to_rfc3339(),"cwd":fixture.root,
            "source":"cli","originator":"codex-tui"}});
    let rows = [
        metadata,
        codex("task_started", "N2"),
        codex("task_complete", "N2"),
    ];
    std::fs::write(
        &native,
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    PreparedIncarnation::create(next.clone()).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&next.tmux_session, "spawn_nonce"),
        &next.execution_nonce,
    )
    .unwrap();
    dedupe::set_codex_delivery_permission_for_tests(
        &next,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    assert!(dedupe::codex_verified::record_executed_resume(&next, id, &native, 0).unwrap());
    let current = p5::codex::read_ownership(&next).unwrap();
    assert!(current.pending.is_empty() && !current.conflicted);
    assert_eq!(current.verified.unwrap().source.session_id, id);
    let events = ChannelBindingLog::new(channel, ShadowProvider::Codex)
        .binding_events_since(channel, 0)
        .unwrap();
    for seq in fixture.pending {
        assert!(
            events.iter().any(|event| matches!(&event.record,
            BindingRecord::Rejected { closes: Some(closed), .. } if *closed == seq)),
            "historical source denial must retain exact terminal {seq}"
        );
    }
    fixture.context = next;
    fixture.native = native;
    fixture.attach();
    if unfinished {
        fixture
            .assert_activity(Activity::Unknown, "binding_pending")
            .await;
        fixture.assert_automatic_held().await;
    } else {
        fixture.admit_once().await;
    }
}

#[test]
fn codex_historical_terminals_remain_closed_in_next_incarnation() {
    let support = crate::services::discord::tmux::watcher_install_fixture::isolated;
    if !support(
        module_path!(),
        "codex_historical_terminals_remain_closed_in_next_incarnation",
    ) {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(companion_next_incarnation_keeps_terminal_meaning(
            7_061_741, false,
        ));
}

#[test]
fn codex_unterminated_historical_pending_keeps_next_incarnation_queue_held() {
    let support = crate::services::discord::tmux::watcher_install_fixture::isolated;
    if !support(
        module_path!(),
        "codex_unterminated_historical_pending_keeps_next_incarnation_queue_held",
    ) {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(companion_next_incarnation_keeps_terminal_meaning(
            7_061_751, true,
        ));
}

/// The boot and full-reconcile loop arms the backstop for a turn-mode channel it could not start,
/// and leaves an ordinary channel as it was.
#[tokio::test(flavor = "current_thread")]
async fn the_boot_kickoff_loop_arms_only_turn_mode_channels_it_could_not_start() {
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let (turn_mode, ordinary) = (6_100_011_u64, 6_100_012_u64);
    let path = root.path().join("parent.jsonl");
    write(&path, &[prompt("a")]);
    let _confirmed = bind_turn_mode_transcript(&shared, &provider, turn_mode, "boot-parent", &path);
    assert_eq!(settled_reason(&shared, &provider, turn_mode).await, "open");
    for (channel, id) in [(turn_mode, 1), (ordinary, 2)] {
        let channel = ChannelId::new(channel);
        let queued = crate::services::turn_orchestrator::Intervention {
            author_id: poise::serenity_prelude::UserId::new(7),
            author_is_bot: false,
            message_id: poise::serenity_prelude::MessageId::new(6_100_020 + id),
            queued_generation: 1,
            source_message_ids: Vec::new(),
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: format!("queued {id}"),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let context =
            crate::services::discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![queued], context)
            .await;
    }
    // Neither channel routes, so no step inside the kickoff arms either one.
    shared.settings.write().await.allowed_channel_ids = vec![6_100_019];
    let mock = discord_mock::DiscordMockState::new();
    let (proxy, gateway, _server) = discord_mock::start(mock).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let started = crate::services::discord::kickoff_idle_queues(&ctx, &shared, "", &provider).await;
    let armed = |channel: u64| {
        let hooks = &shared.restart.deferred_hook_channels;
        hooks.contains_key(&ChannelId::new(channel))
    };
    assert_eq!(
        (started, armed(turn_mode), armed(ordinary)),
        (0, true, false)
    );
    for channel in [turn_mode, ordinary] {
        let snapshot =
            crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(channel)).await;
        assert_eq!(snapshot.intervention_queue.len(), 1, "channel {channel}");
    }
}

/// A replaced or shrunk transcript halts the installed reader for good.
#[test]
fn a_replaced_or_shrunk_transcript_reads_facts_halted() {
    for replace in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (probe, path) = claude(root.path(), &[prompt("a")]);
        assert_eq!(probe.settle(), (Activity::Busy, "open"));
        match replace {
            true => {
                std::fs::remove_file(&path).unwrap();
                write(&path, &[prompt("a"), end()]);
            }
            false => std::fs::write(&path, "").unwrap(),
        }
        for _ in 0..3 {
            assert_eq!(
                probe.ask(),
                (Activity::Unknown, "facts_halted"),
                "replace={replace}"
            );
        }
    }
}

/// A cut-short record may hide an opener: on a first read, a restart's reread and after an earlier
/// closed turn, the channel reads `facts_halted` and never rebuilds for the same key.
#[test]
fn broken_records_read_facts_halted_on_first_read_restart_and_after_a_closed_turn() {
    let cut = |row: serde_json::Value| {
        let row = row.to_string();
        row[..row.len() - 3].to_string()
    };
    let assistant = |id: &str| {
        json!({"type":"response_item","payload":{"type":"message",
        "role":"assistant","id":id,"content":[{"type":"output_text","text":"done"}]}})
    };
    let sequences = [
        vec![
            cut(codex("task_started", "A")),
            codex("turn_aborted", "B").to_string(),
            assistant("A").to_string(),
        ],
        vec![
            codex("task_started", "A").to_string(),
            cut(codex("task_started", "C")),
            codex("task_complete", "A").to_string(),
            assistant("C").to_string(),
        ],
    ];
    let mut observed = Vec::new();
    for sequence in &sequences {
        for earlier in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let prior = match earlier {
                true => vec![codex("task_started", "Z"), codex("task_complete", "Z")],
                false => Vec::new(),
            };
            let (probe, path) = codex_probe(root.path(), &prior);
            let before = probe.settle();
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            sequence
                .iter()
                .for_each(|line| writeln!(file, "{line}").unwrap());
            let first = probe.ask();
            let spawns = probe.counts().1;
            let again = probe.ask();
            let restarted = Probe::new(
                ShadowProvider::Codex,
                lock(&probe.fake.events).clone().unwrap(),
            );
            observed.push((
                before,
                first,
                again,
                probe.counts().1 == spawns,
                restarted.settle(),
            ));
        }
    }
    use Activity::{Idle, Unknown};
    let halted = (Unknown, "facts_halted");
    let expected = |before| (before, halted, halted, true, halted);
    assert_eq!(
        observed,
        [
            expected((Unknown, "no_turn_evidence")),
            expected((Idle, "closed")),
            expected((Unknown, "no_turn_evidence")),
            expected((Idle, "closed")),
        ]
    );
}

/// A channel outside turn mode never reaches the probe from a deliver, a start check or an
/// automatic dequeue, even with a watcher and a bound transcript.
#[tokio::test(flavor = "current_thread")]
async fn a_channel_outside_turn_mode_never_reaches_the_probe() {
    use crate::services::discord::health::{
        HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,
        external_turn_hold_for_start, register_inject_runtime, start_without_gateway,
    };
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let channel = 6_100_041_u64;
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &[channel], None).await;
    let provider = ProviderKind::Claude;
    let path = root.path().join("parent.jsonl");
    write(&path, &[prompt("a")]);
    drop(bind_turn_mode_transcript(
        &shared,
        &provider,
        channel,
        "outside-parent",
        &path,
    ));
    let _starts = start_without_gateway(channel);
    let request = HumanInputRequest {
        channel_id: ChannelId::new(channel),
        provider: provider.clone(),
        text: "status?".into(),
        author_id: 200,
        source: "imessage".into(),
        metadata: None,
        channel_name_hint: None,
    };
    let delivered = deliver_human_input(&registry, request).await;
    assert!(
        matches!(delivered, Ok(HumanInputDelivery::Started { .. })),
        "{delivered:?}"
    );
    let held = external_turn_hold_for_start(Some(&registry), &provider, channel).await;
    let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
        &shared,
        &provider,
        ChannelId::new(channel),
    )
    .await;
    assert_eq!(held, None);
    assert!(taken.intervention.is_none());
    assert!(!lock(&WATCHES).contains_key(&channel), "the probe ran");
}

fn publish<R>(reading: &Reading, run: impl FnOnce() -> R) -> Option<R> {
    futures::executor::block_on(reading.publish_if_current(run))
}
pub(crate) fn small_tail_limits(channel: u64) -> impl Drop {
    tail::overrides::Guard::new(channel, 1, 4096, 4096, 37)
}
#[path = "activity_tests/effect_tests.rs"]
mod effects;

type Reads = Vec<(u64, u64, bool)>;
type ReadStages = Arc<Mutex<Vec<(&'static str, Reads)>>>;
static READ_TRACES: LazyLock<Mutex<HashMap<u64, ReadStages>>> = LazyLock::new(Default::default);
struct ReadTrace(u64, ReadStages);
impl ReadTrace {
    fn new(channel: u64) -> Self {
        let stages: ReadStages = Arc::default();
        assert!(lock(&READ_TRACES).insert(channel, stages.clone()).is_none());
        Self(channel, stages)
    }
    fn take(&self) -> Vec<(&'static str, Reads)> {
        std::mem::take(&mut *lock(&self.1))
    }
}
impl Drop for ReadTrace {
    fn drop(&mut self) {
        lock(&READ_TRACES).remove(&self.0);
    }
}
pub(super) fn record_io(channel: u64, stage: &'static str, reads: Reads) {
    if let Some(stages) = lock(&READ_TRACES).get(&channel) {
        #[cfg(target_os = "linux")]
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        #[cfg(not(target_os = "linux"))]
        let tid = format!("{:?}", std::thread::current().id());
        eprintln!("S1_IO_WORKER channel={channel} stage={stage} tid={tid}");
        lock(stages).push((stage, reads));
    }
}
async fn installed(watch: &Mutex<Watch>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !matches!(lock(watch).outcome, Outcome::Rebuilding) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn s1_actual_tail_initial_resume_and_poll_have_bounded_hash_free_reads() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let path = root.path().join("parent.jsonl");
    let meta = json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path(),"padding":"x".repeat(180)}});
    for _ in 0..160 {
        write(&path, std::slice::from_ref(&meta));
    }
    let start = codex("task_started", "A");
    let close = codex("task_complete", "A");
    let blocked = json!({"type":"response_item","payload":{"type":"unknown_item"}});
    write(&path, &[start, blocked.clone(), close.clone()]);
    let snapshot = std::fs::metadata(&path).unwrap().len();
    let tail = 4096;
    let start = snapshot - tail;
    assert!(start > 4096);
    let channel = 69_183_800;
    let _limits = tail::overrides::Guard::new(channel, 1, tail, 4096, 4096);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let trace = ReadTrace::new(channel);
    let _turn = bind_turn_mode_transcript(&shared, &ProviderKind::Codex, channel, "s1-io", &path);
    let _ = activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel)).await;
    let watch = lock(&WATCHES).get(&channel).unwrap().clone();
    installed(&watch).await;
    assert!(matches!(lock(&watch).outcome, Outcome::Tail(_)));
    for _ in 0..8 {
        if activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
            .await
            .activity
            == Activity::Idle
        {
            break;
        }
        installed(&watch).await;
    }
    assert_eq!(
        activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
            .await
            .activity,
        Activity::Idle
    );
    let before = match &lock(&watch).outcome {
        Outcome::Tail(reader) => reader.diagnostics().2,
        _ => panic!("Tail mode lost"),
    };
    write(&path, &[blocked, close]);
    assert_eq!(
        activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
            .await
            .reason,
        "facts_resumed"
    );
    installed(&watch).await;
    assert_eq!(
        match &lock(&watch).outcome {
            Outcome::Tail(reader) => reader.diagnostics().2,
            _ => panic!("resume changed mode"),
        },
        before,
        "actual worker refilled bootstrap budget"
    );
    assert_eq!(
        activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
            .await
            .activity,
        Activity::Idle
    );
    let eof = std::fs::metadata(&path).unwrap().len();
    let stages = trace.take();
    for required in ["initial", "resume", "effect"] {
        let reads: Vec<_> = stages
            .iter()
            .filter(|(stage, _)| *stage == required)
            .flat_map(|(_, r)| r)
            .collect();
        assert!(
            reads.iter().any(|(a, b, _)| b > a),
            "{required}: real worker trace had no positive read: {stages:?}"
        );
        assert!(
            reads.iter().all(|(_, _, hash)| !*hash),
            "{required}: actual Tail hashed a prefix"
        );
        assert!(
            reads
                .iter()
                .filter(|(a, b, _)| b > a)
                .all(|(a, b, _)| *a >= start - 4096 && *b <= eof),
            "{required}: actual read outside tail/guard/partial bounds: {reads:?}"
        );
    }
    eprintln!(
        "S1_ACTIVITY_IO window={start} snapshot={snapshot} eof={eof} stages={stages:?} skip=0"
    );
    let channel = 69_183_801;
    let _limits = tail::overrides::Guard::new(channel, eof, 4096, 4096, 4096);
    let full = ReadTrace::new(channel);
    let _turn =
        bind_turn_mode_transcript(&shared, &ProviderKind::Codex, channel, "s1-full-io", &path);
    assert_eq!(
        settled_reason(&shared, &ProviderKind::Codex, channel).await,
        "closed"
    );
    assert!(
        full.take()
            .iter()
            .flat_map(|(_, r)| r)
            .any(|(a, b, hash)| *hash && b > a),
        "same actual worker boundary never observed Full prefix hashing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1_read_now_unbound_invalidates_the_same_bound_watch_before_core_await() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(69_183_600);
    let probe = Probe::new(ShadowProvider::Claude, Vec::new());
    let ports: Arc<dyn Ports> = probe.fake.clone();
    let (observed, stamp) = judge(
        &probe.watch,
        &ports,
        probe.provider,
        channel.get(),
        probe.target.clone(),
    );
    let old = Reading {
        observed,
        stamp,
        watch: Some(probe.watch.clone()),
        unbound: None,
    };
    assert_eq!(probe.fake.spawns.load(Ordering::SeqCst), 1);
    lock(&WATCHES).insert(channel.get(), probe.watch.clone());
    let core = shared.core.lock().await;
    let read = reading_now(&shared, &ProviderKind::Claude, channel);
    tokio::pin!(read);
    tokio::select! {_=&mut read=>panic!("read escaped held core"),_=tokio::time::sleep(Duration::from_millis(20))=>{}}
    assert!(
        lock(&probe.watch).key.is_none(),
        "Unbound allocated another watch or waited before invalidation"
    );
    assert!(old.publish_if_current(|| ()).await.is_none());
    drop(core);
    let unbound = read.await;
    assert!(unbound.session().is_none() && unbound.source().is_none());
    let (observed, stamp) = judge(
        &probe.watch,
        &ports,
        probe.provider,
        channel.get(),
        probe.target.clone(),
    );
    let next = Reading {
        observed,
        stamp,
        watch: Some(probe.watch.clone()),
        unbound: None,
    };
    let jobs = std::mem::take(&mut *lock(&probe.fake.jobs));
    assert_eq!(jobs.len(), 2);
    let mut jobs = jobs.into_iter();
    jobs.next().unwrap()();
    assert!(
        matches!(lock(&probe.watch).outcome, Outcome::Rebuilding),
        "old Bound worker installed after Unbound and new Bound"
    );
    assert!(old.publish_if_current(|| ()).await.is_none());
    assert!(unbound.publish_if_current(|| ()).await.is_none());
    assert!(next.publish_if_current(|| ()).await.is_some());
    jobs.next().unwrap()();
}

pub(crate) fn exercise_tail_switch_entry_order(expected: bool, supervisor_first: bool) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let _runtime = crate::config::TestRuntimeRootGuard::new();
            let root = tempfile::tempdir().unwrap();
            let _binding = BindingRoot::enter(root.path());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            if supervisor_first {
                super::super::supervisor::spawn(shared.clone(), provider.clone());
            }
            let unbound = ChannelId::new(69_183_700);
            let watch = lock(&WATCHES).entry(unbound.get()).or_default().clone();
            let generation = lock(&watch).generation;
            let reading = reading_now(&shared, &provider, unbound).await;
            assert_eq!(
                lock(&watch).generation,
                generation + 1,
                "Tail off bypassed Unbound invalidation"
            );
            assert!(reading.session().is_none() && reading.source().is_none());
            if !supervisor_first {
                super::super::supervisor::spawn(shared.clone(), provider.clone());
            }
            assert_eq!(tail::enabled(), expected);
            let channel = 69_183_701;
            let _limits = tail::overrides::Guard::new(channel, 1, 4096, 4096, 59);
            let path = root.path().join("cold.jsonl");
            write(&path, &[prompt("A"), end()]);
            let _turn =
                bind_turn_mode_transcript(&shared, &provider, channel, "s1-switch-cold", &path);
            assert_eq!(
                settled_reason(&shared, &provider, channel).await,
                if expected {
                    "closed"
                } else {
                    "transcript_too_large"
                }
            );
            let channel = 69_183_702;
            let path = root.path().join("full.jsonl");
            write(&path, &[prompt("B"), end()]);
            let size = std::fs::metadata(&path).unwrap().len();
            let _limits = tail::overrides::Guard::new(channel, size, 4096, 4096, 59);
            let _turn =
                bind_turn_mode_transcript(&shared, &provider, channel, "s1-switch-full", &path);
            assert_eq!(settled_reason(&shared, &provider, channel).await, "closed");
            write(&path, &[end()]);
            assert!(std::fs::metadata(&path).unwrap().len() > size);
            assert_eq!(
                settled_reason(&shared, &provider, channel).await,
                "closed",
                "Tail off stopped installed Full growth"
            );
        });
}

pub(crate) struct HandoffMutation(tail::tests::HandoffHook);
impl HandoffMutation {
    pub(crate) fn new(
        channel: u64,
        bootstrap: Option<Box<dyn FnOnce() + Send>>,
        resume: Option<Box<dyn FnOnce() + Send>>,
    ) -> Self {
        Self(tail::tests::HandoffHook::new(channel, bootstrap, resume))
    }
    pub(crate) fn calls(&self) -> usize {
        self.0.calls()
    }
}
