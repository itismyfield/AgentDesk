//! Compaction keeps every reader's answer, a V1 reader refuses V2 from its first bytes, non-scoped
//! readers see V2 entries, and the cost is measured on serialized fixtures.
use super::*;
use crate::config::TestRuntimeRootGuard;
use std::io::Write;
use std::time::Instant;

#[path = "rollback_reader_tests.rs"]
mod rollback_reader;

const NS: &str = "history-test";

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
fn projection(head: Option<&Value>, retained: &[Value], terminal: &[&Value]) -> Value {
    json!({"namespace":NS,"head_injection":head,"retained_inputs":retained.iter().map(|input|json!({"input":input})).collect::<Vec<_>>(),
        "terminal_identities":terminal.iter().map(|key|json!({"identity":key["identity"]})).collect::<Vec<_>>()})
}
fn frame(body: &str) -> String {
    super::super::super::idle("ann", "7", body)
}
fn observed(result: Result<ObservedAttempt, String>) -> String {
    match result {
        Ok(ObservedAttempt::Known(id)) => format!("known:{id}"),
        Ok(ObservedAttempt::Pending) => "pending".into(),
        Ok(ObservedAttempt::None) => "none".into(),
        Err(_) => "err".into(),
    }
}

/// Every reader's answer on one history: registration, source, receipts own and foreign, a
/// contested row in both orders, recognition, and scoped and non-scoped observation.
fn answers(history: &Registry, path: &Path, keys: &[(&Value, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    for (key, frame) in keys {
        out.push(format!(
            "registered={}",
            history.registered(NS, key, path, frame)
        ));
        out.push(format!(
            "source={:?}",
            history.source(NS, key).map(|s| s.offset)
        ));
        out.push(format!("known={}", super::super::known(frame)));
    }
    let (a, fa) = keys[0];
    let (b, fb) = keys[1];
    let (c, _) = keys[2];
    out.push(format!(
        "own-row={:?}",
        history.receipt(NS, a, (path, 0, "codex", Some("row-a"), fa))
    ));
    out.push(format!(
        "other-frame={:?}",
        history.receipt(NS, b, (path, 0, "codex", Some("row-a"), fb))
    ));
    for order in [[b, c], [c, b]] {
        for key in order {
            out.push(format!(
                "contested={:?}",
                history.receipt(NS, key, (path, 0, "user", Some("user-row"), fb))
            ));
        }
    }
    with_attempt(history, NS, a, || {
        out.push(format!(
            "scoped-row={:?}",
            super::super::receipt_row(&a["attempt"]["nonce"].as_str().unwrap(), path)
        ));
        out.push(observed(Ok(super::super::observed_attempt(
            "codex",
            "history-pane",
            &[fa.to_string()],
            Some("row-a"),
        ))));
    })
    .unwrap();
    for (prompt, row) in [(fa, "row-a"), (fb, "other"), ("unrelated", "x")] {
        out.push(observed(super::super::observed_attempt_checked(
            "codex",
            "history-pane",
            &[prompt.to_string()],
            Some(row),
        )));
    }
    out
}

/// A (native row), B (enqueue), C (B's frame) and P (late PreSource) answer alike live, compacted
/// and from a fresh registry; the R reader still opens the file read-only.
#[test]
fn compacted_settled_evidence_keeps_every_reader_answer_across_a_restart() {
    let _root = TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let history = Registry::runtime().unwrap();
    let (fa, fb, fp) = (frame("alpha"), frame("beta"), frame("late bound"));
    let (a, b, c, p) = (
        key(100, &path, false),
        key(200, &path, false),
        key(300, &path, false),
        key(400, &path, true),
    );
    for (k, f) in [(&a, &fa), (&b, &fb), (&p, &fp)] {
        history.prepare(NS, &input(k, f)).unwrap();
    }
    history
        .project(&projection(None, &[input(&p, &fp)], &[]))
        .unwrap();
    history.bind(NS, &p, &source(&p, &path)).unwrap();
    assert_eq!(
        history.receipt(NS, &a, (&path, 0, "codex", Some("row-a"), &fa)),
        Some(true)
    );
    assert_eq!(
        history.receipt(NS, &b, (&path, 0, "enqueue", Some("enq-b"), &fb)),
        Some(true)
    );
    assert_eq!(
        history.receipt(NS, &p, (&path, 0, "codex", Some("row-p"), &fp)),
        Some(true)
    );
    // C comes after B's enqueue, which a second live frame alike would leave contested.
    history.prepare(NS, &input(&c, &fb)).unwrap();
    let keys = [
        (&a, fa.as_str()),
        (&b, fb.as_str()),
        (&c, fb.as_str()),
        (&p, fp.as_str()),
    ];
    let live = answers(&history, &path, &keys);
    history
        .project(&projection(None, &[], &[&a, &b, &c, &p]))
        .unwrap();
    let disk: Value = serde_json::from_slice(&std::fs::read(&history.path).unwrap()).unwrap();
    let passive = disk["passive"].as_array().unwrap();
    assert_eq!(passive.len(), 4);
    for p in passive {
        assert_eq!(p["state"], "settled", "{p}");
        let frame = &p["registration"]["frame"];
        for field in ["author", "body_hash", "header", "screen_prefix"] {
            assert_eq!(frame[field], "", "the wire keeps {field}, emptied: {p}");
            if let Some(entry) = p.get("entry").filter(|e| !e.is_null()) {
                assert_eq!(entry[field], "", "{p}");
            }
        }
    }
    assert_eq!(
        answers(&history, &path, &keys),
        live,
        "settled and compacted"
    );
    let reopened = Registry::at(history.path.clone());
    assert_eq!(
        answers(&reopened, &path, &keys),
        live,
        "a fresh registry decodes the compacted wire"
    );
    let rollback_reader::HistoryRead::ReadOnlyV2(old) =
        rollback_reader::inspect(&std::fs::read(&history.path).unwrap()).unwrap()
    else {
        panic!("the R reader opens compacted V2 read-only");
    };
    assert_eq!(old.passive.len(), 4);
    // A native row of A's frame for a new attempt D: A's settled native event still marks A's row
    // as natively seen, so D takes its own row uncontested.
    let d = key(500, &path, false);
    reopened.prepare(NS, &input(&d, &fa)).unwrap();
    assert_eq!(
        reopened.receipt(NS, &d, (&path, 0, "codex", Some("row-d"), &fa)),
        Some(true),
        "a compacted native event keeps its kind"
    );
}

/// A retained, an abandoned and a migrated V1 entry: only the abandoned registration is
/// compacted; retained screen evidence and a migrated entry keep their text.
#[test]
fn only_settled_or_abandoned_queue_registrations_are_compacted() {
    let _root = TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let legacy = frame("legacy entry");
    super::super::register(("claude", "history-pane", "legacy", "7"), &path, 0, &legacy).unwrap();
    let history = Registry::runtime().unwrap();
    let (r, x) = (key(600, &path, false), key(700, &path, false));
    let (fr, fx) = (frame("retained"), frame("abandoned"));
    history.prepare(NS, &input(&r, &fr)).unwrap();
    history.prepare(NS, &input(&x, &fx)).unwrap();
    let mut abandoned = input(&x, &fx);
    abandoned["phase"] = json!("reserved");
    history
        .project(&json!({"namespace":NS,"head_injection":null,
            "retained_inputs":[{"input":input(&r, &fr)},{"input":abandoned,"disposition":"known_not_sent"}],
            "terminal_identities":[]}))
        .unwrap();
    let disk: Value = serde_json::from_slice(&std::fs::read(&history.path).unwrap()).unwrap();
    let by_state = |state: &str| -> Vec<Value> {
        disk["passive"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["state"] == state)
            .cloned()
            .collect()
    };
    let retained = by_state("retained");
    assert_eq!(
        retained.len(),
        2,
        "the queue's retained input and the V1 entry"
    );
    for p in &retained {
        assert_ne!(p["entry"]["header"], "", "{p}");
    }
    let [abandoned] = by_state(collision::ABANDONED).try_into().unwrap();
    assert_eq!(abandoned["registration"]["frame"]["header"], "");
}

/// A file that opens with the V2 writer's envelope is refused from its first 16 bytes, with no
/// parse; V1, spaced, damaged and future envelopes take the full read with the same results.
#[test]
fn a_v1_reader_refuses_a_large_v2_file_from_its_first_bytes() {
    let _root = TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("history.json");
    let state = fixture(Scenario::AllReceived, 5001, dir.path());
    std::fs::write(&file, encode_compacted(state)).unwrap();
    compat::COST.with(|cost| cost.set((0, 0, 0)));
    let refused = compat::read_legacy(&file).map(|s| s.entries.len());
    let cost = compat::COST.with(|cost| cost.get());
    assert_eq!(refused, Err("frame history V2 is read-only".to_string()));
    assert_eq!(
        cost,
        (0, 16, 0),
        "one bounded read, no parse, no inspection"
    );
    for (raw, legacy) in [
        ("{\"entries\":[]}", Ok(0)),
        (" {\"version\":2,\"live\":[],\"passive\":[]}", Err(())),
        ("{\"version\":3,\"live\":[],\"passive\":[]}", Err(())),
        ("{\"version\":2,", Err(())),
        ("not json", Err(())),
    ] {
        std::fs::write(&file, raw).unwrap();
        let full = compat::inspect(raw.as_bytes()).map(|read| match read {
            compat::HistoryRead::Legacy(store) => store.entries.len(),
            compat::HistoryRead::ReadOnlyV2(_) => usize::MAX,
        });
        let read = compat::read_legacy(&file).map(|s| s.entries.len());
        assert_eq!(read.is_ok(), legacy.is_ok(), "{raw}");
        assert_eq!(
            read.as_ref().ok(),
            full.as_ref().ok().filter(|n| **n != usize::MAX),
            "{raw}"
        );
    }
    std::fs::remove_file(&file).unwrap();
    assert_eq!(compat::read_legacy(&file).map(|s| s.entries.len()), Ok(0));
}

/// A non-scoped observation of a V2 file: an exact row is Known, a candidate (bound or PreSource)
/// Pending, an abandoned frame and an unknown one None, and an unreadable file an error.
#[test]
fn a_non_scoped_observation_reads_v2_entries_and_keeps_errors() {
    let _root = TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let history = Registry::runtime().unwrap();
    let (a, p, x) = (
        key(800, &path, false),
        key(801, &path, true),
        key(802, &path, false),
    );
    let (fa, fp, fx) = (frame("bound"), frame("presource"), frame("abandoned"));
    for (k, f) in [(&a, &fa), (&p, &fp), (&x, &fx)] {
        history.prepare(NS, &input(k, f)).unwrap();
    }
    history
        .receipt(NS, &a, (&path, 0, "codex", Some("row-a"), &fa))
        .unwrap();
    let mut abandoned = input(&x, &fx);
    abandoned["phase"] = json!("reserved");
    history
        .project(&json!({"namespace":NS,"head_injection":null,
            "retained_inputs":[{"input":abandoned,"disposition":"known_not_sent"}],"terminal_identities":[]}))
        .unwrap();
    let ask = |prompt: &str, row: Option<&str>| {
        observed(super::super::observed_attempt_checked(
            "codex",
            "history-pane",
            &[prompt.to_string()],
            row,
        ))
    };
    assert_eq!(ask(&fa, Some("row-a")), "known:nonce800");
    assert_eq!(ask(&fa, Some("other-row")), "pending");
    assert_eq!(ask(&fp, Some("row-p")), "pending", "an unbound PreSource");
    assert_eq!(
        ask(&fx, Some("row-x")),
        "none",
        "abandoned is never a candidate"
    );
    assert_eq!(ask(&frame("unknown"), None), "none");
    std::fs::write(&history.path, "{\"version\":2, damaged").unwrap();
    assert_eq!(
        ask(&fa, Some("row-a")),
        "err",
        "a damaged V2 file stays an error"
    );
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    /// Every attempt received its native row in one transcript.
    AllReceived,
    /// Eight sessions over 77 transcripts, every receipt received.
    ManySessions,
    /// No receipt ever arrived.
    MissingReceipt,
}

/// One real settled passive, as `project` leaves it, before compaction; needs a test runtime root.
fn template(dir: &Path, received: bool) -> Passive {
    let path = dir.join("template.jsonl");
    std::fs::write(&path, "").unwrap();
    let history = Registry::at(path_in_root(&format!(
        "template-{}.json",
        uuid::Uuid::new_v4()
    )));
    let k = key(1, &path, false);
    let f = frame("a measured prompt of ordinary length for one Discord message");
    history.prepare(NS, &input(&k, &f)).unwrap();
    if received {
        history
            .receipt(NS, &k, (&path, 0, "codex", Some("row-1"), &f))
            .unwrap();
    }
    history
        .transaction(|state| {
            let bound = state.live.remove(0);
            Ok(Passive {
                registration: Some(bound.registration),
                entry: Some(bound.entry),
                state: "settled".into(),
                encoded: None,
            })
        })
        .unwrap()
}

/// `n` distinct settled passives of one scenario, uncompacted.
fn fixture(scenario: Scenario, n: u64, dir: &Path) -> Snapshot {
    let seed = template(dir, !matches!(scenario, Scenario::MissingReceipt));
    let paths: Vec<PathBuf> = (0..77)
        .map(|i| {
            let path = dir.join(format!("old-{i}.jsonl"));
            std::fs::write(&path, "").unwrap();
            path
        })
        .collect();
    let passive = (0..n)
        .map(|i| {
            let mut p = seed.clone();
            let r = p.registration.as_mut().unwrap();
            r.key["identity"]["primary"] = json!(10_000 + i);
            r.key["identity"]["sources"][0]["message_id"] = json!(10_000 + i);
            r.key["attempt"]["nonce"] = json!(format!("nonce{}", 10_000 + i));
            let e = p.entry.as_mut().unwrap();
            e.id = format!("nonce{}", 10_000 + i);
            if let Some(event) = e.events.first_mut() {
                event.key = format!("codex:row-{i}");
                event.row = Some(format!("row-{i}"));
            }
            if let Scenario::ManySessions = scenario {
                let at = (i % 77) as usize;
                e.session = format!("history-pane-{}", i % 8);
                e.path = paths[at].clone();
                e.file = file_id(&std::fs::metadata(&paths[at]).unwrap());
            }
            p
        })
        .collect();
    Snapshot {
        version: 2,
        live: vec![],
        planned: vec![],
        passive,
    }
}

fn path_in_root(name: &str) -> PathBuf {
    let dir = crate::config::runtime_root().unwrap().join("runtime");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn encode_compacted(mut state: Snapshot) -> String {
    state.passive.iter_mut().for_each(Passive::compact);
    state.encode().unwrap()
}

/// Bytes before and after compaction, a registration's wall time and parses over the compacted
/// file, and a V1 append bind's parses (none). Printed as `HISTORY_MEASURE` rows.
fn measure(scenario: Scenario) {
    for n in [0, 1024, 5001] {
        let _root = TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let mut full = fixture(scenario, n, dir.path());
        let full_bytes = full.encode().unwrap().len();
        let compact = encode_compacted(fixture(scenario, n, dir.path()));
        // The runtime history, so the V1 append bind below reads this file.
        let history = Registry::runtime().unwrap();
        std::fs::create_dir_all(history.path.parent().unwrap()).unwrap();
        std::fs::write(&history.path, &compact).unwrap();
        let transcript = dir.path().join("new.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let fresh = key(99_999, &transcript, false);
        compat::COST.with(|cost| cost.set((0, 0, 0)));
        let started = Instant::now();
        history
            .prepare(NS, &input(&fresh, &frame("fresh")))
            .unwrap();
        let prepare = started.elapsed();
        let prepare_parses = compat::COST.with(|cost| cost.get().0);
        // First live-to-settled publication of the new attempt, on the warm cache.
        let started = Instant::now();
        history.project(&projection(None, &[], &[&fresh])).unwrap();
        let settle = started.elapsed();
        let settle_parses = compat::COST.with(|cost| cost.get().0) - prepare_parses;
        compat::COST.with(|cost| cost.set((0, 0, 0)));
        let record = json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"t",
            "item":{"type":"UserMessage","id":"r","content":[{"type":"text","text":"x"}]}}});
        let mut native = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(native, "{record}").unwrap();
        let _ = super::super::append::bind_record_at(&transcript, 0, &record);
        let (bind_parses, bind_bytes, bind_inspects) = compat::COST.with(|cost| cost.get());
        let ratio = compact.len() as f64 / full_bytes.max(1) as f64;
        println!(
            "HISTORY_MEASURE scenario={scenario:?} passive={n} bytes_full={full_bytes} bytes_compact={} ratio={ratio:.3} prepare_ms={} prepare_parses={prepare_parses} settle_ms={} settle_parses={settle_parses} bind_parses={bind_parses} bind_inspects={bind_inspects} bind_prefix_bytes={bind_bytes}",
            compact.len(),
            prepare.as_millis(),
            settle.as_millis(),
        );
        assert!(
            prepare_parses <= 1,
            "{scenario:?}/{n}: one parse per cold transaction"
        );
        assert_eq!(
            settle_parses, 0,
            "{scenario:?}/{n}: a warm cache parses nothing"
        );
        assert_eq!(
            (bind_parses, bind_inspects),
            (0, 0),
            "{scenario:?}/{n}: a V1 append bind parses no V2 file without migrated appends"
        );
        // Accepted contract: emptying only text no reader matches keeps at most 84% of the bytes;
        // measured 0.826-0.831.
        if n > 0 {
            assert!(
                ratio <= 0.84,
                "{scenario:?}/{n}: compaction keeps at most 84% of the bytes ({ratio:.3})"
            );
        }
    }
}

#[test]
fn history_cost_all_received_same_file() {
    measure(Scenario::AllReceived);
}

#[test]
fn history_cost_many_sessions_old_files() {
    measure(Scenario::ManySessions);
}

#[test]
fn history_cost_missing_receipt() {
    measure(Scenario::MissingReceipt);
}

/// A retained passive rising to settled, and a PreSource bound after it settled, are compacted
/// in that transaction and keep their recognition.
#[test]
fn rank_rise_and_late_bind_are_compacted_and_still_recognized() {
    let _root = TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let history = Registry::runtime().unwrap();
    let (r, p) = (key(900, &path, false), key(901, &path, true));
    let (fr, fp) = (frame("retained first"), frame("presource settled"));
    history.prepare(NS, &input(&r, &fr)).unwrap();
    history.prepare(NS, &input(&p, &fp)).unwrap();
    history
        .project(&projection(None, &[input(&r, &fr), input(&p, &fp)], &[]))
        .unwrap();
    history.project(&projection(None, &[], &[&r, &p])).unwrap();
    history.bind(NS, &p, &source(&p, &path)).unwrap();
    let disk: Value = serde_json::from_slice(&std::fs::read(&history.path).unwrap()).unwrap();
    for p in disk["passive"].as_array().unwrap() {
        assert_eq!(p["state"], "settled", "{p}");
        assert_eq!(p["registration"]["frame"]["header"], "", "{p}");
        assert_eq!(p["entry"]["header"], "", "{p}");
    }
    assert!(super::super::known(&fr) && super::super::known(&fp));
    assert_eq!(
        Registry::at(history.path.clone()).receipt(NS, &p, (&path, 0, "codex", Some("late"), &fp)),
        Some(true)
    );
}
