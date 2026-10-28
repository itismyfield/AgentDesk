//! A durable Codex End plus its exact first fresh successor Seal proves only that applied hop.

use std::time::{Duration, SystemTime};

use super::*;
use crate::services::tui_o::store::codex_end::{
    self, Appended, CodexEnd, ContextSnapshot, EndBasis, SuccessorSeal,
};
use crate::services::tui_o::writer::ReaderConfig;
use crate::services::tui_o::writer::rotation::{RETIRE_QUIET, Sources, probe};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, PreparedIncarnation, stable_host_identity,
};

struct Contexts {
    _env: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
    _host: crate::config::TestEnvVarGuard,
    _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

fn contexts() -> Contexts {
    let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let env =
        crate::services::tui_prompt_dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new("codex-end-proof-host"),
    );
    Contexts {
        _env: env,
        _host: host,
        _lock: lock,
    }
}

fn launched(root: &Path) -> BindingContext {
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now() - chrono::TimeDelta::seconds(5),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: "tmux".into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: stable_host_identity(),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: Some("shadow".into()),
        claude_resume: None,
    };
    PreparedIncarnation::create(context.clone()).unwrap();
    context
}

fn metadata(session: &str) -> Vec<u8> {
    let at = Utc::now().to_rfc3339();
    codex_line(serde_json::json!({"type": "session_meta", "timestamp": at,
        "payload": {"id": session, "timestamp": at, "cwd": "/",
            "source": "cli", "originator": "codex_cli_rs"}}))
}

fn answer(id: &str, text: &str) -> Vec<u8> {
    codex_line(serde_json::json!({"type": "response_item",
        "timestamp": Utc::now().to_rfc3339(), "payload": {
            "type": "message", "role": "assistant", "id": id,
            "content": [{"type": "output_text", "text": text}]}}))
}

fn native(root: &Path, label: &str, body: &[u8]) -> (PathBuf, SourceId) {
    let session = uuid::Uuid::new_v4().hyphenated().to_string();
    let path = root
        .canonicalize()
        .unwrap()
        .join(format!("rollout-{label}-{session}.jsonl"));
    std::fs::write(&path, [metadata(&session), body.to_vec()].concat()).unwrap();
    let source = source_id_for(&session, &path).unwrap();
    (path, source)
}

struct Fixture {
    harness: Harness,
    old_path: PathBuf,
    old: SourceId,
    old_context: BindingContext,
    new_path: PathBuf,
    new: SourceId,
    new_context: BindingContext,
    bindings: Arc<FakeBindings>,
}

impl Fixture {
    fn new() -> Self {
        let mut initial = None;
        let harness = Harness::build_channel(CHANNEL, |root| {
            let (path, source) = native(root, "old", &answer("before", "before"));
            let bytes = std::fs::read(&path).unwrap();
            initial = Some((path, source.clone()));
            vec![InitSource {
                source_id: source,
                delivery_start: bytes.len() as u64,
                prefix_hash: hex::encode(Sha256::digest(&bytes)),
            }]
        });
        let (old_path, old) = initial.unwrap();
        let root = old_path.parent().unwrap();
        let old_context = launched(root);
        let new_context = launched(root);
        let (new_path, new) = native(root, "new", &answer("new", "new first"));
        let bindings = Arc::new(FakeBindings::new());
        bindings.commit(codex_hop(1, None, &old, &old_context));
        harness.gate.acquired();
        Self {
            harness,
            old_path,
            old,
            old_context,
            new_path,
            new,
            new_context,
            bindings,
        }
    }

    fn end(&self) -> CodexEnd {
        CodexEnd {
            end_id: uuid::Uuid::new_v4().simple().to_string(),
            channel: CHANNEL,
            provider: "codex".into(),
            tmux_session: "tmux".into(),
            execution_nonce: self.old_context.execution_nonce.clone(),
            host: self.old_context.host.clone().unwrap(),
            owner_runtime_root: self.old_context.owner_runtime_root.clone(),
            provider_root: self.old_path.parent().unwrap().to_path_buf(),
            source: self.old.clone(),
            binding_seq: 1,
            captured_at: Utc::now(),
            ended_at: Utc::now(),
            basis: EndBasis::ExplicitClear,
            context: ContextSnapshot::from(&self.old_context),
            successor: None,
        }
    }

    fn seal(&self, seq: u64) -> SuccessorSeal {
        SuccessorSeal {
            execution_nonce: self.new_context.execution_nonce.clone(),
            binding_seq: seq,
            source: self.new.clone(),
            context: ContextSnapshot::from(&self.new_context),
        }
    }

    fn publish(&self, end: &CodexEnd, seal: Option<&SuccessorSeal>) {
        let root = self.harness._runtime.path();
        assert_eq!(codex_end::append(root, end), Ok(Appended::Durable));
        if let Some(seal) = seal {
            assert_eq!(
                codex_end::seal(root, CHANNEL, &end.end_id, seal),
                Ok(Appended::Durable)
            );
        }
    }

    fn bind(&self) {
        self.bindings
            .commit(codex_hop(2, Some(&self.old), &self.new, &self.new_context));
    }

    fn engine(&self, configured: bool, at: SystemTime) -> ReaderEngine {
        let mut sources = Sources::new(CHANNEL, ShadowProvider::Codex, self.bindings.clone());
        if configured {
            sources = sources.with_readers(ReaderConfig { dormant: true });
        }
        let mut writer = self.harness.writer();
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let mut owed = VecDeque::new();
        sources
            .resume_at(&mut writer, &mut deriver, &mut owed, at)
            .unwrap();
        sources.follow(&mut writer).unwrap();
        (sources, writer, deriver, owed)
    }
}

fn codex_hop(
    seq: u64,
    old: Option<&SourceId>,
    new: &SourceId,
    context: &BindingContext,
) -> BindingEvent {
    let mut event = hop_at(seq, "tmux", old, new, BindingCause::Startup, "");
    event.provider = ShadowProvider::Codex;
    event.execution_nonce = context.execution_nonce.clone();
    event
}

#[tokio::test(start_paused = true)]
async fn codex_sealed_direct_successor_proves_a_startup_hop_without_a_provider_clear() {
    let _contexts = contexts();
    for basis in [EndBasis::ExplicitClear, EndBasis::DirectReplacement] {
        let fixture = Fixture::new();
        fixture.bind();
        let mut end = fixture.end();
        end.basis = basis;
        fixture.publish(&end, Some(&fixture.seal(2)));
        let (_sources, _writer, ..) = fixture.engine(false, SystemTime::now());
        assert_eq!(
            successor(&fixture.harness, &fixture.old).proof,
            Some(2),
            "a Startup hop has no provider proof; its exact durable End and Seal supply it"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn codex_pending_successor_proof_keeps_the_hop_seq_distinct_from_the_seal_seq() {
    let _contexts = contexts();
    for configured in [false, true] {
        let fixture = Fixture::new();
        let mut pending = codex_hop(2, Some(&fixture.old), &fixture.new, &fixture.new_context);
        if let BindingRecord::Bound { new, .. } = &mut pending.record {
            *new = BindingTarget::Pending {
                payload_session_id: fixture.new.session_id.clone(),
                payload_transcript_path: fixture.new.path.clone(),
            };
        }
        fixture.bindings.commit(pending);
        let mut resolved = event(
            3,
            BindingRecord::Resolved {
                resolves_seq: 2,
                source: fixture.new.clone(),
            },
            Utc::now(),
        );
        resolved.provider = ShadowProvider::Codex;
        resolved.execution_nonce = fixture.new_context.execution_nonce.clone();
        fixture.bindings.commit(resolved);
        fixture.publish(&fixture.end(), Some(&fixture.seal(3)));
        let now = SystemTime::now();
        let (mut sources, mut writer, mut deriver, mut owed) = fixture.engine(configured, now);
        let next = successor(&fixture.harness, &fixture.old);
        assert_eq!(next.seq, Some(2), "the Bound made the applied hop");
        assert_eq!(next.proof, Some(3), "the Seal names its durable Resolved");
        assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(3));
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        deliver_all(&mut writer, &mut owed).await;
        assert_eq!(fixture.harness.port.posts(), ["new first"]);
        reader_tend(&mut sources, &mut writer, now).await.unwrap();
        assert!(!retired(&fixture.harness, &fixture.old));

        let just_before = RETIRE_QUIET - Duration::from_millis(1);
        tokio::time::advance(just_before).await;
        sources.tend_at(&mut writer, now + just_before).unwrap();
        assert!(
            !retired(&fixture.harness, &fixture.old),
            "the Pending hop still waits at quiet minus 1 ms, configured={configured}"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        let quiet = now + RETIRE_QUIET;
        sources.tend_at(&mut writer, quiet).unwrap();
        if configured {
            assert!(!retired(&fixture.harness, &fixture.old));
            assert!(
                probe::validating(&sources),
                "quiet equality starts prefix validation"
            );
            validated(&sources).await;
            assert!(!retired(&fixture.harness, &fixture.old));
            sources.tend_at(&mut writer, quiet).unwrap();
        }
        assert!(
            retired(&fixture.harness, &fixture.old),
            "the Resolved seal retires the old source at quiet equality, configured={configured}"
        );
        assert!(!retired(&fixture.harness, &fixture.new));
        assert!(!probe::reading(&sources, &fixture.old));
        assert_eq!(successor(&fixture.harness, &fixture.old).proof, Some(3));
    }
}

#[tokio::test(start_paused = true)]
async fn a_historical_codex_seal_proves_its_edge_after_the_pane_binds_a_later_source() {
    let _contexts = contexts();
    let fixture = Fixture::new();
    fixture.bind();
    fixture.publish(&fixture.end(), Some(&fixture.seal(2)));
    let context = launched(fixture.old_path.parent().unwrap());
    let (_, third) = native(
        fixture.old_path.parent().unwrap(),
        "third",
        &answer("third", "third first"),
    );
    fixture
        .bindings
        .commit(codex_hop(3, Some(&fixture.new), &third, &context));
    let (_sources, _writer, ..) = fixture.engine(false, SystemTime::now());
    assert_eq!(successor(&fixture.harness, &fixture.old).proof, Some(2));
    assert_eq!(
        successor(&fixture.harness, &fixture.new).proof,
        None,
        "later binding history neither invalidates nor extends the sealed edge"
    );
}

#[tokio::test(start_paused = true)]
async fn a_codex_seal_never_proves_an_edge_that_a_later_binding_replaced() {
    let _contexts = contexts();
    let fixture = Fixture::new();
    fixture.bind();
    fixture.publish(&fixture.end(), Some(&fixture.seal(2)));
    let context = launched(fixture.old_path.parent().unwrap());
    let (_, third) = native(fixture.old_path.parent().unwrap(), "replacement", b"");
    fixture
        .bindings
        .commit(codex_hop(3, Some(&fixture.old), &third, &context));
    let (_sources, _writer, ..) = fixture.engine(false, SystemTime::now());
    let next = successor(&fixture.harness, &fixture.old);
    assert_eq!(next.source, third);
    assert_eq!(next.seq, Some(3));
    assert_eq!(
        next.proof, None,
        "the seal only names the superseded A to B hop"
    );
}

#[tokio::test(start_paused = true)]
async fn codex_end_only_and_mismatched_identity_evidence_never_prove_a_hop() {
    let _contexts = contexts();
    for case in [
        "end_only",
        "binding_nonce",
        "binding_pane",
        "end_source",
        "seal_source",
        "end_inode",
        "seal_inode",
        "old_snapshot",
        "new_snapshot",
        "host",
        "root",
        "seal_sequence",
        "replaced_native_file",
        "non_cli_native_source",
    ] {
        let fixture = Fixture::new();
        let mut hop = codex_hop(2, Some(&fixture.old), &fixture.new, &fixture.new_context);
        match case {
            "binding_nonce" => hop.execution_nonce = uuid::Uuid::new_v4().simple().to_string(),
            "binding_pane" => hop.tmux_session = "other-pane".into(),
            _ => {}
        }
        fixture.bindings.commit(hop);
        let (mut sources, mut writer, ..) = fixture.engine(false, SystemTime::now());
        assert_eq!(successor(&fixture.harness, &fixture.old).proof, None);
        let mut end = fixture.end();
        let mut seal = fixture.seal(2);
        match case {
            "end_source" => {
                end.source = native(fixture.old_path.parent().unwrap(), "other-old", b"").1
            }
            "seal_source" => {
                seal.source = native(fixture.old_path.parent().unwrap(), "other-new", b"").1
            }
            "end_inode" => end.source.ino += 1,
            "seal_inode" => seal.source.ino += 1,
            "old_snapshot" => end.context.created_at += chrono::TimeDelta::nanoseconds(1),
            "new_snapshot" => seal.context.created_at += chrono::TimeDelta::nanoseconds(1),
            "host" => {
                end.host = "another-host".into();
                end.context.host = Some(end.host.clone());
                seal.context.host = Some(end.host.clone());
            }
            "root" => end.provider_root = end.provider_root.parent().unwrap().to_path_buf(),
            "seal_sequence" => seal.binding_seq = 3,
            _ => {}
        }
        fixture.publish(&end, (case != "end_only").then_some(&seal));
        if case == "replaced_native_file" {
            let saved = fixture.new_path.with_extension("saved");
            std::fs::rename(&fixture.new_path, &saved).unwrap();
            std::fs::copy(&saved, &fixture.new_path).unwrap();
            let replacement = source_id_for(&fixture.new.session_id, &fixture.new_path).unwrap();
            assert_ne!(
                replacement.ino, fixture.new.ino,
                "fixture replaces the inode"
            );
        }
        if case == "non_cli_native_source" {
            let bytes = std::fs::read(&fixture.new_path).unwrap();
            let mut rows = bytes.split_inclusive(|byte| *byte == b'\n');
            let mut meta: serde_json::Value = serde_json::from_slice(rows.next().unwrap()).unwrap();
            meta["payload"]["source"] = "exec".into();
            let rewritten = [codex_line(meta), rows.flatten().copied().collect()].concat();
            std::fs::write(&fixture.new_path, rewritten).unwrap();
        }
        sources.follow(&mut writer).unwrap();
        assert_eq!(
            successor(&fixture.harness, &fixture.old).proof,
            None,
            "{case} must not supply rotation proof"
        );
        assert!(!retired(&fixture.harness, &fixture.old), "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_unsynced_codex_seal_waits_across_restore_and_retries_on_an_idle_follow() {
    use std::io::ErrorKind;

    use crate::services::tui_o::store::fault::{self, Step};

    let _contexts = contexts();
    let fixture = Fixture::new();
    fixture.bind();
    let (mut sources, mut writer, ..) = fixture.engine(false, SystemTime::now());
    let end = fixture.end();
    fixture.publish(&end, None);
    let root = fixture.harness._runtime.path();
    let dir = root.join(codex_end::DIR_NAME);
    let failure = fault::plant(&dir, Step::ReplaceDirSync, ErrorKind::Other, None);
    assert!(matches!(
        codex_end::seal(root, CHANNEL, &end.end_id, &fixture.seal(2)),
        Ok(Appended::PublishedUnsynced(_))
    ));
    let file = dir.join(format!("{CHANNEL}.json"));
    let published = std::fs::read(&file).unwrap();
    assert!(
        codex_end::read(root, CHANNEL).unwrap()[0]
            .successor
            .is_some()
    );
    sources.follow(&mut writer).unwrap();
    assert_eq!(successor(&fixture.harness, &fixture.old).proof, None);
    drop((sources, writer));
    let (mut sources, mut writer, ..) = fixture.engine(false, SystemTime::now());
    assert_eq!(
        successor(&fixture.harness, &fixture.old).proof,
        None,
        "reopening a reader cannot turn published unsynced bytes into proof"
    );
    assert_eq!(std::fs::read(&file).unwrap(), published);
    drop(failure);
    sources.follow(&mut writer).unwrap();
    assert_eq!(
        successor(&fixture.harness, &fixture.old).proof,
        Some(2),
        "an idle poll retries durability without a new binding notification"
    );
    assert_eq!(std::fs::read(file).unwrap(), published);
}

fn ignored_backlog() -> Vec<u8> {
    let bytes: Vec<u8> = (0..10)
        .flat_map(|_| {
            codex_line(serde_json::json!({"type": "response_item", "payload": {
                "type": "reasoning", "summary": [{"type": "summary_text",
                    "text": "x".repeat(256 << 10)}]}}))
        })
        .collect();
    assert!(bytes.len() as u64 > 2 * MAX_READ_BYTES);
    bytes
}

#[tokio::test(start_paused = true)]
async fn a_codex_seal_drains_its_old_source_without_retiring_older_unproven_backlog() {
    let _contexts = contexts();
    let fixture = Fixture::new();
    append(
        &fixture.old_path,
        &[ignored_backlog(), answer("old-tail", "old tail")].concat(),
    );
    fixture.bind();
    let context = launched(fixture.old_path.parent().unwrap());
    let (_, third) = native(
        fixture.old_path.parent().unwrap(),
        "third",
        &answer("third", "third first"),
    );
    fixture
        .bindings
        .commit(codex_hop(3, Some(&fixture.new), &third, &context));
    let mut end = fixture.end();
    end.execution_nonce = fixture.new_context.execution_nonce.clone();
    end.source = fixture.new.clone();
    end.binding_seq = 2;
    end.context = ContextSnapshot::from(&fixture.new_context);
    let seal = SuccessorSeal {
        execution_nonce: context.execution_nonce.clone(),
        binding_seq: 3,
        source: third,
        context: ContextSnapshot::from(&context),
    };
    fixture.publish(&end, Some(&seal));
    let (stop, task) = spawn_with(
        fixture.harness.writer(),
        ShadowProvider::Codex,
        fixture.bindings.clone(),
    );
    tokio::time::sleep(POLL_INTERVAL / 2).await;
    halt(stop, task).await;
    assert!(
        fixture
            .harness
            .channel()
            .cursor(&fixture.old)
            .unwrap()
            .captured_through
            < len(&fixture.old_path),
        "the restart interrupts the older source's multi-read backlog"
    );
    let (stop, task) = spawn_with(
        fixture.harness.writer(),
        ShadowProvider::Codex,
        fixture.bindings.clone(),
    );
    polls(20).await;
    assert_eq!(
        fixture.harness.port.posts(),
        ["old tail", "new first", "third first"]
    );
    assert_eq!(successor(&fixture.harness, &fixture.old).proof, None);
    assert!(!retired(&fixture.harness, &fixture.old));
    assert_eq!(successor(&fixture.harness, &fixture.new).proof, Some(3));
    assert!(retired(&fixture.harness, &fixture.new));
    append(&fixture.old_path, &answer("late-old", "late old"));
    polls(3).await;
    assert_eq!(
        fixture.harness.port.posts().last().map(String::as_str),
        Some("late old")
    );
    assert!(fixture.harness.alarms.taken().is_empty());
    halt(stop, task).await;
}

async fn deliver_all(writer: &mut Writer, owed: &mut VecDeque<Derived>) {
    while let Some(item) = owed.pop_front() {
        writer.deliver(&item).await;
    }
}

async fn validated(sources: &Sources<FakeBindings>) {
    for _ in 0..1000 {
        if probe::ready(sources) {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("prefix validation did not finish");
}

#[tokio::test(start_paused = true)]
async fn twenty_codex_same_source_restores_keep_one_cursor_and_post_each_row_once() {
    let _contexts = contexts();
    for configured in [false, true] {
        let fixture = Fixture::new();
        fixture.bindings.commit(codex_hop(
            2,
            Some(&fixture.old),
            &fixture.old,
            &fixture.new_context,
        ));
        let mut seal = fixture.seal(2);
        seal.source = fixture.old.clone();
        fixture.publish(&fixture.end(), Some(&seal));
        let mut expected = Vec::new();
        for cycle in 0..20 {
            let text = format!("cycle {cycle}");
            append(&fixture.old_path, &answer(&format!("cycle-{cycle}"), &text));
            expected.push(text);
            let now = SystemTime::now();
            let (mut sources, mut writer, mut deriver, mut owed) = fixture.engine(configured, now);
            sources
                .capture(&mut writer, &mut deriver, &mut owed)
                .unwrap();
            deliver_all(&mut writer, &mut owed).await;
            sources.tend_at(&mut writer, now).unwrap();
            assert_eq!(
                fixture.harness.port.posts(),
                expected,
                "cycle {cycle}, configured={configured}"
            );
            assert_eq!(writer.store().cursors().count(), 1, "cycle {cycle}");
            assert!(writer.store().rotation().unwrap().successors.is_empty());
            assert!(!retired(&fixture.harness, &fixture.old));
            assert_eq!(
                writer
                    .store()
                    .cursor(&fixture.old)
                    .unwrap()
                    .captured_through,
                len(&fixture.old_path)
            );
        }
        assert!(fixture.harness.alarms.taken().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn twenty_distinct_codex_fresh_cycles_retire_and_restart_with_one_live_reader() {
    let _contexts = contexts();
    for configured in [false, true] {
        let fixture = Fixture::new();
        let mut wall = SystemTime::now();
        let mut engine = fixture.engine(configured, wall);
        let mut old = fixture.old.clone();
        let mut old_context = fixture.old_context.clone();
        let mut predecessors = Vec::new();
        let mut expected = Vec::new();
        for seq in 2..=21 {
            let context = launched(fixture.old_path.parent().unwrap());
            let text = format!("fresh cycle {seq}");
            let (_, next) = native(
                fixture.old_path.parent().unwrap(),
                &format!("cycle-{seq}"),
                &answer(&format!("fresh-{seq}"), &text),
            );
            assert_ne!(old, next, "every cycle binds a distinct fresh source");
            fixture
                .bindings
                .commit(codex_hop(seq, Some(&old), &next, &context));
            let mut end = fixture.end();
            end.execution_nonce = old_context.execution_nonce.clone();
            end.source = old.clone();
            end.binding_seq = seq - 1;
            end.context = ContextSnapshot::from(&old_context);
            end.basis = if seq % 2 == 0 {
                EndBasis::ExplicitClear
            } else {
                EndBasis::DirectReplacement
            };
            let seal = SuccessorSeal {
                execution_nonce: context.execution_nonce.clone(),
                binding_seq: seq,
                source: next.clone(),
                context: ContextSnapshot::from(&context),
            };
            fixture.publish(&end, Some(&seal));
            expected.push(text);
            {
                let (sources, writer, deriver, owed) = &mut engine;
                sources.follow(writer).unwrap();
                assert_eq!(successor(&fixture.harness, &old).proof, Some(seq));
                sources.capture(writer, deriver, owed).unwrap();
                deliver_all(writer, owed).await;
                wall = wall.max(SystemTime::now());
                reader_tend(sources, writer, wall).await.unwrap();
                assert!(
                    !retired(&fixture.harness, &old),
                    "cycle {seq} waits for quiet"
                );
                let elapsed = RETIRE_QUIET + Duration::from_secs(1);
                tokio::time::advance(elapsed).await;
                wall += elapsed;
                reader_tend(sources, writer, wall).await.unwrap();
                assert!(
                    retired(&fixture.harness, &old),
                    "cycle {seq}, configured={configured}"
                );
                assert_eq!(fixture.harness.port.posts(), expected, "cycle {seq}");
            }
            predecessors.push(old);
            (old, old_context) = (next, context);
            drop(engine);
            engine = fixture.engine(configured, wall);
            let (sources, writer, ..) = &mut engine;
            assert_eq!(
                writer
                    .store()
                    .cursors()
                    .filter(|cursor| !cursor.retired)
                    .count(),
                1,
                "cycle {seq} leaves one unretired source after restart"
            );
            assert_eq!(
                writer
                    .store()
                    .cursors()
                    .filter(|cursor| probe::reading(sources, &cursor.source))
                    .count(),
                1,
                "cycle {seq} leaves one live reader after restart"
            );
            assert!(probe::reading(sources, &old));
            assert_eq!(
                fixture.harness.port.posts(),
                expected,
                "restart {seq} posts no duplicate"
            );
            for (index, source) in predecessors.iter().enumerate() {
                assert_eq!(
                    successor(&fixture.harness, source).proof,
                    Some(index as u64 + 2)
                );
                assert!(retired(&fixture.harness, source));
            }
        }
        let records = codex_end::read(fixture.harness._runtime.path(), CHANNEL).unwrap();
        assert!(!records.is_empty() && records.len() <= 16);
        assert!(
            records.iter().all(|record| record.binding_seq != 1),
            "the first End was evicted while its rotation proof survived every restart"
        );
        assert_eq!(successor(&fixture.harness, &fixture.old).proof, Some(2));
        assert!(retired(&fixture.harness, &fixture.old));
        assert!(fixture.harness.alarms.taken().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn codex_proof_restores_durably_and_uses_quiet_validation_and_regrowth_with_both_reader_policies()
 {
    let _contexts = contexts();
    for configured in [false, true] {
        let fixture = Fixture::new();
        fixture.bind();
        let now = SystemTime::now();
        let (mut sources, mut writer, mut deriver, mut owed) = fixture.engine(configured, now);
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        deliver_all(&mut writer, &mut owed).await;
        reader_tend(&mut sources, &mut writer, now).await.unwrap();
        assert_eq!(successor(&fixture.harness, &fixture.old).proof, None);
        drop((sources, writer, deriver, owed));

        fixture.publish(&fixture.end(), Some(&fixture.seal(2)));
        let (sources, ..) = fixture.engine(configured, now);
        assert_eq!(successor(&fixture.harness, &fixture.old).proof, Some(2));
        assert!(
            probe::reading(&sources, &fixture.old),
            "a late proof wakes an EOF reader on restore"
        );
        assert!(!retired(&fixture.harness, &fixture.old));
        drop(sources);

        // Bounded evidence may later be evicted; the proof already committed to rotation remains.
        std::fs::remove_file(
            fixture
                .harness
                ._runtime
                .path()
                .join(codex_end::DIR_NAME)
                .join(format!("{CHANNEL}.json")),
        )
        .unwrap();
        let (mut sources, mut writer, mut deriver, mut owed) = fixture.engine(configured, now);
        assert_eq!(successor(&fixture.harness, &fixture.old).proof, Some(2));
        assert!(
            probe::reading(&sources, &fixture.old),
            "a persisted proof wakes it again"
        );
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        deliver_all(&mut writer, &mut owed).await;
        assert_eq!(fixture.harness.port.posts(), ["new first"]);
        reader_tend(&mut sources, &mut writer, now).await.unwrap();
        assert!(
            !retired(&fixture.harness, &fixture.old),
            "proof alone does not retire"
        );
        let elapsed = RETIRE_QUIET + Duration::from_secs(1);
        tokio::time::advance(elapsed).await;
        let quiet = now + elapsed;
        sources.tend_at(&mut writer, quiet).unwrap();
        if configured {
            assert!(!retired(&fixture.harness, &fixture.old));
            assert!(
                probe::validating(&sources),
                "quiet retirement must validate the consumed prefix"
            );
            validated(&sources).await;
            assert!(
                !retired(&fixture.harness, &fixture.old),
                "completed validation still needs a tend"
            );
            sources.tend_at(&mut writer, quiet).unwrap();
        }
        assert!(
            retired(&fixture.harness, &fixture.old),
            "configured={configured}"
        );
        assert!(!probe::reading(&sources, &fixture.old));

        append(&fixture.old_path, &answer("late-old", "late old"));
        sources
            .tend_at(&mut writer, quiet + Duration::from_secs(1))
            .unwrap();
        assert!(
            !retired(&fixture.harness, &fixture.old),
            "growth re-arms a retired source"
        );
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        deliver_all(&mut writer, &mut owed).await;
        assert_eq!(fixture.harness.port.posts(), ["new first", "late old"]);
        assert!(matches!(fixture.harness.alarms.taken().as_slice(),
            [WriterAlarm::RetiredSourceGrew { source, .. }] if source == &fixture.old));
        drop((sources, writer, deriver, owed));
        let (mut sources, mut writer, mut deriver, mut owed) =
            fixture.engine(configured, quiet + Duration::from_secs(1));
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        deliver_all(&mut writer, &mut owed).await;
        assert_eq!(
            fixture.harness.port.posts(),
            ["new first", "late old"],
            "restart after regrowth posts no duplicate"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_codex_execution_end_proof_never_bypasses_the_old_prefix_validation() {
    let _contexts = contexts();
    let fixture = Fixture::new();
    fixture.bind();
    fixture.publish(&fixture.end(), Some(&fixture.seal(2)));
    let now = SystemTime::now();
    let (mut sources, mut writer, mut deriver, mut owed) = fixture.engine(true, now);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    reader_tend(&mut sources, &mut writer, now).await.unwrap();
    assert_eq!(successor(&fixture.harness, &fixture.old).proof, Some(2));
    let mut bytes = std::fs::read(&fixture.old_path).unwrap();
    let changed = bytes
        .windows(b"before".len())
        .position(|word| word == b"before")
        .unwrap();
    bytes[changed] = b'B';
    std::fs::write(&fixture.old_path, bytes).unwrap();
    let elapsed = RETIRE_QUIET + Duration::from_secs(1);
    tokio::time::advance(elapsed).await;
    let quiet = now + elapsed;
    sources.tend_at(&mut writer, quiet).unwrap();
    assert!(probe::validating(&sources));
    validated(&sources).await;
    let result = sources.tend_at(&mut writer, quiet);
    assert!(
        matches!(result, Err(WriterAlarm::Halted { .. })),
        "{result:?}"
    );
    assert!(
        !retired(&fixture.harness, &fixture.old),
        "a proven source is never retired with a changed consumed prefix"
    );
}
