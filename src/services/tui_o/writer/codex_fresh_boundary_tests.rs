//! A Codex source an Unknown bind held is owed from 0 only when its own fresh execution proves it.

use std::time::SystemTime;

use super::*;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::writer::rotation::{Owe, Sources};
use crate::services::tui_o::writer::suspension::Suspension;
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, PreparedIncarnation, stable_host_identity,
};

const PROMPT: &str = "  e2e codex prompt\n첫 입력  ";
const ANSWERS: [&str; 4] = [
    "e2e answer one",
    "e2e answer two",
    "e2e answer three",
    "e2e answer four",
];
const OPERATOR: &str = "\"operator\":\"codex_fresh_launch\"";

type FreshWriter = ChannelWriter<FakePort, Arc<FakeLease>, Arc<AlarmRouter>>;

struct Env {
    _env: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
    _host: crate::config::TestEnvVarGuard,
    _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

fn env() -> Env {
    let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let env =
        crate::services::tui_prompt_dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new("codex-fresh-boundary-host"),
    );
    Env {
        _env: env,
        _host: host,
        _lock: lock,
    }
}

fn digest(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn json_line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

fn meta(session: &str, created: chrono::DateTime<Utc>) -> Vec<u8> {
    let at = created.to_rfc3339();
    json_line(serde_json::json!({"type": "session_meta", "timestamp": at,
        "payload": {"id": session, "timestamp": at, "cwd": "/", "source": "cli",
            "originator": "codex_cli_rs"}}))
}

fn message(role: &str, id: &str, text: &str) -> Vec<u8> {
    let kind = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    json_line(
        serde_json::json!({"type": "response_item", "timestamp": Utc::now().to_rfc3339(),
        "payload": {"type": "message", "role": role, "id": id,
            "content": [{"type": kind, "text": text}]}}),
    )
}

/// Four turns, the first opened by the launch prompt, each answered once.
fn turns() -> Vec<u8> {
    let mut body = Vec::new();
    for (index, answer) in ANSWERS.iter().enumerate() {
        let prompt = if index == 0 {
            PROMPT.to_string()
        } else {
            format!("turn {index}")
        };
        body.extend(message("user", &format!("user-{index}"), &prompt));
        body.extend(message("assistant", &format!("answer-{index}"), answer));
    }
    body
}

fn rollout(root: &Path, label: &str, session: &str, bytes: &[u8]) -> (PathBuf, SourceId) {
    let path = root.join(format!("rollout-{label}-{session}.jsonl"));
    std::fs::write(&path, bytes).unwrap();
    let source = source_id_for(session, &path).unwrap();
    (path, source)
}

fn session() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn launched(root: &Path, nonce: &str, change: impl FnOnce(&mut BindingContext)) -> PathBuf {
    let mut context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now() - chrono::TimeDelta::seconds(5),
        execution_nonce: nonce.into(),
        tmux_session: "tmux".into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: stable_host_identity(),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.to_path_buf()),
        first_prompt_digest: Some(digest(PROMPT)),
        source_policy: Some("shadow".into()),
    };
    change(&mut context);
    PreparedIncarnation::create(context).unwrap().path
}

fn codex_bind(
    seq: u64,
    old: Option<&SourceId>,
    new: &SourceId,
    cause: BindingCause,
    nonce: &str,
) -> BindingEvent {
    let mut event = hop_at(seq, "tmux", old, new, cause, "");
    event.provider = ShadowProvider::Codex;
    event.execution_nonce = nonce.into();
    event
}

fn ledger_text(harness: &Harness) -> String {
    let path = (harness._runtime.path().join("o_store"))
        .join(CHANNEL.to_string())
        .join("ledger.jsonl");
    std::fs::read_to_string(path).unwrap()
}

fn resolutions(harness: &Harness) -> usize {
    ledger_text(harness)
        .matches("\"boundary_resolved\"")
        .count()
}

fn boundary(harness: &Harness, source: &SourceId) -> Boundary {
    harness
        .channel()
        .rotation()
        .unwrap()
        .link(source)
        .unwrap()
        .boundary
        .clone()
}

fn held() -> Boundary {
    Boundary::Pending {
        candidates: vec![0],
    }
}

/// A writer reporting to its own health, with the Sources, deriver and collector of one process.
struct Engine {
    health: Arc<AlarmHealth>,
    writer: FreshWriter,
    sources: Sources<FakeBindings>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
}

impl Engine {
    fn start(harness: &Harness, bindings: &Arc<FakeBindings>, provider: ShadowProvider) -> Self {
        Self::start_with(harness, bindings, provider, Suspension::default())
    }

    fn start_with(
        harness: &Harness,
        bindings: &Arc<FakeBindings>,
        provider: ShadowProvider,
        suspension: Suspension,
    ) -> Self {
        let health = Arc::new(AlarmHealth::default());
        let router = Arc::new(AlarmRouter::new(None, None, Arc::clone(&health)));
        let writer = ChannelWriter::new(
            harness.channel(),
            Arc::clone(&harness.gate),
            Arc::clone(&harness.port),
            Arc::clone(&harness.lease),
            router,
        );
        let sources =
            Sources::new(CHANNEL, provider, Arc::clone(bindings)).with_suspension(suspension);
        let deriver = UnitDeriver::new(CHANNEL, provider);
        Self {
            health,
            writer,
            sources,
            deriver,
            owed: VecDeque::new(),
        }
    }

    fn resume(&mut self) -> Result<(), WriterAlarm> {
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        self.sources
            .resume_at(writer, deriver, owed, SystemTime::now())?;
        self.sources.follow(writer)
    }

    fn capture(&mut self) {
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        self.sources.follow(writer).unwrap();
        self.sources.capture(writer, deriver, owed).unwrap();
    }

    async fn deliver(&mut self) {
        while let Some(item) = self.owed.pop_front() {
            assert_eq!(self.writer.deliver(&item).await, Step::Done, "{item:?}");
        }
    }

    fn pending_health(&self) -> bool {
        let reason = format!("tui_o:boundary_pending:{CHANNEL}");
        self.health
            .current_at(std::time::Instant::now())
            .contains(&reason)
    }
}

/// The disk the 10-27 incident left: an old source O, the fresh source F an Unknown bind held
/// with its four answers spooled, retired after a proven hop to N. The execution context of F
/// was unreadable while it was bound, so the held start came from the code as it was.
struct Ops {
    harness: Harness,
    bindings: Arc<FakeBindings>,
    root: PathBuf,
    fresh_path: PathBuf,
    fresh: SourceId,
    fresh_nonce: String,
    context: PathBuf,
}

impl Ops {
    fn new(
        change: impl FnOnce(&mut BindingContext),
        fresh_meta: Option<chrono::DateTime<Utc>>,
    ) -> Self {
        let mut old = None;
        let harness = Harness::build_channel(CHANNEL, |root| {
            let root = root.canonicalize().unwrap();
            let body = [
                meta(&session(), Utc::now()),
                message("assistant", "before", "before"),
            ];
            let (path, source) = rollout(&root, "old", &session(), &body.concat());
            let bytes = std::fs::read(&path).unwrap();
            old = Some(source.clone());
            vec![InitSource {
                source_id: source,
                delivery_start: bytes.len() as u64,
                prefix_hash: hex::encode(Sha256::digest(&bytes)),
            }]
        });
        let old = old.unwrap();
        let root = old.path.parent().unwrap().to_path_buf();
        let fresh_nonce = nonce();
        let context = launched(&root, &fresh_nonce, change);
        let id = session();
        let created = fresh_meta.unwrap_or_else(Utc::now);
        let (fresh_path, fresh) =
            rollout(&root, "fresh", &id, &[meta(&id, created), turns()].concat());
        let next_id = session();
        let (_, next) = rollout(&root, "next", &next_id, &meta(&next_id, Utc::now()));
        let bindings = Arc::new(FakeBindings::new());
        bindings.commit(codex_bind(1, None, &old, BindingCause::Startup, &nonce()));
        bindings.commit(codex_bind(
            2,
            Some(&old),
            &fresh,
            BindingCause::Unknown,
            &fresh_nonce,
        ));
        harness.gate.acquired();
        let ops = Self {
            harness,
            bindings,
            root,
            fresh_path,
            fresh,
            fresh_nonce,
            context,
        };
        ops.hidden(|| {
            let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Codex);
            engine.resume().unwrap();
            engine.capture();
            ops.bindings.commit(codex_bind(
                3,
                Some(&ops.fresh),
                &next,
                BindingCause::Startup,
                &nonce(),
            ));
            engine.capture();
        });
        let mut store = ops.harness.channel();
        let mut rotation = store.rotation().unwrap();
        let hop = rotation
            .successors
            .get_mut(&source_key(&ops.fresh))
            .unwrap();
        assert_eq!(hop.drain_to, Some(len(&ops.fresh_path)));
        hop.proof = Some(3);
        store.write_rotation(&rotation).unwrap();
        store.set_retired(&ops.fresh, true).unwrap();
        assert_eq!(rotation.successors[&source_key(&old)].proof, None);
        assert_eq!(boundary(&ops.harness, &ops.fresh), held());
        assert_eq!(resolutions(&ops.harness), 0);
        ops
    }

    /// Runs `run` while F's execution context cannot be read.
    fn hidden(&self, run: impl FnOnce()) {
        let aside = self.context.with_extension("aside");
        std::fs::rename(&self.context, &aside).unwrap();
        run();
        std::fs::rename(&aside, &self.context).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn the_incident_store_is_owed_from_zero_on_deploy_and_posts_each_answer_once() {
    let _env = env();
    let ops = Ops::new(|_| {}, None);
    let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Codex);
    engine.resume().unwrap();
    assert_eq!(
        boundary(&ops.harness, &ops.fresh),
        Boundary::Owed { from: 0 }
    );
    assert_eq!(resolutions(&ops.harness), 1);
    assert!(ledger_text(&ops.harness).contains(OPERATOR));
    assert!(!engine.pending_health());
    engine.deliver().await;
    assert_eq!(ops.harness.port.posts(), ANSWERS);

    // A new process replays the spool against the ledger and owes nothing again.
    drop(engine);
    let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Codex);
    engine.resume().unwrap();
    engine.deliver().await;
    assert_eq!(ops.harness.port.posts(), ANSWERS);
    assert_eq!(resolutions(&ops.harness), 1);
    assert!(!engine.pending_health());
}

#[tokio::test(start_paused = true)]
async fn a_resolution_logged_before_a_crash_is_applied_without_a_second_entry() {
    let _env = env();
    let ops = Ops::new(|_| {}, None);
    ops.harness
        .store
        .seed_boundary_resolved(CHANNEL, &ops.fresh, 0)
        .unwrap();
    let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Codex);
    engine.resume().unwrap();
    assert_eq!(
        boundary(&ops.harness, &ops.fresh),
        Boundary::Owed { from: 0 }
    );
    assert_eq!(resolutions(&ops.harness), 1);
    assert!(!ledger_text(&ops.harness).contains(OPERATOR));
    engine.deliver().await;
    assert_eq!(ops.harness.port.posts(), ANSWERS);
}

#[tokio::test(start_paused = true)]
async fn the_store_refuses_a_second_resolution_its_handle_has_not_read() {
    let _env = env();
    let ops = Ops::new(|_| {}, None);
    let mut store = ops.harness.channel();
    ops.harness
        .store
        .seed_boundary_resolved(CHANNEL, &ops.fresh, 0)
        .unwrap();
    assert!(store.resolve_codex_fresh_boundary(&ops.fresh, 2).is_err());
    assert_eq!(resolutions(&ops.harness), 1);
    assert_eq!(
        store.rotation().unwrap().link(&ops.fresh).unwrap().boundary,
        held()
    );
    let mut reopened = ops.harness.channel();
    reopened.apply_resolved_boundaries().unwrap();
    assert_eq!(
        boundary(&ops.harness, &ops.fresh),
        Boundary::Owed { from: 0 }
    );
}

/// Each case keeps F held: no resolution is written and its boundary does not move.
#[tokio::test(start_paused = true)]
async fn missing_or_mismatched_fresh_evidence_keeps_the_source_held() {
    let _env = env();
    let before = Utc::now() - chrono::TimeDelta::seconds(60);
    let cases: [(&str, fn(&mut BindingContext), Option<chrono::DateTime<Utc>>); 9] = [
        ("context_missing", |_| {}, None),
        ("resume_mode", |c| c.launch_mode = "resume".into(), None),
        (
            "resume_expected",
            |c| {
                c.launch_mode = "resume".into();
                c.expected_native_session_id = Some("named".into());
            },
            None,
        ),
        ("digest_absent", |c| c.first_prompt_digest = None, None),
        (
            "digest_mismatch",
            |c| c.first_prompt_digest = Some(digest("another prompt")),
            None,
        ),
        (
            "owner",
            |c| c.owner_runtime_root = "/another-owner".into(),
            None,
        ),
        ("pane", |c| c.tmux_session = "another-pane".into(), None),
        ("channel", |c| c.channel_id = Some(CHANNEL + 1), None),
        ("created_before_launch", |_| {}, Some(before)),
    ];
    for (case, change, created) in cases {
        let ops = Ops::new(change, created);
        if case == "context_missing" {
            std::fs::remove_file(&ops.context).unwrap();
        }
        let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Codex);
        engine.resume().unwrap();
        assert_eq!(boundary(&ops.harness, &ops.fresh), held(), "{case}");
        assert_eq!(resolutions(&ops.harness), 0, "{case}");
        assert!(engine.pending_health(), "{case}");
        engine.deliver().await;
        assert_eq!(ops.harness.port.posts(), Vec::<String>::new(), "{case}");
    }
}

/// The incident shape with one change to the binding log before F's bind.
fn rebound(
    prior: impl FnOnce(&Path, &SourceId, &str) -> Vec<BindingEvent>,
    cause: BindingCause,
) -> (Harness, Arc<FakeBindings>, SourceId) {
    let mut old = None;
    let harness = Harness::build_channel(CHANNEL, |root| {
        let root = root.canonicalize().unwrap();
        let body = [
            meta(&session(), Utc::now()),
            message("assistant", "before", "before"),
        ];
        let (path, source) = rollout(&root, "old", &session(), &body.concat());
        let bytes = std::fs::read(&path).unwrap();
        old = Some(source.clone());
        vec![InitSource {
            source_id: source,
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    let old = old.unwrap();
    let root = old.path.parent().unwrap().to_path_buf();
    let fresh_nonce = nonce();
    launched(&root, &fresh_nonce, |_| {});
    let id = session();
    let (_, fresh) = rollout(
        &root,
        "fresh",
        &id,
        &[meta(&id, Utc::now()), turns()].concat(),
    );
    let bindings = Arc::new(FakeBindings::new());
    bindings.commit(codex_bind(1, None, &old, BindingCause::Startup, &nonce()));
    let mut seq = 2;
    for mut event in prior(&root, &old, &fresh_nonce) {
        event.seq = seq;
        bindings.commit(event);
        seq += 1;
    }
    bindings.commit(codex_bind(seq, Some(&old), &fresh, cause, &fresh_nonce));
    harness.gate.acquired();
    (harness, bindings, fresh)
}

#[tokio::test(start_paused = true)]
async fn a_fresh_unknown_bind_is_owed_at_once_unless_its_history_names_an_earlier_source() {
    let _env = env();
    type Prior = fn(&Path, &SourceId, &str) -> Vec<BindingEvent>;
    let none: Prior = |_, _, _| Vec::new();
    // The execution first re-pinned the source the pane already read.
    let repin: Prior =
        |_, old, nonce| vec![codex_bind(0, Some(old), old, BindingCause::Unknown, nonce)];
    // The execution first bound another file of its own, now attached.
    let own_file: Prior = |root, old, nonce| {
        let id = session();
        let (_, other) = rollout(root, "own", &id, &meta(&id, Utc::now()));
        vec![codex_bind(
            0,
            Some(old),
            &other,
            BindingCause::Unknown,
            nonce,
        )]
    };
    let cases: [(&str, Prior, BindingCause, bool); 4] = [
        ("first_source", none, BindingCause::Unknown, true),
        ("repin_before", repin, BindingCause::Unknown, true),
        ("own_file_first", own_file, BindingCause::Unknown, false),
        ("resume_cause", none, BindingCause::Resume, false),
    ];
    for (case, prior, cause, owed) in cases {
        let (harness, bindings, fresh) = rebound(prior, cause);
        let mut engine = Engine::start(&harness, &bindings, ShadowProvider::Codex);
        engine.resume().unwrap();
        let expected = if owed {
            Boundary::Owed { from: 0 }
        } else {
            held()
        };
        assert_eq!(boundary(&harness, &fresh), expected, "{case}");
        assert_eq!(engine.pending_health(), !owed, "{case}");
        engine.capture();
        engine.deliver().await;
        let posts = if owed { ANSWERS.to_vec() } else { Vec::new() };
        assert_eq!(harness.port.posts(), posts, "{case}");
        assert_eq!(
            resolutions(&harness),
            0,
            "{case}: a bind decides without a resolution"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_startup_or_parented_bind_keeps_its_start() {
    let _env = env();
    for (cause, expected) in [
        (BindingCause::Startup, Boundary::Owed { from: 0 }),
        (BindingCause::Fork, Boundary::Undecided),
    ] {
        let (harness, bindings, fresh) = rebound(|_, _, _| Vec::new(), cause);
        if cause == BindingCause::Fork {
            let mut events = bindings.events.lock().unwrap();
            let last = events.last_mut().unwrap();
            if let BindingRecord::Bound {
                old, parent_hint, ..
            } = &mut last.record
            {
                *parent_hint = old.clone();
            }
        }
        let mut engine = Engine::start(&harness, &bindings, ShadowProvider::Codex);
        engine.resume().unwrap();
        assert_eq!(boundary(&harness, &fresh), expected, "{cause:?}");
    }
}

/// An earlier event named F's file, so its records may be old, whether or not that name is read.
#[tokio::test(start_paused = true)]
async fn an_earlier_name_of_the_fresh_file_keeps_it_held() {
    let _env = env();
    type Rename = fn(&mut SourceId);
    let cases: [(&str, Rename, bool); 3] = [
        ("other_dev_unread", |s| s.dev += 1, false),
        ("empty_session_unread", |s| s.session_id.clear(), false),
        ("empty_session_attached", |s| s.session_id.clear(), true),
    ];
    for (case, rename, attached) in cases {
        let mut old = None;
        let harness = Harness::build_channel(CHANNEL, |root| {
            let root = root.canonicalize().unwrap();
            let body = [
                meta(&session(), Utc::now()),
                message("assistant", "before", "before"),
            ];
            let (path, source) = rollout(&root, "old", &session(), &body.concat());
            let bytes = std::fs::read(&path).unwrap();
            old = Some(source.clone());
            vec![InitSource {
                source_id: source,
                delivery_start: bytes.len() as u64,
                prefix_hash: hex::encode(Sha256::digest(&bytes)),
            }]
        });
        let old = old.unwrap();
        let root = old.path.parent().unwrap().to_path_buf();
        let fresh_nonce = nonce();
        launched(&root, &fresh_nonce, |_| {});
        let id = session();
        let (_, fresh) = rollout(
            &root,
            "fresh",
            &id,
            &[meta(&id, Utc::now()), turns()].concat(),
        );
        let mut earlier = fresh.clone();
        rename(&mut earlier);
        let bindings = Arc::new(FakeBindings::new());
        let mut other_pane = codex_bind(1, None, &earlier, BindingCause::Startup, &nonce());
        other_pane.tmux_session = "another-pane".into();
        if attached {
            bindings.commit(codex_bind(1, None, &old, BindingCause::Startup, &nonce()));
            other_pane.seq = 2;
            bindings.commit(other_pane);
        } else {
            bindings.commit(other_pane);
            bindings.commit(codex_bind(2, None, &old, BindingCause::Startup, &nonce()));
        }
        bindings.commit(codex_bind(
            3,
            Some(&old),
            &fresh,
            BindingCause::Unknown,
            &fresh_nonce,
        ));
        harness.gate.acquired();
        let mut engine = Engine::start(&harness, &bindings, ShadowProvider::Codex);
        engine.resume().unwrap();
        engine.capture();
        assert_eq!(
            harness.channel().cursor(&earlier).is_some(),
            attached,
            "{case}: the earlier name is read only when O applied it"
        );
        assert_eq!(boundary(&harness, &fresh), held(), "{case}");
        assert_eq!(resolutions(&harness), 0, "{case}");
        // A later process judges the retained spool again and still refuses it.
        drop(engine);
        let mut engine = Engine::start(&harness, &bindings, ShadowProvider::Codex);
        engine.resume().unwrap();
        assert_eq!(boundary(&harness, &fresh), held(), "{case}");
        assert_eq!(resolutions(&harness), 0, "{case}");
        if !attached {
            engine.deliver().await;
            assert_eq!(harness.port.posts(), Vec::<String>::new(), "{case}");
        }
    }
}

/// Records each owed record's range, so a re-derivation shows up even when its unit is sealed.
#[derive(Default)]
struct Ranges {
    ranges: Vec<(u64, u64)>,
    derived: VecDeque<Derived>,
}

impl Owe for Ranges {
    fn owe(&mut self, _: &SourceId, record: &CapturedRecord, derived: Vec<Derived>) {
        self.ranges.push((record.start, record.end));
        self.derived.extend(derived);
    }

    fn finish_replay(
        &mut self,
        ledger: &crate::services::tui_o::store::ledger::LedgerState,
    ) -> crate::services::tui_o::writer::ReplaySummary {
        self.derived.finish_replay(ledger)
    }
}

#[tokio::test(start_paused = true)]
async fn a_prompt_written_after_the_bind_resolves_on_the_capture_that_reads_it() {
    let _env = env();
    let (harness, bindings, fresh) = rebound(|_, _, _| Vec::new(), BindingCause::Unknown);
    let id = fresh.session_id.clone();
    let created = Utc::now();
    std::fs::write(&fresh.path, meta(&id, created)).unwrap();
    let mut engine = Engine::start(&harness, &bindings, ShadowProvider::Codex);
    engine.resume().unwrap();
    assert_eq!(boundary(&harness, &fresh), held(), "no first prompt yet");
    let mut ranges = Ranges::default();
    let (writer, deriver) = (&mut engine.writer, &mut engine.deriver);
    engine
        .sources
        .capture(writer, deriver, &mut ranges)
        .unwrap();
    assert_eq!(boundary(&harness, &fresh), held());
    assert_eq!(resolutions(&harness), 0);

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&fresh.path)
        .unwrap();
    std::io::Write::write_all(&mut file, &message("user", "user-0", PROMPT)).unwrap();
    std::io::Write::write_all(&mut file, &message("assistant", "answer-0", ANSWERS[0])).unwrap();
    drop(file);
    engine
        .sources
        .capture(writer, deriver, &mut ranges)
        .unwrap();
    assert_eq!(boundary(&harness, &fresh), Boundary::Owed { from: 0 });
    assert_eq!(resolutions(&harness), 1);
    assert!(!engine.pending_health());
    let mut seen = ranges.ranges.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        ranges.ranges.len(),
        "each record is owed once: {:?}",
        ranges.ranges
    );
    assert_eq!(seen.len(), 3, "metadata, prompt and answer");
    engine.owed = std::mem::take(&mut ranges.derived);
    engine.deliver().await;
    assert_eq!(harness.port.posts(), [ANSWERS[0]]);
}

#[tokio::test(start_paused = true)]
async fn changed_or_replaced_fresh_bytes_are_never_resolved() {
    let _env = env();
    for case in ["answer_rewritten", "inode_replaced", "truncated"] {
        let ops = Ops::new(|_| {}, None);
        let bytes = std::fs::read(&ops.fresh_path).unwrap();
        match case {
            "answer_rewritten" => {
                let text = String::from_utf8(bytes)
                    .unwrap()
                    .replace(ANSWERS[3], "e2e answer FOUR");
                std::fs::write(&ops.fresh_path, text).unwrap();
            }
            "inode_replaced" => {
                std::fs::remove_file(&ops.fresh_path).unwrap();
                std::fs::write(&ops.fresh_path, &bytes).unwrap();
            }
            _ => std::fs::write(&ops.fresh_path, &bytes[..bytes.len() - 10]).unwrap(),
        }
        let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Codex);
        let resumed = engine.resume();
        assert_eq!(resolutions(&ops.harness), 0, "{case}");
        assert_eq!(boundary(&ops.harness, &ops.fresh), held(), "{case}");
        if case == "answer_rewritten" {
            assert!(
                matches!(&resumed, Err(WriterAlarm::Halted { detail }) if detail.contains("changed")),
                "{case}: changed bytes still halt the resume: {resumed:?}"
            );
        } else {
            assert!(
                resumed.is_ok(),
                "{case}: a retired unread source is closed: {resumed:?}"
            );
            assert!(engine.pending_health(), "{case}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_suspended_held_source_is_left_alone_while_a_selected_one_stays_counted() {
    let _env = env();
    let ops = Ops::new(|_| {}, None);
    let unproven = {
        let id = session();
        let (_, other) = rollout(
            &ops.root,
            "unproven",
            &id,
            &[meta(&id, Utc::now()), turns()].concat(),
        );
        other
    };
    let next = ops.bindings.events.lock().unwrap().last().cloned().unwrap();
    let BindingRecord::Bound {
        new: BindingTarget::Source(next),
        ..
    } = next.record
    else {
        unreachable!()
    };
    ops.bindings.commit(codex_bind(
        4,
        Some(&next),
        &unproven,
        BindingCause::Unknown,
        &nonce(),
    ));
    let suspension = Suspension::default();
    suspension.extend(ops.fresh_nonce.clone(), [ops.fresh.clone()]);
    let mut engine = Engine::start_with(
        &ops.harness,
        &ops.bindings,
        ShadowProvider::Codex,
        suspension,
    );
    engine.resume().unwrap();
    engine.capture();
    assert_eq!(boundary(&ops.harness, &ops.fresh), held());
    assert_eq!(boundary(&ops.harness, &unproven), held());
    assert_eq!(resolutions(&ops.harness), 0);
    assert!(
        engine.pending_health(),
        "the selected held source is still counted"
    );
    engine.deliver().await;
    assert_eq!(ops.harness.port.posts(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_claude_writer_never_runs_the_codex_judgment() {
    let _env = env();
    let ops = Ops::new(|_| {}, None);
    let mut engine = Engine::start(&ops.harness, &ops.bindings, ShadowProvider::Claude);
    engine.resume().unwrap();
    assert_eq!(boundary(&ops.harness, &ops.fresh), held());
    assert_eq!(resolutions(&ops.harness), 0);
}
