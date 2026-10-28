#![cfg(unix)]

use super::super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
#[cfg(unix)]
use crate::test_support::input_owner::{Attempt, Scope, StrictOwnerEffectGuard, Target};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const CHANNEL: u64 = 7_272_591;
const NAMESPACE: &str = "s5-fixture";
const FRAME: &str = "fixture literal _ 한글\nreply + merged + voice";

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/input_owner_rollback/fixtures")
}

fn store_dir() -> tempfile::TempDir {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn original() -> PendingQueueItem {
    serde_json::from_value(json!({
        "author_id":7,"author_is_bot":true,"message_id":100,"queued_generation":5,
        "source_message_ids":[100,101],"channel_id":CHANNEL,"override_channel_id":91,
        "source_message_queued_generations":[
            {"message_id":100,"queued_generation":5}, {"message_id":101,"queued_generation":6}],
        "source_text_segments":[{"message_id":100,"text":"voice 원문"},
            {"message_id":101,"text":"merged 원문"}],
        "text":"voice 원문\nmerged 원문","reply_context":"reply 원문", "has_reply_boundary":true,
        "pending_uploads":["fixture://asset-a.txt","fixture://asset-b.txt"],
        "voice_announcement":{"transcript":"voice 원문","user_id":"7","utterance_id":"utterance-1",
            "language":"ko","verbose_progress":false,"started_at":null,"completed_at":null,
            "samples_written":128,"control_channel_id":91}
    }))
    .unwrap()
}

fn identity() -> QueueIdentity {
    serde_json::from_value(json!({"primary":100,"queued_generation":5,
        "sources":[{"message_id":100,"queued_generation":5},
            {"message_id":101,"queued_generation":6}]}))
    .unwrap()
}

fn attempt(physical: &str, root: &str) -> v2_types::AttemptIdentity {
    serde_json::from_value(json!({"nonce":"s5-attempt-A","provider":"codex",
        "target":{"host":"tmux","owner":physical,"root":root,"execution_nonce":"execution-A"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,
            "binding_watermark":"before-create"}}))
    .unwrap()
}

fn reserve(owner: &mut v2::Owner, physical: &str, root: &str) -> v2::AttemptRef {
    owner.enqueue_fixture(&original()).unwrap();
    owner
        .reserve(
            attempt(physical, root),
            v2_types::PreparedPayload {
                frame: FRAME.into(),
                digest: "fixture-digest".into(),
                attribution: json!({"author_id":7}),
            },
        )
        .unwrap()
}

fn generated(temp: &Path) -> Vec<(&'static str, String)> {
    let item = original();
    let head = PersistedHeadInjection {
        identity: identity(),
        original: canonical_item(
            &super::super::super::pending_queue_persistence::pending_queue_item_to_intervention(
                item.clone(),
                std::time::SystemTime::now(),
                std::time::Instant::now(),
            ),
            ChannelId::new(CHANNEL),
        ),
        phase: HeadPhase::Unknown,
        attempt: Some(mend::HeadAttempt {
            nonce: "legacy-nonce".into(),
            transcript: "/fixture/legacy.jsonl".into(),
            session_id: "legacy-session".into(),
            file: (3, 4),
            offset: 9,
        }),
    };
    let meta = SnapshotMeta {
        head_injection: Some(head),
        terminal_identities: vec![],
    };
    let array = snapshot_json(vec![item.clone()], None).unwrap();
    let v1 = snapshot_json(vec![item.clone()], Some(&meta)).unwrap();
    let metadata = snapshot_json(
        vec![],
        Some(&SnapshotMeta {
            head_injection: None,
            terminal_identities: vec![identity()],
        }),
    )
    .unwrap();
    let file = temp.join("v2.json");
    let mut owner = v2::Owner::adopt(file.clone(), NAMESPACE.into()).unwrap();
    let key = reserve(&mut owner, "s5-pane", "/fixture/root");
    drop(owner.arm_bootstrap(&key, 1_000).unwrap());
    owner.payload_started(&key, 1_000).unwrap();
    let v2 = std::fs::read_to_string(&file).unwrap();
    owner.tick(31_000).unwrap();
    assert!(owner.snapshot.items.is_empty());
    assert_eq!(owner.snapshot.retained_inputs.len(), 1);
    let retained = std::fs::read_to_string(file).unwrap();
    let intervention =
        super::super::super::pending_queue_persistence::pending_queue_item_to_intervention(
            item,
            std::time::SystemTime::now(),
            std::time::Instant::now(),
        );
    super::super::super::pending_queue_persistence::save_channel_pending_dispatch_marker(
        &crate::services::provider::ProviderKind::Codex,
        NAMESPACE,
        ChannelId::new(CHANNEL),
        &intervention,
        Some(91),
    )
    .unwrap();
    let marker = pending_queue_file_path(
        &crate::services::provider::ProviderKind::Codex,
        NAMESPACE,
        ChannelId::new(CHANNEL),
    )
    .unwrap()
    .with_extension("dispatch");
    vec![
        ("raw-array.json", array),
        ("v1.json", v1),
        ("v2.json", v2),
        ("metadata-only-v1.json", metadata),
        ("metadata-only-v2.json", retained),
        ("legacy.dispatch", std::fs::read_to_string(marker).unwrap()),
        ("unknown.json", "{\"version\":99,\"items\":[]}".into()),
        ("corrupt.json", "{\"version\":2,\"items\":[".into()),
    ]
}

#[test]
fn s5_fixture_writer_goldens_verify_separately_from_generation() {
    let _root = TestRuntimeRootGuard::new();
    let temp = store_dir();
    let actual = generated(temp.path());
    if std::env::var_os("AGENTDESK_S5_GENERATE_GOLDENS").is_some() {
        for (name, content) in actual {
            std::fs::write(directory().join(name), content).unwrap();
        }
        panic!("goldens generated; run verification separately without generation mode");
    }
    for (name, content) in actual {
        let expected = std::fs::read_to_string(directory().join(name)).unwrap();
        if name == "corrupt.json" {
            assert_eq!(content, expected);
            assert!(compat::inspect(&expected).is_err());
            continue;
        }
        let mut actual: Value = serde_json::from_str(&content).unwrap();
        let mut expected: Value = serde_json::from_str(&expected).unwrap();
        if name == "legacy.dispatch" {
            assert!(actual["created_at_wall_time_ms"].as_u64().unwrap() > 0);
            actual
                .as_object_mut()
                .unwrap()
                .remove("created_at_wall_time_ms");
            expected
                .as_object_mut()
                .unwrap()
                .remove("created_at_wall_time_ms");
        }
        assert_eq!(actual, expected, "actual writer drift: {name}");
    }
}

fn expected_queue_identity(expected: &Value) -> Value {
    let generation = expected["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["message_id"] == expected["primary"])
        .unwrap()["queued_generation"]
        .clone();
    json!({"primary":expected["primary"],"queued_generation":generation,"sources":expected["sources"]})
}

fn assert_item_identity(item: &Value, expected: &Value, canonical_v1: bool) {
    let actual = json!({
        "author":item["author_id"],"author_is_bot":item["author_is_bot"],
        "channel":item["channel_id"],"primary":item["message_id"],
        "generation":item["queued_generation"],"source_ids":item["source_message_ids"],
        "sources":item["source_message_queued_generations"],
        "role_override":item["override_channel_id"],"reply_context":item["reply_context"],
        "has_reply_boundary":item["has_reply_boundary"],
        "voice_utterance":item["voice_announcement"]["utterance_id"],"assets":item["pending_uploads"]
    });
    let queue = expected_queue_identity(expected);
    let source_ids: Vec<_> = expected["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["message_id"].clone())
        .collect();
    let role = if canonical_v1 {
        &expected["v1_head_role_override"]
    } else {
        &expected["role_override"]
    };
    assert_eq!(
        actual,
        json!({
            "author":expected["author"],"author_is_bot":expected["author_is_bot"],
            "channel":expected["channel"],"primary":expected["primary"],
            "generation":queue["queued_generation"],"source_ids":source_ids,"sources":expected["sources"],
            "role_override":role,"reply_context":expected["reply_context"],
            "has_reply_boundary":expected["has_reply_boundary"],
            "voice_utterance":expected["voice_utterance"],"assets":expected["assets"]
        }),
        "independent expected identity"
    );
}

fn assert_attempt_identity(input: &Value, expected: &Value) {
    assert_eq!(input["identity"], expected_queue_identity(expected));
    assert_eq!(input["attempt"]["provider"], expected["provider"]);
    assert_eq!(input["attempt"]["nonce"], expected["nonce"]);
    assert_eq!(
        input["attempt"]["target"]["execution_nonce"],
        expected["execution_nonce"]
    );
    assert_item_identity(&input["original"], expected, false);
}

#[test]
fn s5_fixture_legacy_identity_voice_assets_and_metadata_stay_distinct() {
    let _root = TestRuntimeRootGuard::new();
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(directory().join("expected-identity.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(expected["channel"], CHANNEL);
    assert_eq!(expected["namespace"], NAMESPACE);
    use sha2::Digest;
    for (asset, digest) in expected["asset_sha256"].as_object().unwrap() {
        let bytes = std::fs::read(directory().join(asset)).unwrap();
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(bytes)),
            digest.as_str().unwrap()
        );
    }
    for name in [
        "raw-array.json",
        "v1.json",
        "v2.json",
        "metadata-only-v1.json",
        "metadata-only-v2.json",
        "legacy.dispatch",
    ] {
        let raw = std::fs::read_to_string(directory().join(name)).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        match name {
            "raw-array.json" => assert_item_identity(&value[0], &expected, false),
            "legacy.dispatch" => assert_item_identity(&value, &expected, false),
            "metadata-only-v1.json" => {
                assert_eq!(value["items"], json!([]));
                assert_eq!(
                    value["terminal_identities"][0],
                    expected_queue_identity(&expected)
                );
            }
            "metadata-only-v2.json" => {
                assert_eq!(value["namespace"], expected["namespace"]);
                assert_attempt_identity(&value["retained_inputs"][0]["input"], &expected);
            }
            "v2.json" => {
                assert_eq!(value["namespace"], expected["namespace"]);
                assert_item_identity(&value["items"][0], &expected, false);
                assert_attempt_identity(&value["head_injection"], &expected);
            }
            "v1.json" => {
                assert_item_identity(&value["items"][0], &expected, false);
                assert_item_identity(&value["head_injection"]["original"], &expected, true);
                assert_eq!(
                    value["head_injection"]["identity"],
                    expected_queue_identity(&expected)
                );
                let (_, meta) = parse_snapshot(&raw).unwrap();
                let legacy = meta.unwrap().head_injection.unwrap().attempt.unwrap();
                assert_eq!(legacy.nonce, expected["legacy_nonce"].as_str().unwrap());
                assert_eq!((legacy.file, legacy.offset), ((3, 4), 9));
                let migrated = v2::Owner::adopt(directory_copy(&raw), NAMESPACE.into()).unwrap();
                let input = migrated
                    .snapshot
                    .head_injection
                    .as_ref()
                    .expect("legacy Unknown head remains pending until tick");
                assert_eq!(input.phase, HeadPhase::Unknown);
                assert!(migrated.snapshot.retained_inputs.is_empty());
                assert!(
                    input.attempt.is_none(),
                    "legacy marker cannot invent provider/target authority"
                );
                assert_eq!(
                    input.legacy_attempt.as_ref().unwrap().nonce,
                    expected["legacy_nonce"].as_str().unwrap()
                );
                assert_item_identity(
                    &serde_json::to_value(&input.original).unwrap(),
                    &expected,
                    false,
                );
                assert_eq!(
                    serde_json::to_value(&input.identity).unwrap(),
                    expected_queue_identity(&expected)
                );
            }
            _ => unreachable!(),
        }
    }
    for name in ["unknown.json", "corrupt.json"] {
        let raw = std::fs::read_to_string(directory().join(name)).unwrap();
        let path = directory_copy(&raw);
        assert!(v2::Owner::adopt(path.clone(), NAMESPACE.into()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    }
}

fn directory_copy(raw: &str) -> PathBuf {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(format!("s5-{}.json", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, raw).unwrap();
    path
}

#[test]
#[cfg(unix)]
fn s5_fixture_real_writer_proof_transport_and_publish_paths() {
    let _root = TestRuntimeRootGuard::new();
    let temp = store_dir();
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(temp.path());
    let path = temp.path().join("queue.json");
    let watch = Watch::new(temp.path(), Plan::default());
    let target = Target {
        provider: "codex".into(),
        namespace: NAMESPACE.into(),
        host: "tmux".into(),
        root: temp.path().display().to_string(),
        physical: "s5-real-writer".into(),
        execution: "execution-A".into(),
    };
    let guard = StrictOwnerEffectGuard::new(target.clone());
    let mut owner = v2::Owner::adopt(path.clone(), NAMESPACE.into()).unwrap();
    let key = reserve(&mut owner, &target.physical, &target.root);
    let attempt = Attempt {
        target,
        sources: vec![(100, 5), (101, 6)],
        primary: (100, 5),
        nonce: key.attempt.nonce.clone(),
        native_source: serde_json::to_value(&key.attempt.source)
            .unwrap()
            .to_string(),
    };
    guard.entry_reached();
    guard.admission(&attempt);
    assert!(guard.observe_durable(&attempt, &path).is_err());
    let permit = owner.arm(&key, 1_000).unwrap();
    guard.observe_durable(&attempt, &path).unwrap();
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        crate::services::platform::tmux::send_literal(&attempt.target.physical, FRAME).unwrap();
    }
    drop(permit);
    guard.assert_owner_payload(&attempt, FRAME.as_bytes(), 1);
    let events = watch.events();
    assert!(
        events
            .iter()
            .any(|e| e.target == path && e.step == Step::ParentSync && e.ok)
    );
    let history = temp.path().join("history.json");
    crate::services::discord::runtime_store::critical_publish(
        &history,
        "{}",
        crate::services::discord::runtime_store::AtomicWriteContext::new("fixture_history"),
    )
    .unwrap();
    assert_eq!(
        watch
            .events()
            .iter()
            .filter(|e| e.target == history && e.step == Step::FileSync)
            .count(),
        1
    );
}

#[test]
fn s5_fixture_terminal_cutoff_plus_minus_one_and_clock_reversal() {
    let _root = TestRuntimeRootGuard::new();
    let spec: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/input_owner_rollback/fixtures/scenarios.json"
    )))
    .unwrap();
    for case in spec["terminal_clock"]["cases"].as_array().unwrap() {
        let temp = store_dir();
        let file = temp.path().join("queue.json");
        let mut owner = v2::Owner::adopt(file.clone(), NAMESPACE.into()).unwrap();
        let key = reserve(&mut owner, "clock-pane", &file.display().to_string());
        drop(owner.arm(&key, 1_000).unwrap());
        owner.settle(&key, true, 1_000).unwrap();
        owner.tick(case["now_ms"].as_u64().unwrap()).unwrap();
        assert_eq!(
            !owner.snapshot.terminal_identities.is_empty(),
            case["fenced"].as_bool().unwrap()
        );
        assert!(owner.snapshot.items.is_empty() && owner.snapshot.retained_inputs.is_empty());
    }
}

#[test]
fn s5_fixture_durable_crash_cuts_record_real_fault_and_same_ticket() {
    let _root = TestRuntimeRootGuard::new();
    use super::super::v2::source_admission::{AdmitError, Custody};
    for step in [Step::Write, Step::FileSync, Step::ParentSync] {
        let temp = store_dir();
        let path = temp.path().join("queue.json");
        let mut owner = v2::Owner::adopt(path.clone(), NAMESPACE.into()).unwrap();
        let mut custody = Custody::new(&original());
        let watch = Watch::new(
            &path,
            Plan {
                fail: vec![step],
                ..Default::default()
            },
        );
        let result = custody.advance(&mut owner);
        assert!(matches!(
            result,
            Err(AdmitError::NotCommitted(_)) | Err(AdmitError::Pending)
        ));
        let events = watch.events();
        assert!(events.iter().any(|event| event.step == step && !event.ok));
        let before: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            before["items"].as_array().unwrap().len(),
            usize::from(step == Step::ParentSync)
        );
        drop(watch);
        owner.recover().unwrap();
        custody.advance(&mut owner).unwrap();
        let admitted = serde_json::to_value(&owner.snapshot.items).unwrap();
        assert_eq!(admitted.as_array().unwrap().len(), 1);
        assert_eq!(admitted[0]["source_message_ids"], json!([100, 101]));
        assert_eq!(owner.snapshot.admission_receipts.len(), 1);
    }
}

#[test]
fn s5_fixture_throwaway_pg_records_source_nonce_expectations_pg() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("CREATE TEMP TABLE s5_fixture_expectation (source BIGINT PRIMARY KEY, nonce TEXT NOT NULL, content BIGINT NOT NULL)")
            .execute(&mut *connection).await.unwrap();
        let spec: Value = serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"),
            "/tests/input_owner_rollback/fixtures/scenarios.json"))).unwrap();
        let first = spec["inputs"]["first_source"].as_i64().unwrap();
        let count = spec["inputs"]["count"].as_i64().unwrap();
        let written = sqlx::query("INSERT INTO s5_fixture_expectation SELECT $1 + i, 's5-attempt-' || i, 1 FROM generate_series(0::BIGINT, $2 - 1) i")
            .bind(first).bind(count).execute(&mut *connection).await.unwrap().rows_affected();
        assert_eq!(written, 5001);
        let counts: (i64, i64, i64) = sqlx::query_as("SELECT count(*), count(DISTINCT nonce), sum(content)::BIGINT FROM s5_fixture_expectation")
            .fetch_one(&mut *connection).await.unwrap();
        assert_eq!(counts, (5001,5001,5001));
        drop(connection);
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn s5_fixture_publish_barrier_stops_before_durable_admission_returns() {
    use crate::services::discord::runtime_store::durable_probe::Gate;
    let _root = TestRuntimeRootGuard::new();
    let temp = store_dir();
    let path = temp.path().join("queue.json");
    let mut owner = v2::Owner::adopt(path.clone(), NAMESPACE.into()).unwrap();
    let gate = std::sync::Arc::new(Gate::default());
    let watch = Watch::new(
        &path,
        Plan {
            block: Some(gate.clone()),
            ..Default::default()
        },
    );
    let returned = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|threads| {
        let join = threads.spawn(|| {
            owner.enqueue_fixture(&original()).unwrap();
            returned.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let entered = gate.wait_entered(std::time::Duration::from_secs(5));
        let prematurely_returned = returned.load(std::sync::atomic::Ordering::SeqCst);
        gate.release();
        join.join().unwrap();
        assert!(entered, "actual publish must reach the barrier");
        assert!(!prematurely_returned);
    });
    assert_eq!(owner.snapshot.items.len(), 1);
    assert!(
        watch
            .events()
            .iter()
            .any(|event| event.target == path && event.step == Step::ParentSync && event.ok)
    );
}
