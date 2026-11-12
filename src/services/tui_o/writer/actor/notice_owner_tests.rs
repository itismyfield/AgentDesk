use super::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const OWNER_NONCE: &str = "73320000000000000000000000000092";
const NAMESPACE: &str = "notice-owner-tests";

#[path = "notice_cache_tests.rs"]
mod cache_tests;
#[path = "notice_resume_tests.rs"]
mod resume_tests;

struct Case {
    path: PathBuf,
    source: crate::services::tui_o::shadow::SourceId,
    logical: String,
    o: OStore,
    port: Arc<Port>,
    _binding: crate::services::tui_o::writer::input_notices::admission::tests::BindingRoot,
    _runtime: tempfile::TempDir,
    _guard: crate::config::TestRuntimeRootGuard,
}

fn registration(path: &Path, logical: &str, nonce: &str, id: u64, pre: bool) -> Value {
    let (dev, ino) =
        crate::services::tui_o::shadow::capture::file_identity(&std::fs::metadata(path).unwrap());
    json!({"identity":{"primary":id,"queued_generation":5,"sources":[{"message_id":id,"queued_generation":5}]},
        "attempt":{"nonce":format!("notice-attempt-{id}"),"provider":"claude",
            "target":{"host":"tmux","owner":logical,"root":"/tmp","execution_nonce":nonce},
            "source":if pre { json!({"kind":"pre_source","expected_session_id":"native","expected_transcript":path}) }
                else { json!({"kind":"existing","session_id":"native","transcript":path,"file":[dev,ino],"offset":0}) }}})
}

fn registered_input(key: &Value, frame: &str) -> Value {
    json!({"identity":key["identity"],"attempt":key["attempt"],"original":{"author_id":1},"payload":{"frame":frame}})
}

pub(super) fn ambiguous_registration(
    path: &Path,
    logical: &str,
    nonce: &str,
    frame: &str,
) -> (history::v2::Registry, [Value; 2]) {
    let registry = history::v2::Registry::runtime().unwrap();
    let keys = [8817, 8818].map(|id| registration(path, logical, nonce, id, false));
    for key in &keys {
        registry
            .prepare(NAMESPACE, &registered_input(key, frame))
            .unwrap();
    }
    (registry, keys)
}

pub(super) fn ambiguous_receipt(
    registry: &history::v2::Registry,
    key: &Value,
    path: &Path,
    uuid: &str,
    frame: &str,
) {
    assert_eq!(
        registry.receipt(NAMESPACE, key, (path, 0, "user", Some(uuid), frame)),
        Some(true)
    );
}

pub(super) fn abandon_rival(registry: &history::v2::Registry, keys: &[Value; 2], frame: &str) {
    let mut rival = registered_input(&keys[1], frame);
    rival["phase"] = "reserved".into();
    registry
        .project(&json!({
            "namespace": NAMESPACE,
            "head_injection": registered_input(&keys[0], frame),
            "retained_inputs": [{"input": rival, "disposition": "known_not_sent"}],
            "terminal_identities": []
        }))
        .unwrap();
}

pub(super) fn ambiguous_withdraw(registry: &history::v2::Registry, keys: &[Value; 2], frame: &str) {
    let retained: Vec<_> = keys
        .iter()
        .map(|key| {
            let mut input = registered_input(key, frame);
            input["phase"] = "reserved".into();
            json!({"input":input,"disposition":"known_not_sent"})
        })
        .collect();
    registry
        .project(
            &json!({"namespace":NAMESPACE,"retained_inputs":retained,"terminal_identities":[]}),
        )
        .unwrap();
}

impl Case {
    fn new(label: &str) -> Self {
        let guard = crate::config::TestRuntimeRootGuard::new();
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("native.jsonl");
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native", &path).unwrap();
        let logical = format!("notice-owner-{label}");
        let binding = bind(runtime.path(), CHANNEL, &source, &logical, OWNER_NONCE);
        let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        era(&o, &source);
        Self {
            path,
            source,
            logical,
            o,
            port: Arc::new(Port::default()),
            _binding: binding,
            _runtime: runtime,
            _guard: guard,
        }
    }

    fn frame(&self) -> String {
        format!(
            "[User: imessage (ID: 1)]\n[아이메시지 요청 · 시험]\n{}",
            self.logical
        )
    }

    fn register(&self, frame: &str) {
        history::register(
            ("claude", &self.logical, "notice-attempt", "1"),
            &self.path,
            0,
            frame,
        )
        .unwrap();
    }

    fn v2(&self, frame: &str, id: u64, pre: bool) -> (history::v2::Registry, Value) {
        let registry = history::v2::Registry::runtime().unwrap();
        let key = registration(&self.path, &self.logical, OWNER_NONCE, id, pre);
        registry
            .prepare(NAMESPACE, &registered_input(&key, frame))
            .unwrap();
        (registry, key)
    }

    fn input(&self, id: &str, text: &str) {
        append(&self.path, &row(id, text));
        append(
            &self.path,
            &body(&format!("body-{id}"), &format!("answer-{id}")),
        );
    }

    fn open(&self) -> Host {
        let mut actor = host(&self.o, self.port.clone(), Alarms::default(), u64::MAX);
        if actor
            .writer
            .store()
            .ledger()
            .input_notices()
            .migration
            .is_none()
        {
            start_migration(&mut actor);
        }
        resume(&mut actor);
        actor
    }

    async fn run(&self, actor: &mut Host) {
        let fresh = actor.read_sources();
        actor.prepare_notices(&fresh).await;
        actor
            .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
                CHANNEL,
            ))
            .await;
    }

    fn notices(&self) -> usize {
        self.port
            .posts
            .lock()
            .unwrap()
            .iter()
            .filter(|text| text.starts_with("-# ⌨️"))
            .count()
    }

    fn excluded(&self, actor: &mut Host, id: &str) {
        assert_eq!(
            actor.writer.store().ledger().excluded(&input_key(id)),
            Some("input_notice_source")
        );
        assert_eq!(self.notices(), 0);
        assert_eq!(actor.notice_debt_extra(), 0);
    }

    fn collector_ends(&self, frame: &str, enqueue: bool) {
        if enqueue {
            let mut bytes =
                serde_json::to_vec(&json!({"type":"queue-operation","operation":"enqueue",
                "uuid":"enqueue-only","content":frame}))
                .unwrap();
            bytes.push(b'\n');
            append(&self.path, &bytes);
        }
        let (path, frame) = (self.path.clone(), frame.to_owned());
        let reader = std::thread::spawn(move || {
            crate::services::claude_tui::busy_inject::await_receipt(
                (&path, 0, "notice-attempt", &frame),
                std::time::Instant::now()
                    + std::time::Duration::from_millis(if enqueue { 2000 } else { 20 }),
                std::time::Duration::from_millis(2),
            )
        });
        let result = reader.join().unwrap();
        assert_eq!(
            result.is_ok(),
            enqueue,
            "the real collector ended before the user row: {result:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn notice_owner_excludes_enqueue_only_input_after_late_user_row() {
    let c = Case::new("enqueue-only");
    let frame = c.frame();
    c.register(&frame);
    c.collector_ends(&frame, true);
    c.input("owned", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(notice_evidence(&mut actor, &input_key("owned")), 1);
    c.excluded(&mut actor, "owned");
    assert_eq!(*c.port.posts.lock().unwrap(), ["answer-owned"]);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_excludes_row_written_after_receipt_window() {
    let c = Case::new("after-window");
    let frame = c.frame();
    c.register(&frame);
    c.collector_ends(&frame, false);
    c.input("late", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.excluded(&mut actor, "late");
    assert_eq!(*c.port.posts.lock().unwrap(), ["answer-late"]);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_settles_restored_materialized_debt_after_raw_release() {
    let c = Case::new("restored");
    let frame = c.frame();
    c.register(&frame);
    c.input("restored", &frame);
    let mut actor = c.open();
    actor.read_sources();
    let parked = c.path.with_extension("parked");
    std::fs::rename(&c.path, &parked).unwrap();
    actor.prepare_notices(&[]).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(actor.notice_debt_extra(), 1);
    assert!(actor.notice_raw_independent());
    std::fs::rename(&parked, &c.path).unwrap();
    let through = std::fs::metadata(&c.path).unwrap().len();
    let cap = actor.writer.store().spool_bytes();
    actor.writer.store().set_limits_for_test(1, cap);
    append(&c.path, &body("later-body", &"later answer".repeat(40)));
    actor.read_sources();
    assert_eq!(
        actor
            .writer
            .store()
            .cursor(&c.source)
            .unwrap()
            .captured_through,
        through,
        "small cap must hold the next batch before raw collection"
    );
    actor.collect_settled();
    assert_eq!(actor.writer.store().retained_segments(&c.source), 0);
    drop(actor);
    let mut actor = c.open();
    let fresh = actor.read_sources();
    assert!(!fresh.contains(&input_key("restored")));
    actor.prepare_notices(&fresh).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    c.excluded(&mut actor, "restored");
    assert_eq!(notice_evidence(&mut actor, &input_key("restored")), 1);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_keeps_pending_without_native_census() {
    let c = Case::new("missing-census");
    let frame = c.frame();
    c.register(&frame);
    c.input("missing", &frame);
    let mut actor = c.open();
    actor.read_sources();
    let parked = c.path.with_extension("parked");
    std::fs::rename(&c.path, &parked).unwrap();
    actor.prepare_notices(&[]).await;
    assert_eq!(actor.notice_debt_extra(), 1);
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("missing"))
            .is_none()
    );
    assert_eq!(c.notices(), 0);
    std::fs::rename(&parked, &c.path).unwrap();
    assert_eq!(
        history::receipt(
            "notice-attempt",
            &c.path,
            0,
            "user",
            Some("missing"),
            &frame
        ),
        Some(true)
    );
    std::fs::rename(&c.path, parked).unwrap();
    actor.prepare_notices(&[]).await;
    c.excluded(&mut actor, "missing");
}

#[tokio::test(start_paused = true)]
async fn direct_twin_of_registered_frame_posts_once() {
    let c = Case::new("body-twin");
    let text = "same body";
    let frame = format!("[User: imessage (ID: 1)]\n{text}");
    c.register(&frame);
    c.input("injected", &frame);
    c.input("direct", text);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 1);
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("direct"))
            .is_none()
    );
    assert!(
        matches!(actor.writer.store().ledger().latest_piece(&input_key("direct"), 0),
        Some((_, record)) if matches!(record.outcome, Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(_))))
    );
}

#[tokio::test(start_paused = true)]
async fn direct_full_frame_twin_after_registered_owner_posts_once() {
    let c = Case::new("full-frame-twin");
    let frame = c.frame();
    c.register(&frame);
    c.input("injected", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.excluded(&mut actor, "injected");
    c.input("direct", &frame);
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 1);
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("direct"))
            .is_none()
    );
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_never_reuses_occupied_attempt() {
    let c = Case::new("occupied");
    let frame = c.frame();
    c.register(&frame);
    assert_eq!(
        history::receipt(
            "notice-attempt",
            &c.path,
            0,
            "user",
            Some("historical-row-outside-census"),
            &frame
        ),
        Some(true)
    );
    c.input("direct", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("direct"))
            .is_none()
    );
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_rejects_other_source_registration() {
    for wrong in ["session", "nonce", "file", "path", "offset"] {
        let c = Case::new(wrong);
        let frame = c.frame();
        let mut key = registration(&c.path, &c.logical, OWNER_NONCE, 8811, false);
        let other = c.path.with_extension("other");
        std::fs::write(&other, b"").unwrap();
        match wrong {
            "session" => key["attempt"]["source"]["session_id"] = "foreign".into(),
            "nonce" => key["attempt"]["target"]["execution_nonce"] = "foreign".into(),
            "offset" => {
                c.input("foreign", &frame);
                key["attempt"]["source"]["offset"] =
                    std::fs::metadata(&c.path).unwrap().len().into();
            }
            "file" | "path" => {
                let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
                    &std::fs::metadata(&other).unwrap(),
                );
                key["attempt"]["source"]["transcript"] = json!(other);
                key["attempt"]["source"]["file"] = json!([dev, ino]);
            }
            _ => unreachable!(),
        }
        history::v2::Registry::runtime()
            .unwrap()
            .prepare(NAMESPACE, &registered_input(&key, &frame))
            .unwrap();
        if wrong != "offset" {
            c.input("foreign", &frame);
        }
        let mut actor = c.open();
        c.run(&mut actor).await;
        assert!(
            actor
                .writer
                .store()
                .ledger()
                .excluded(&input_key("foreign"))
                .is_none(),
            "{wrong}"
        );
        assert_eq!(c.notices(), 0, "{wrong}");
        assert_eq!(actor.notice_debt_extra(), 1, "{wrong}");
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_imessage_debt_outside_snapshot_settles_without_post() {
    let c = Case::new("outside-snapshot");
    let frame = c.frame();
    c.v2(&frame, 8812, false);
    c.input("outside-snapshot", &frame);
    let mut actor = c.open();
    actor.read_sources();
    actor.prepare_notices(&[]).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    c.excluded(&mut actor, "outside-snapshot");
    assert_eq!(*c.port.posts.lock().unwrap(), ["answer-outside-snapshot"]);
}

#[tokio::test(start_paused = true)]
async fn legacy_imessage_debt_presource_binds_then_settles_without_post() {
    let c = Case::new("bound-presource");
    let frame = c.frame();
    let (registry, key) = c.v2(&frame, 8813, true);
    c.input("bound-presource", &frame);
    registry
        .bind(
            NAMESPACE,
            &key,
            &history::v2::Source {
                target: key["attempt"]["target"].clone(),
                provider: "claude".into(),
                session_id: "native".into(),
                transcript: c.path.clone(),
                file: [c.source.dev, c.source.ino],
                offset: 0,
                binding_watermark: None,
            },
        )
        .unwrap();
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.excluded(&mut actor, "bound-presource");
}

#[tokio::test(start_paused = true)]
async fn typed_imessage_text_without_registration_posts_once() {
    let c = Case::new("typed-imessage");
    c.input("typed", "[아이메시지 요청 · 시험]\n직접 입력한 내용");
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn registered_imessage_row_never_posted_by_writer() {
    for variant in ["exact", "census", "planned", "unregistered"] {
        let c = Case::new(variant);
        let frame = c.frame();
        let registered =
            (variant != "unregistered").then(|| c.v2(&frame, 8814, variant == "planned"));
        c.input("duplicate", &frame);
        if variant == "exact" {
            let (registry, key) = registered.as_ref().unwrap();
            assert_eq!(
                registry.receipt(
                    NAMESPACE,
                    key,
                    (&c.path, 0, "user", Some("duplicate"), &frame)
                ),
                Some(true)
            );
        }
        let mut actor = c.open();
        c.run(&mut actor).await;
        assert_eq!(c.notices(), 0, "{variant}");
        assert_eq!(
            *c.port.posts.lock().unwrap(),
            ["answer-duplicate"],
            "{variant}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn settled_v2_registration_still_needs_native_notice_attribution() {
    let c = Case::new("settled");
    let frame = c.frame();
    let (registry, key) = c.v2(&frame, 8815, false);
    registry
        .project(&json!({"namespace":NAMESPACE,"retained_inputs":[],
        "terminal_identities":[{"identity":key["identity"]}]}))
        .unwrap();
    c.input("settled", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.excluded(&mut actor, "settled");
}

#[tokio::test(start_paused = true)]
async fn notice_owner_v2_exact_receipt_settles_without_current_native_file() {
    let c = Case::new("historical-v2");
    let frame = c.frame();
    let (registry, key) = c.v2(&frame, 8819, false);
    c.input("historical-v2", &frame);
    let mut actor = c.open();
    actor.read_sources();
    let parked = c.path.with_extension("parked");
    std::fs::rename(&c.path, &parked).unwrap();
    actor.prepare_notices(&[]).await;
    assert_eq!(actor.notice_debt_extra(), 1);
    std::fs::rename(&parked, &c.path).unwrap();
    assert_eq!(
        registry.receipt(
            NAMESPACE,
            &key,
            (&c.path, 0, "user", Some("historical-v2"), &frame)
        ),
        Some(true)
    );
    std::fs::rename(&c.path, parked).unwrap();
    actor.prepare_notices(&[]).await;
    c.excluded(&mut actor, "historical-v2");
}

#[tokio::test(start_paused = true)]
async fn registered_headless_routine_input_settles_excluded() {
    let c = Case::new("routine");
    let frame = "[User: routine (ID: 1)]\nroutine input";
    c.v2(frame, 8816, false);
    c.input("routine", frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.excluded(&mut actor, "routine");
}

#[tokio::test(start_paused = true)]
async fn known_discord_input_writes_no_notice_evidence() {
    let c = Case::new("known-discord");
    let frame = "[User: ann (ID: 733201)]\noriginal Discord input";
    c.register(frame);
    c.input("known", frame);
    assert_eq!(
        history::receipt("notice-attempt", &c.path, 0, "user", Some("known"), frame),
        Some(true)
    );
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .input_notices()
            .evidence
            .is_empty()
    );
    assert_eq!(c.notices(), 0);
}

#[tokio::test(start_paused = true)]
async fn undecidable_ownership_keeps_notice_debt_while_rival_is_unresolved() {
    let c = Case::new("rival-unresolved");
    let frame = c.frame();
    let (registry, keys) = ambiguous_registration(&c.path, &c.logical, OWNER_NONCE, &frame);
    c.input("ambiguous", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(actor.notice_debt_extra(), 1);
    for key in &keys {
        assert_eq!(
            registry.receipt(
                NAMESPACE,
                key,
                (&c.path, 0, "user", Some("ambiguous"), &frame)
            ),
            None
        );
    }
    actor.prepare_notices(&[]).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("ambiguous"))
            .is_none()
    );
    assert_eq!(actor.notice_debt_extra(), 1);
    assert_eq!(notice_evidence(&mut actor, &input_key("ambiguous")), 1);
    assert_eq!(c.notices(), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_batch_scans_each_source_once_for_all_evidence() {
    let c = Case::new("one-census");
    let frame = c.frame();
    c.register(&frame);
    for id in ["owned", "direct-1", "direct-2"] {
        c.input(id, &frame);
    }
    let mut actor = c.open();
    let fresh = actor.read_sources();
    history::notice_owner::CENSUS_READS.with(|count| count.set(0));
    actor.prepare_notices(&fresh).await;
    assert_eq!(
        history::notice_owner::CENSUS_READS.with(|count| count.get()),
        1
    );
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(
        history::notice_owner::CENSUS_READS.with(|count| count.get()),
        1,
        "final admission consumes the cache"
    );
    assert_eq!(
        actor.writer.store().ledger().excluded(&input_key("owned")),
        Some("input_notice_source")
    );
    assert_eq!(c.notices(), 2);
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_history_change_after_preflight_defers_until_next_batch() {
    let c = Case::new("changed-history");
    let frame = c.frame();
    c.register(&frame);
    assert_eq!(
        history::receipt(
            "notice-attempt",
            &c.path,
            0,
            "user",
            Some("historical"),
            &frame
        ),
        Some(true)
    );
    c.input("direct", &frame);
    let mut actor = c.open();
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    history::register(
        ("claude", &c.logical, "future-attempt", "1"),
        &c.path,
        std::fs::metadata(&c.path).unwrap().len(),
        &frame,
    )
    .unwrap();
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(c.notices(), 0);
    assert_eq!(
        actor.progress().debt,
        1,
        "the withheld notice stays owed in the queue"
    );
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .latest_piece(&input_key("direct"), 0)
            .is_none()
    );
    actor.prepare_notices(&[]).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_migrated_v1_registration_keeps_its_historical_scope() {
    let c = Case::new("migrated-v1");
    let frame = c.frame();
    c.register(&frame);
    c.v2(
        "[User: routine (ID: 1)] another registered input",
        8821,
        false,
    );
    c.input("migrated", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(notice_evidence(&mut actor, &input_key("migrated")), 1);
    c.excluded(&mut actor, "migrated");
}

#[tokio::test(start_paused = true)]
async fn legacy_imessage_twenty_one_materialized_debts_exclude_on_reopen_without_post() {
    let c = Case::new("legacy-twenty-one");
    for n in 0..21 {
        let frame = format!("{}\n기존 요청 {n}", c.frame());
        c.v2(&frame, 8900 + n, false);
        append(&c.path, &row(&format!("legacy-{n}"), &frame));
    }
    let mut actor = c.open();
    actor.read_sources();
    let parked = c.path.with_extension("parked");
    std::fs::rename(&c.path, &parked).unwrap();
    actor.prepare_notices(&[]).await;
    assert_eq!(actor.notice_debt_extra(), 21);
    assert_eq!(
        actor.writer.store().ledger().input_notices().evidence.len(),
        21
    );
    assert!(c.port.posts.lock().unwrap().is_empty());
    drop(actor);
    std::fs::rename(parked, &c.path).unwrap();
    let mut actor = c.open();
    let fresh = actor.read_sources();
    assert!(!fresh.iter().any(|key| key.kind == UnitKind::InputNotice));
    actor.prepare_notices(&fresh).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    for n in 0..21 {
        assert_eq!(
            actor
                .writer
                .store()
                .ledger()
                .excluded(&input_key(&format!("legacy-{n}"))),
            Some("input_notice_source")
        );
    }
    assert_eq!(actor.notice_debt_extra(), 0);
    assert!(
        c.port.posts.lock().unwrap().is_empty(),
        "21/21 Excluded and POST 0"
    );
    drop(actor);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(actor.notice_debt_extra(), 0);
    assert!(c.port.posts.lock().unwrap().is_empty());
}

async fn repeated_nonce_case(separate_namespace: bool) {
    let c = Case::new(if separate_namespace {
        "nonce-namespace"
    } else {
        "nonce-identity"
    });
    let frame = c.frame();
    let (registry, first) = c.v2(&frame, 8823, false);
    c.input("full-key-first", &frame);
    assert_eq!(
        registry.receipt(
            NAMESPACE,
            &first,
            (&c.path, 0, "user", Some("full-key-first"), &frame)
        ),
        Some(true)
    );
    let (second_namespace, mut second) = if separate_namespace {
        ("notice-owner-other", first.clone())
    } else {
        (
            NAMESPACE,
            registration(&c.path, &c.logical, OWNER_NONCE, 8824, false),
        )
    };
    if !separate_namespace {
        second["attempt"]["nonce"] = first["attempt"]["nonce"].clone();
        second["attempt"]["source"]["offset"] = std::fs::metadata(&c.path).unwrap().len().into();
    }
    assert_eq!(first["attempt"]["nonce"], second["attempt"]["nonce"]);
    assert_ne!((NAMESPACE, &first), (second_namespace, &second));
    registry
        .prepare(second_namespace, &registered_input(&second, &frame))
        .unwrap();
    assert!(registry.registered(second_namespace, &second, &c.path, &frame));
    c.input("full-key-second", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(
        c.notices(),
        0,
        "a receipt must occupy only its full namespace/key"
    );
    c.excluded(&mut actor, "full-key-second");
    assert_eq!(
        *c.port.posts.lock().unwrap(),
        ["answer-full-key-first", "answer-full-key-second"]
    );
}

#[tokio::test(start_paused = true)]
async fn notice_owner_keeps_distinct_v2_identities_with_same_nonce() {
    repeated_nonce_case(false).await;
}

#[tokio::test(start_paused = true)]
async fn notice_owner_keeps_distinct_v2_namespaces_with_same_nonce() {
    repeated_nonce_case(true).await;
}
