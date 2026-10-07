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

const CHANNEL: u64 = 6_100_001;
const SESSION: &str = "presence-parent";

struct Fake {
    seq: Mutex<Result<u64, String>>,
    events: Mutex<Result<Vec<BindingEvent>, String>>,
    present: AtomicBool,
    ready: AtomicBool,
    busy: AtomicBool,
    folds: AtomicUsize,
    jobs: Mutex<Vec<Job>>,
    spawns: AtomicUsize,
    /// The next pane read reports it started, then waits until released.
    pause: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Ports for Fake {
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
            folds: AtomicUsize::new(0),
            jobs: Mutex::new(Vec::new()),
            spawns: AtomicUsize::new(0),
            pause: Mutex::new(None),
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
        ("no watcher, no session", (Idle, "no_session")),
        ("no watcher, session gone", (Idle, "no_session")),
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
        ("too large", (Unknown, "transcript_too_large")),
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
        (&large, "transcript_too_large"),
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
    use crate::services::tui_prompt_dedupe::binding_events as p5;
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
    let source = source_id_for("parent", path).unwrap();
    let now = chrono::Utc::now();
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: channel,
        provider: provider.as_str().into(),
        tmux_session: session.into(),
        execution_nonce: Some("turn-presence-test".into()),
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
    crate::services::tui_o::turn_mode::TestConfirmation::new(channel)
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
