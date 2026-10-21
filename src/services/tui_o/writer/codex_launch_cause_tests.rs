//! Fresh Codex discovery reaches O through the production registration and binding log.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_o::writer::binding::BindingLog;
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_prompt_dedupe::{
    self as dedupe, binding_context, binding_events as events,
};
use binding_context::PreparedIncarnation;
use serde_json::json;
use sha2::{Digest, Sha256};

const PROMPT: &str = "  fresh Codex launch prompt\n첫 입력\n  ";
const ANSWER: &str = "fresh Codex answer";

struct Fixture {
    runtime: tempfile::TempDir,
    _env: [crate::config::TestEnvVarGuard; 2],
    _sessions: tempfile::TempDir,
    prepared: PreparedIncarnation,
    tmux: String,
    id: String,
    path: PathBuf,
    harness: Harness,
    writer: Writer,
    sources: Sources<BindingLog>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
}

impl Fixture {
    fn new() -> Self {
        let (runtime, env) = binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        events::set_test_root(Some(runtime.path()));
        let sessions = tempfile::tempdir().unwrap();
        let tmux = format!("codex-launch-cause-{}", uuid::Uuid::new_v4().simple());
        let prepared = PreparedIncarnation::prepare_pinned(
            "codex",
            &tmux,
            Some(CHANNEL),
            None,
            false,
            Some(sessions.path().canonicalize().unwrap()),
            (
                Some(format!("sha256:{:x}", Sha256::digest(PROMPT.as_bytes()))),
                Some("shadow".into()),
            ),
        )
        .unwrap();
        std::fs::write(
            tc::session_temp_path(&tmux, "spawn_nonce"),
            &prepared.context.execution_nonce,
        )
        .unwrap();
        dedupe::register_tmux_channel(&tmux, CHANNEL);
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions.path().join(format!("rollout-test-{id}.jsonl"));
        let harness = Harness::new();
        harness.channel().set_binding_checkpoint(0).unwrap();
        harness.gate.acquired();
        let mut fixture = Self {
            writer: harness.writer(),
            sources: Sources::new(CHANNEL, ShadowProvider::Codex, Arc::new(BindingLog)),
            deriver: UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
            owed: VecDeque::new(),
            runtime,
            _env: env,
            _sessions: sessions,
            prepared,
            tmux,
            id,
            path,
            harness,
        };
        fixture
            .sources
            .resume(&mut fixture.writer, &mut fixture.deriver, &mut fixture.owed)
            .unwrap();
        fixture.write_complete();
        fixture
    }

    fn header(&self) -> serde_json::Value {
        json!({"type":"session_meta","payload":{
            "id":self.id,"cwd":"/synthetic","source":"cli",
            "timestamp":(self.prepared.context.created_at + chrono::Duration::seconds(1)).to_rfc3339()
        }})
    }

    fn write_complete(&self) {
        self.write_rows(&[
            self.header(),
            json!({"type":"response_item","payload":{
                "type":"message","role":"user","id":"user-first",
                "content":[{"type":"input_text","text":PROMPT}]
            }}),
            json!({"type":"response_item","payload":{
                "type":"message","role":"assistant","id":"answer-first",
                "content":[{"type":"output_text","text":ANSWER}]
            }}),
            json!({"type":"event_msg","payload":{
                "type":"task_complete","last_agent_message":ANSWER
            }}),
        ]);
    }

    fn write_rows(&self, rows: &[serde_json::Value]) {
        let bytes = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        std::fs::write(&self.path, bytes).unwrap();
    }

    fn binding(&self) -> dedupe::TuiRuntimeBinding {
        dedupe::TuiRuntimeBinding {
            runtime_kind: CodexTui,
            output_path: self.path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(self.id.clone()),
            last_offset: 0,
            relay_last_offset: None,
        }
    }

    fn register(&self) -> bool {
        dedupe::register_rehydrated_tmux_runtime_binding(
            "codex",
            &self.tmux,
            CHANNEL,
            self.binding(),
        )
    }

    fn step(&mut self) {
        self.sources.follow(&mut self.writer).unwrap();
        self.sources
            .capture(&mut self.writer, &mut self.deriver, &mut self.owed)
            .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                while let Some(item) = self.owed.pop_front() {
                    assert_eq!(self.writer.deliver(&item).await, Step::Done);
                }
            });
    }

    fn history(&self) -> Vec<events::BindingEvent> {
        events::records_strict(CHANNEL).unwrap().unwrap_or_default()
    }

    fn write_context(&self, change: impl FnOnce(&mut binding_context::BindingContext)) {
        let mut context = self.prepared.context.clone();
        change(&mut context);
        std::fs::write(&self.prepared.path, serde_json::to_vec(&context).unwrap()).unwrap();
    }

    fn restart_at(&mut self, checkpoint: u64) {
        self.harness
            .channel()
            .set_binding_checkpoint(checkpoint)
            .unwrap();
        self.writer = self.harness.writer();
        self.sources = Sources::new(CHANNEL, ShadowProvider::Codex, Arc::new(BindingLog));
        self.deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        self.owed.clear();
        self.sources
            .resume(&mut self.writer, &mut self.deriver, &mut self.owed)
            .unwrap();
    }

    fn posted(&mut self) -> usize {
        let ledger = self.writer.store().ledger();
        (0..ledger.next_serial())
            .filter_map(|serial| ledger.piece(serial))
            .filter(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
            .count()
    }
}

#[test]
fn launch_and_discovery_orders_keep_one_startup_and_one_post() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for order in [
        "launch_first",
        "discovery_twice",
        "discovery_then_launch",
        "concurrent",
    ] {
        let mut fixture = Fixture::new();
        if order == "concurrent" {
            let barrier = std::sync::Barrier::new(3);
            let root = fixture.runtime.path();
            let tmux = &fixture.tmux;
            let binding = fixture.binding();
            std::thread::scope(|scope| {
                let barrier = &barrier;
                let launch = binding.clone();
                scope.spawn(move || {
                    events::set_test_root(Some(root));
                    barrier.wait();
                    dedupe::register_launched_tmux_runtime_binding(tmux, launch);
                    events::set_test_root(None);
                });
                scope.spawn(move || {
                    events::set_test_root(Some(root));
                    barrier.wait();
                    assert!(dedupe::register_rehydrated_tmux_runtime_binding(
                        "codex", tmux, CHANNEL, binding,
                    ));
                    events::set_test_root(None);
                });
                barrier.wait();
            });
        } else {
            if order == "launch_first" {
                dedupe::register_launched_tmux_runtime_binding(&fixture.tmux, fixture.binding());
            }
            assert!(fixture.register(), "{order}");
            if order == "discovery_twice" {
                assert!(fixture.register());
            } else if order == "discovery_then_launch" {
                dedupe::register_launched_tmux_runtime_binding(&fixture.tmux, fixture.binding());
            }
        }
        fixture.step();
        fixture.step();
        assert_eq!(fixture.harness.port.posts(), [ANSWER], "{order}");
        assert_eq!(fixture.posted(), 1, "{order}");
        assert_eq!(
            fixture
                .history()
                .iter()
                .filter(|event| event.cause == events::BindingCause::Startup)
                .count(),
            1,
            "{order}"
        );
    }
}

#[test]
fn unproven_contexts_never_promote_discovery_to_startup() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for case in [
        "schema",
        "provider",
        "nonce",
        "tmux",
        "resume",
        "expected",
        "verified",
        "legacy",
        "owner",
        "channel",
        "marker_absent",
        "marker_unknown",
        "digest",
    ] {
        let mut fixture = Fixture::new();
        match case {
            "schema" => fixture.write_context(|context| context.schema = 2),
            "provider" => fixture.write_context(|context| context.provider = "claude".into()),
            "nonce" => fixture.write_context(|context| {
                context.execution_nonce = uuid::Uuid::new_v4().simple().to_string()
            }),
            "tmux" => {
                fixture.write_context(|context| context.tmux_session = "different-pane".into())
            }
            "resume" => fixture.write_context(|context| context.launch_mode = "resume".into()),
            "expected" => fixture.write_context(|context| {
                context.expected_native_session_id = Some(fixture.id.clone())
            }),
            "verified" => {
                fixture.write_context(|context| context.source_policy = Some("verified".into()))
            }
            "legacy" => fixture.write_context(|context| context.source_policy = None),
            "owner" => fixture
                .write_context(|context| context.owner_runtime_root = "/different-owner".into()),
            "channel" => fixture.write_context(|context| context.channel_id = Some(CHANNEL + 1)),
            "marker_absent" => {
                std::fs::remove_file(tc::session_temp_path(&fixture.tmux, "spawn_nonce")).unwrap()
            }
            "marker_unknown" => {
                let marker = tc::session_temp_path(&fixture.tmux, "spawn_nonce");
                std::fs::remove_file(&marker).unwrap();
                std::fs::create_dir(marker).unwrap();
            }
            "digest" => fixture.write_context(|context| {
                context.first_prompt_digest = Some(format!(
                    "sha256:{:x}",
                    Sha256::digest(b"a different prompt")
                ))
            }),
            _ => unreachable!(),
        }
        let published = fixture.register();
        fixture.step();
        assert_eq!(fixture.harness.port.posts(), Vec::<String>::new(), "{case}");
        assert_eq!(fixture.posted(), 0, "{case}");
        let history = fixture.history();
        assert!(
            history
                .iter()
                .all(|event| event.cause == events::BindingCause::Unknown),
            "{case}: {history:?}"
        );
        if case != "verified" {
            assert!(
                published,
                "definitive context rejection keeps ordinary discovery: {case}"
            );
            assert_eq!(history.len(), 1, "{case}");
            let links = fixture.writer.store().rotation().unwrap().links;
            assert_eq!(links.len(), 1, "{case}");
            assert!(
                links
                    .values()
                    .all(|link| matches!(link.boundary, Boundary::Pending { .. })),
                "{case}"
            );
        }
    }
}

#[test]
fn mismatched_native_rollouts_never_promote_discovery_to_startup() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for case in [
        "native_id",
        "native_source",
        "subagent",
        "created_before_launch",
        "created_missing",
    ] {
        let mut fixture = Fixture::new();
        let mut rows = std::fs::read_to_string(&fixture.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        match case {
            "native_id" => rows[0]["payload"]["id"] = json!(uuid::Uuid::new_v4().to_string()),
            "native_source" => rows[0]["payload"]["source"] = json!("exec"),
            "subagent" => {
                rows[0]["payload"]["source"] =
                    json!({"subagent":{"thread_spawn":{"parent_thread_id":fixture.id}}})
            }
            "created_before_launch" => {
                rows[0]["payload"]["timestamp"] = json!(
                    (fixture.prepared.context.created_at - chrono::Duration::seconds(1))
                        .to_rfc3339()
                )
            }
            "created_missing" => {
                rows[0]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("timestamp");
            }
            _ => unreachable!(),
        }
        fixture.write_rows(&rows);
        let published = fixture.register();
        fixture.step();
        assert_eq!(fixture.harness.port.posts(), Vec::<String>::new(), "{case}");
        assert_eq!(fixture.posted(), 0, "{case}");
        let history = fixture.history();
        assert!(
            history
                .iter()
                .all(|event| event.cause == events::BindingCause::Unknown),
            "{case}: {history:?}"
        );
        if case != "subagent" {
            assert!(
                published,
                "definitive native rejection stays observed: {case}"
            );
            assert_eq!(history.len(), 1, "{case}");
            assert!(
                fixture
                    .writer
                    .store()
                    .rotation()
                    .unwrap()
                    .links
                    .values()
                    .all(|link| matches!(link.boundary, Boundary::Pending { .. })),
                "{case}"
            );
        }
    }
}

#[test]
fn incomplete_native_header_and_prompt_retry_without_poisoning_history() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = Fixture::new();
    std::fs::write(&fixture.path, "{\"type\":\"session_meta\"").unwrap();
    assert!(!fixture.register(), "partial header defers registration");
    assert!(fixture.history().is_empty());
    assert!(dedupe::peek_tmux_runtime_binding(&fixture.tmux).is_none());
    fixture.write_rows(&[fixture.header()]);
    assert!(
        !fixture.register(),
        "complete header without first input still defers"
    );
    assert!(fixture.history().is_empty());
    fixture.write_rows(&[fixture.header(), json!({"type":"response_item","payload":{
        "type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>synthetic launch environment</environment_context>"}]
    }})]);
    assert!(
        !fixture.register(),
        "synthetic environment is not the first actual input"
    );
    assert!(fixture.history().is_empty());
    fixture.write_complete();
    assert!(
        fixture.register(),
        "a later discovery tick retries complete native evidence"
    );
    fixture.step();
    assert_eq!(fixture.harness.port.posts(), [ANSWER]);
    assert_eq!(fixture.posted(), 1);
    assert_eq!(fixture.history().len(), 1);
    assert_eq!(fixture.history()[0].cause, events::BindingCause::Startup);
}

#[test]
fn an_unread_prior_binding_still_holds_a_fresh_startup_source() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = Fixture::new();
    let prior_tmux = format!("{}-prior", fixture.tmux);
    dedupe::register_tmux_channel(&prior_tmux, CHANNEL);
    dedupe::register_tmux_runtime_binding(&prior_tmux, fixture.binding());
    assert_eq!(fixture.history().len(), 1);
    assert_eq!(fixture.history()[0].cause, events::BindingCause::Unknown);
    fixture.restart_at(1);
    assert!(fixture.register());
    fixture.step();
    let history = fixture.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].cause, events::BindingCause::Startup);
    assert_eq!(fixture.harness.port.posts(), Vec::<String>::new());
    assert_eq!(fixture.posted(), 0);
    let links = fixture.writer.store().rotation().unwrap().links;
    assert_eq!(links.len(), 1);
    assert!(
        links
            .values()
            .all(|link| matches!(link.boundary, Boundary::Pending { .. }))
    );
}

#[test]
fn a_stale_rollout_with_the_same_prompt_cannot_become_a_fresh_startup() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = Fixture::new();
    fixture.write_context(|context| context.created_at += chrono::Duration::days(1));
    assert!(fixture.register());
    fixture.step();
    assert_eq!(fixture.harness.port.posts(), Vec::<String>::new());
    assert_eq!(fixture.posted(), 0);
    assert_eq!(fixture.history().len(), 1);
    assert_eq!(fixture.history()[0].cause, events::BindingCause::Unknown);
    let links = fixture.writer.store().rotation().unwrap().links;
    assert_eq!(links.len(), 1);
    assert!(
        links
            .values()
            .all(|link| matches!(link.boundary, Boundary::Pending { .. }))
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        events::forget_channel_for_tests(CHANNEL);
        events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}

#[test]
fn discovery_first_fresh_codex_posts_once_through_production_registration() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = Fixture::new();
    assert!(fixture.register());
    fixture.step();
    assert_eq!(
        fixture.harness.port.posts(),
        [ANSWER],
        "fresh body reaches Discord"
    );
    assert_eq!(
        fixture.posted(),
        1,
        "delivery is durably Posted, not merely captured"
    );
    let history = fixture.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].cause, events::BindingCause::Startup);
    assert_eq!(
        history[0].evidence.hook_event, None,
        "no native hook is needed"
    );
    let links = fixture.writer.store().rotation().unwrap().links;
    assert_eq!(links.len(), 1);
    assert!(
        links
            .values()
            .all(|link| link.boundary == Boundary::Owed { from: 0 })
    );
    assert!(
        !fixture
            .harness
            .alarms
            .taken()
            .iter()
            .any(|alarm| matches!(alarm, WriterAlarm::BoundaryPending { .. }))
    );
    assert!(fixture.register());
    fixture.step();
    assert_eq!(fixture.harness.port.posts(), [ANSWER]);
    assert_eq!(fixture.posted(), 1);
}
