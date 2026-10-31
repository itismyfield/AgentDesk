use super::*;
use crate::config::TestRuntimeRootGuard;
use std::io::Write;

fn key(id: u64, source: &Path, pre: bool) -> Value {
    json!({"identity":{"primary":id,"queued_generation":5,"sources":[{"message_id":id,"queued_generation":5}]},
        "attempt":{"nonce":format!("nonce{id}"),"provider":"codex", "target":{"host":"tmux","owner":"history-pane","root":"/tmp","execution_nonce":"spawn"},
            "source":if pre { json!({"kind":"pre_source","expected_session_id":"native","expected_transcript":source,"binding_watermark":"watermark"}) }
            else { json!({"kind":"existing","session_id":"native","transcript":source,"file":file_id(&std::fs::metadata(source).unwrap()),"offset":0}) }}})
}
fn input(key: &Value, frame: &str) -> Value {
    json!({"identity":key["identity"],"attempt":key["attempt"],"original":{"author_id":7},"payload":{"frame":frame}})
}
fn source(key: &Value, path: &Path) -> Source {
    Source {
        target: key["attempt"]["target"].clone(),
        provider: "codex".into(),
        session_id: "native".into(),
        transcript: path.into(),
        file: file_id(&std::fs::metadata(path).unwrap()),
        offset: 0,
        binding_watermark: Some("watermark".into()),
    }
}
fn projection(head: Option<&Value>, retained: &[Value], terminal: &[Value]) -> Value {
    json!({"namespace":"history-test","head_injection":head,"retained_inputs":retained.iter().map(|input|json!({"input":input})).collect::<Vec<_>>(),
        "terminal_identities":terminal.iter().map(|key|json!({"identity":key["identity"]})).collect::<Vec<_>>()})
}
fn setup() -> (tempfile::TempDir, PathBuf, Registry) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("native.jsonl");
    std::fs::write(&source, "").unwrap();
    (root, source, Registry::runtime().unwrap())
}

#[test]
fn history_v2_same_body_retained_never_blocks_registration_or_claims_an_ambiguous_row() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let frame = super::super::super::idle("ann", "7", "same input");
    let a = key(100, &path, false);
    let b = key(200, &path, false);
    let ia = input(&a, &frame);
    let ib = input(&b, &frame);
    history.prepare("history-test", &ia).unwrap();
    history
        .project(&projection(None, std::slice::from_ref(&ia), &[]))
        .unwrap();
    history
        .prepare("history-test", &ib)
        .expect("retained identical frame cannot block B registration");
    for keys in [[&a, &b], [&b, &a]] {
        for key in keys {
            assert_eq!(
                history.receipt(
                    "history-test",
                    key,
                    (&path, 0, "codex", Some("ambiguous"), &frame)
                ),
                None,
                "ambiguous native row must remain unowned in both query orders"
            );
        }
    }
    let snapshot = history.json().unwrap();
    assert_eq!(snapshot["live"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["passive"].as_array().unwrap().len(), 1);
    assert!(super::super::known(&frame));
}

#[test]
fn history_v2_planned_binds_once_without_changing_the_full_attempt_key() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    std::fs::remove_file(&path).unwrap();
    let a = key(300, &path, true);
    let frame = "planned frame";
    history.prepare("history-test", &input(&a, frame)).unwrap();
    assert_eq!(
        history.json().unwrap()["planned"].as_array().unwrap().len(),
        1
    );
    let mut scope_entries = 0;
    with_attempt(&history, "history-test", &a, || {
        scope_entries += 1;
    })
    .unwrap();
    assert_eq!(
        scope_entries, 1,
        "planned custody enters bookkeeping scope before Source exists; scope is not an effect permit"
    );
    assert!(history.source("history-test", &a).is_err());
    std::fs::write(&path, "native source\n").unwrap();
    let pin = source(&a, &path);
    let mut wrong = pin.clone();
    wrong.target["execution_nonce"] = json!("other");
    assert!(
        history.bind("history-test", &a, &wrong).is_err(),
        "foreign source must never bind"
    );
    history.bind("history-test", &a, &pin).unwrap();
    history.bind("history-test", &a, &pin).unwrap();
    wrong = pin.clone();
    wrong.session_id = "foreign".into();
    assert!(
        history.bind("history-test", &a, &wrong).is_err(),
        "foreign source must never bind"
    );
    let s = history.json().unwrap();
    assert_eq!(
        s["live"][0]["registration"]["key"], a,
        "binding never mutates attempt identity"
    );
    assert!(s["planned"].as_array().unwrap().is_empty());
    assert!(
        history.source("other-namespace", &a).is_err(),
        "full receipt namespace must match"
    );
    let mut alias = a.clone();
    alias["identity"]["primary"] = json!(301);
    assert_eq!(
        history.receipt(
            "history-test",
            &alias,
            (&path, 0, "codex", Some("row"), frame)
        ),
        None
    );
    assert_eq!(
        history.receipt(
            "other-namespace",
            &a,
            (&path, 0, "codex", Some("row"), frame)
        ),
        None
    );
    assert_eq!(
        history.receipt("history-test", &a, (&path, 0, "codex", Some("row"), frame)),
        Some(true)
    );
    std::fs::rename(&path, path.with_extension("old")).unwrap();
    std::fs::write(&path, "replacement\n").unwrap();
    assert_eq!(
        history.receipt(
            "history-test",
            &a,
            (&path, 0, "codex", Some("other-row"), frame)
        ),
        None
    );
}

#[test]
fn history_v2_enqueue_evidence_survives_slot_return_and_blocks_late_command_misattribution() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let frame = "same Claude frame";
    let a = key(400, &path, false);
    let b = key(401, &path, false);
    let ia = input(&a, frame);
    history.prepare("history-test", &ia).unwrap();
    assert_eq!(
        history.receipt(
            "history-test",
            &a,
            (&path, 0, "enqueue", Some("enqueue-A"), frame)
        ),
        Some(true)
    );
    history
        .project(&projection(None, &[], std::slice::from_ref(&a)))
        .unwrap();
    history.prepare("history-test", &input(&b, frame)).unwrap();
    assert_eq!(
        history.receipt(
            "history-test",
            &b,
            (&path, 1, "command", Some("late-A"), frame)
        ),
        None,
        "late A command cannot confirm same-body B after A slot returned"
    );
    assert_eq!(
        history.receipt(
            "history-test",
            &a,
            (&path, 0, "enqueue", Some("enqueue-A"), frame)
        ),
        Some(true)
    );
}

#[test]
fn history_v2_five_thousand_registrations_return_slots_and_keep_old_recognition() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    std::fs::write(
        &path,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"native\",\"cwd\":\"/tmp\"}}\n",
    )
    .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    // A native submit check reparses the whole history, so it runs only for the first
    // registration, both sides of the slot capacity and the last one.
    let submits = [0, CAPACITY as u64 - 1, CAPACITY as u64, 5000];
    // Batches divide CAPACITY, so registration CAPACITY + 1 needs every earlier slot returned.
    const BATCH: usize = 64;
    let mut pending = Vec::with_capacity(BATCH);
    let started = std::time::Instant::now();
    for id in 0..5001u64 {
        let k = key(10_000 + id, &path, false);
        let frame = super::super::super::idle("ann", "7", &format!("prompt {id}"));
        history.prepare("history-test", &input(&k, &frame)).unwrap();
        if submits.contains(&id) {
            let offset = file.metadata().unwrap().len();
            writeln!(file, "{}", json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"turn", "item":{"type":"UserMessage","id":format!("row-{id}"),"content":[{"type":"input_text","text":frame}]}}})).unwrap();
            file.flush().unwrap();
            let nonce = k["attempt"]["nonce"].as_str().unwrap();
            assert_eq!(
                with_attempt(&history, "history-test", &k, || {
                    crate::services::codex_tui::busy_inject::submitted(&path, offset, nonce)
                })
                .unwrap(),
                Some(Some("turn".into()))
            );
        }
        pending.push(k);
        if pending.len() == BATCH || id == 5000 {
            history.project(&projection(None, &[], &pending)).unwrap();
            pending.clear();
        }
        if matches!(id, 0 | 32 | 256 | 1024 | 4096 | 5000) {
            println!(
                "HISTORY_COST registered={} bytes={} elapsed_ms={}",
                id + 1,
                std::fs::metadata(&history.path).unwrap().len(),
                started.elapsed().as_millis()
            );
        }
    }
    let s = history.json().unwrap();
    assert_eq!(
        s["live"].as_array().unwrap().len(),
        0,
        "completed registrations return every live slot"
    );
    assert_eq!(s["passive"].as_array().unwrap().len(), 5001);
    let first = super::super::super::idle("ann", "7", "prompt 0");
    assert!(
        super::super::known(&first),
        "old frame recognition survives more than 4096 registrations"
    );
    assert_eq!(
        history.receipt(
            "history-test",
            &key(10000, &path, false),
            (&path, 0, "codex", Some("row-0"), &first)
        ),
        Some(true)
    );
}

#[test]
fn history_v2_scope_validates_the_prepared_frame_and_is_removed_after_unwind() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let a = key(700, &path, false);
    let frame = "exact prepared body";
    history.prepare("history-test", &input(&a, frame)).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_attempt(&history, "history-test", &a, || {
            assert!(
                super::super::register(("codex", "history-pane", "nonce700", "7"), &path, 0, frame)
                    .is_ok()
            );
            assert!(
                super::super::register(("codex", "history-pane", "other", "7"), &path, 0, frame)
                    .is_err()
            );
            assert!(
                super::super::register(
                    ("codex", "history-pane", "nonce700", "7"),
                    &path,
                    0,
                    "other body"
                )
                .is_err()
            );
            panic!("scope unwind");
        })
    }));
    assert!(result.is_err());
    assert!(!scoped());
}

#[path = "rollback_reader_tests.rs"]
mod rollback_reader;

fn normalize(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                normalize(value);
            }
        }
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if matches!(key.as_str(), "path" | "transcript" | "expected_transcript")
                    && value.is_string()
                {
                    *value = json!("/source/native.jsonl");
                } else if key == "file" {
                    *value = json!([11, 12]);
                } else {
                    normalize(value);
                }
            }
        }
        _ => {}
    }
}

#[test]
fn history_v2_actual_writer_golden_is_read_only_for_the_approved_rollback_reader() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let a = key(800, &path, false);
    let b = key(801, &path, true);
    let c = key(802, &path, false);
    let ia = input(&a, "retained frame");
    let ib = input(&b, "planned frame");
    let ic = input(&c, "live frame");
    history.prepare("history-test", &ia).unwrap();
    history
        .project(&projection(None, std::slice::from_ref(&ia), &[]))
        .unwrap();
    history.prepare("history-test", &ib).unwrap();
    history.prepare("history-test", &ic).unwrap();
    let before = std::fs::read(&history.path).unwrap();
    assert!(
        serde_json::from_slice::<Store>(&before).is_err(),
        "old V1 reader rejects real writer V2"
    );
    let rollback_reader::HistoryRead::ReadOnlyV2(old) = rollback_reader::inspect(&before).unwrap()
    else {
        panic!("read-only V2")
    };
    assert_eq!((old.version, old.live.len(), old.passive.len()), (2, 1, 1));
    assert!(rollback_reader::read_legacy(&history.path).is_err());
    assert_eq!(
        std::fs::read(&history.path).unwrap(),
        before,
        "rollback reader preserves exact raw bytes"
    );
    let rollback_reader::HistoryRead::Legacy(old) =
        rollback_reader::inspect(b"{\"entries\":[]}").unwrap()
    else {
        panic!("V1 compatibility")
    };
    assert!(old.entries.is_empty());
    let mut golden = serde_json::from_slice::<Value>(&before).unwrap();
    normalize(&mut golden);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/input_owner/history-v2.json");
    if std::env::var_os("ADK_UPDATE_HISTORY_GOLDEN").is_some() {
        std::fs::write(
            &fixture,
            serde_json::to_string_pretty(&golden).unwrap() + "\n",
        )
        .unwrap();
        panic!("history golden regenerated; rerun without ADK_UPDATE_HISTORY_GOLDEN to compare");
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(fixture).unwrap()).unwrap(),
        golden,
        "actual V2 writer golden is pinned"
    );
}

#[test]
fn history_v2_active_cap_excludes_passive_evidence() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let a = key(900, &path, false);
    history.prepare("history-test", &input(&a, "seed")).unwrap();
    history
        .receipt(
            "history-test",
            &a,
            (&path, 0, "codex", Some("seed"), "seed"),
        )
        .unwrap();
    history
        .project(&projection(None, &[], std::slice::from_ref(&a)))
        .unwrap();
    history
        .transaction(|state| {
            let seed = state.passive[0].clone();
            state.passive = (0..CAPACITY)
                .map(|n| {
                    let mut entry = seed.clone();
                    let r = entry.registration.as_mut().unwrap();
                    r.key["identity"]["primary"] = json!(1000 + n);
                    r.key["identity"]["sources"][0]["message_id"] = json!(1000 + n);
                    entry.encoded = None;
                    r.key["attempt"]["nonce"] = json!(format!("old{n}"));
                    entry.entry.as_mut().unwrap().id = format!("old{n}");
                    entry
                })
                .collect();
            Ok(())
        })
        .unwrap();
    history
        .prepare("history-test", &input(&key(999999, &path, false), "fresh"))
        .expect("passive evidence cannot exhaust active slots");
    assert_eq!(history.json().unwrap()["live"].as_array().unwrap().len(), 1);
}

#[test]
fn history_v2_passive_encoding_tracks_late_binding_receipts_and_settlement() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let a = key(950, &path, true);
    let frame = "late bound frame";
    let input = input(&a, frame);
    history.prepare("history-test", &input).unwrap();
    history.project(&projection(None, &[input], &[])).unwrap();
    assert!(
        super::super::known(frame),
        "unbound retained PreSource keeps frame recognition"
    );
    history
        .bind("history-test", &a, &source(&a, &path))
        .unwrap();
    assert_eq!(
        history.receipt(
            "history-test",
            &a,
            (&path, 0, "codex", Some("late-row"), frame)
        ),
        Some(true)
    );
    with_attempt(&history, "history-test", &a, || {
        assert_eq!(super::super::receipt_row("nonce950", &path).as_deref(),Some("late-row"));
        assert!(matches!(super::super::observed_attempt("codex", "history-pane", &[frame.into()], Some("late-row")), ObservedAttempt::Known(id) if id=="nonce950"));
    }).unwrap();
    history
        .project(&projection(None, &[], std::slice::from_ref(&a)))
        .unwrap();
    let disk: Value = serde_json::from_slice(&std::fs::read(&history.path).unwrap()).unwrap();
    assert_eq!(
        disk,
        history.json().unwrap(),
        "cached encoding equals all current in-memory evidence"
    );
    assert_eq!(
        disk,
        Registry::at(history.path.clone()).json().unwrap(),
        "restart sees every late passive change"
    );
    assert_eq!(disk["passive"][0]["state"], "settled");
    assert_eq!(disk["passive"][0]["entry"]["events"][0]["row"], "late-row");
}

#[path = "v2/s4_tests.rs"]
mod s4;
