use super::*;
use crate::services::tui_o::shadow::identity::{self, RecordFact};
use crate::services::tui_o::writer::pieces::{SummaryInputs, ToolCardInputs};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/claude_away_summary.jsonl"
    )))
    .unwrap()
}

fn captured(value: &Value) -> CapturedRecord {
    let line = serde_json::to_vec(value).unwrap();
    CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    }
}

fn native(value: &Value) -> Vec<u8> {
    [serde_json::to_vec(value).unwrap(), vec![b'\n']].concat()
}

fn expected() -> String {
    "-# 🧭 recap\n-# The sample worker finished checking the local parser and saved a small report for review.\n-# The next step is to run the focused checks and confirm that the recap remains visible after a new request.".into()
}

fn one_piece(work: &[Derived]) -> &PieceWork {
    assert_eq!(work.len(), 1, "{work:?}");
    let Derived::Piece(piece) = &work[0] else {
        panic!("expected recap piece: {work:?}");
    };
    piece
}

#[test]
fn away_summary_classifies_and_renders_subtext() {
    let value = fixture();
    let body = value["content"].as_str().unwrap();
    assert!((95..=274).contains(&body.chars().count()));
    let facts = identity::classify(ShadowProvider::Claude, &value);
    assert_eq!(facts.len(), 1, "{facts:?}");
    let RecordFact::Unit(native, kind, _) = &facts[0] else {
        panic!("expected independent unit: {facts:?}");
    };
    assert_eq!(native, "away_summary:11111111-1111-4111-8111-111111111143");
    assert_eq!(format!("{kind:?}"), "Recap");
    assert!(identity::classify(ShadowProvider::Codex, &value).is_empty());
    let mut derive = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let row = captured(&value);
    let first = derive.derive(&row);
    let piece = one_piece(&first);
    assert_eq!(piece.payload, expected());
    assert!(
        crate::services::tui_o::repost::o_piece_delivery::PieceKey::new(piece.unit_key.clone(), 0)
            .is_none()
    );
    assert_eq!(
        crate::services::tui_o::repost::o_piece_delivery::kind_name(piece.unit_key.kind),
        "recap"
    );
    let repeated = derive.derive(&row);
    assert_eq!(
        first.len() + repeated.len(),
        1,
        "seal suppresses duplicate output"
    );

    for subtype in ["informational", "turn_duration", "away_summary_changed"] {
        let mut other = value.clone();
        other["subtype"] = json!(subtype);
        assert!(derive.derive(&captured(&other)).is_empty());
    }
    for content in [None, Some(json!(42)), Some(json!({"text":"wrong shape"}))] {
        let mut malformed = value.clone();
        malformed.as_object_mut().unwrap().remove("content");
        if let Some(content) = content {
            malformed["content"] = content;
        }
        let work = UnitDeriver::new(CHANNEL, ShadowProvider::Claude).derive(&captured(&malformed));
        assert!(work.is_empty(), "unknown recap shape is ignored: {work:?}");
    }
    for uuid in [Value::Null, json!(""), json!("   "), json!(17)] {
        let mut malformed = value.clone();
        malformed["uuid"] = uuid;
        let work = UnitDeriver::new(CHANNEL, ShadowProvider::Claude).derive(&captured(&malformed));
        assert!(work.is_empty(), "unknown recap shape is ignored: {work:?}");
    }
    let mut empty = value.clone();
    empty["content"] = json!(" \n\t ");
    assert!(matches!(
        UnitDeriver::new(CHANNEL, ShadowProvider::Claude).derive(&captured(&empty)).as_slice(),
        [Derived::Excluded { reason, .. }] if reason == "empty_payload"
    ));
    for version in 1..8 {
        assert!(
            UnitDeriver::new(CHANNEL, ShadowProvider::Claude)
                .derive_with_version(&row, version)
                .is_empty(),
            "version={version}"
        );
    }
    let mut multiline = value.clone();
    multiline["content"] = json!("first\r\n\r\nlast");
    assert_eq!(
        one_piece(&UnitDeriver::new(CHANNEL, ShadowProvider::Claude).derive(&captured(&multiline)))
            .payload,
        "-# 🧭 recap\n-# first\n-# \n-# last"
    );
    let mut long = value.clone();
    long["content"] = json!("🧭".repeat(2200));
    let work = UnitDeriver::new(CHANNEL, ShadowProvider::Claude).derive(&captured(&long));
    let expected = crate::domain::message_split::split_for_shadow(&format!(
        "-# 🧭 recap\n-# {}",
        "🧭".repeat(2200)
    ));
    assert!(expected.len() > 1);
    assert_eq!(work.len(), expected.len());
    for (item, (text, units)) in work.iter().zip(expected) {
        let Derived::Piece(piece) = item else {
            panic!("{item:?}");
        };
        assert_eq!(piece.payload, text);
        assert!(units <= 2000);
    }
    let mut changed = value;
    changed["content"] = json!("changed recap");
    assert!(matches!(
        derive.derive(&captured(&changed)).as_slice(),
        [Derived::Blocked { .. }]
    ));
}

async fn wait_posts(harness: &Harness, task: &Actor, expected: usize) {
    for _ in 0..20 {
        polls(1).await;
        if harness.port.posts().len() >= expected || task.is_finished() {
            break;
        }
    }
    assert_eq!(
        harness.port.posts().len(),
        expected,
        "{:?}",
        harness.alarms.taken()
    );
}

#[tokio::test(start_paused = true)]
async fn recap_uuid_survives_replay_restart_and_fork() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    let (stop, task) = spawn_with(writer, ShadowProvider::Claude, Arc::clone(&bindings));
    let recap = native(&fixture());
    append(&path, &recap);
    wait_posts(&harness, &task, 1).await;
    append(&path, &recap);
    polls(3).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), [expected()]);

    let (stop, task) = spawn_with(
        harness.writer(),
        ShadowProvider::Claude,
        Arc::clone(&bindings),
    );
    polls(3).await;
    let fork_path = path.with_file_name("fork.jsonl");
    let mut fork_row = fixture();
    fork_row["sessionId"] = json!("fork-session");
    let inherited = native(&fork_row);
    fork_row["uuid"] = json!("22222222-2222-4222-8222-222222222143");
    std::fs::write(&fork_path, [inherited, native(&fork_row)].concat()).unwrap();
    let fork = source_id_for("fork-session", &fork_path).unwrap();
    bindings.commit(bound(
        2,
        Some(&source),
        BindingTarget::Source(fork.clone()),
        BindingCause::Fork,
        Some(&source),
    ));
    wait_posts(&harness, &task, 2).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), [expected(), expected()]);
    assert_eq!(
        *harness.port.prepared_before_post.lock().unwrap(),
        [true, true]
    );
    let store = harness.channel();
    assert_eq!(store.ledger().next_serial(), 2);
    assert!(harness.alarms.taken().is_empty());
}

#[tokio::test(start_paused = true)]
async fn recap_survives_next_prompt_without_turn_side_effects() {
    let recap = captured(&fixture());
    let prompt =
        captured(&json!({"type":"user","uuid":"next-input","message":{"content":"next request"}}));
    let idle = captured(&json!({"type":"system","subtype":"turn_duration"}));
    for open in [false, true] {
        let mut derive =
            UnitDeriver::new(CHANNEL, ShadowProvider::Claude).with_turn_summary(SummaryInputs {
                turn_marks: Default::default(),
                disposed: Default::default(),
                context_window: None,
            });
        if open {
            assert!(derive.derive(&prompt).is_empty());
        } else {
            assert!(derive.derive(&idle).is_empty());
        }
        let pending = derive.derive(&recap);
        let piece = one_piece(&pending);
        assert_eq!(piece.payload, expected());
        let next = derive.derive(&prompt);
        assert!(next.is_empty());
        assert!(
            derive.derive(&idle).is_empty(),
            "recap cannot make a turn footer"
        );
        let harness = Harness::new();
        harness.gate.acquired();
        let mut writer = harness.writer();
        assert_eq!(
            writer.deliver(&pending[0]).await,
            Step::Done,
            "pending recap survives prompt"
        );
        let saved = harness.port.history.lock().unwrap().clone();
        assert_eq!(writer.deliver(&pending[0]).await, Step::Done);
        assert_eq!(harness.port.posts(), [expected()]);
        let after = harness.port.history.lock().unwrap().clone();
        assert_eq!(after.len(), saved.len());
        assert_eq!(after[0].id, saved[0].id);
        assert_eq!(after[0].content, saved[0].content);
    }
    let mut cards =
        UnitDeriver::new(CHANNEL, ShadowProvider::Claude).with_tool_cards(ToolCardInputs {
            known: Default::default(),
            recorded_cards: Default::default(),
            switch: Arc::new(|| true),
        });
    let tool = captured(
        &json!({"type":"assistant","uuid":"tool-row","apiBlockIndex":0,
        "message":{"id":"tool-message","content":[{"type":"tool_use","id":"tool-id","name":"Bash","input":{"command":"pwd"}}]}}),
    );
    assert!(
        cards
            .derive(&tool)
            .iter()
            .all(|item| matches!(item, Derived::Excluded { .. }))
    );
    assert_eq!(one_piece(&cards.derive(&recap)).payload, expected());
    assert_eq!(
        cards.derive(&prompt).len(),
        1,
        "tool card remains pending until the prompt"
    );
    let (harness, path, _) = switched_over(b"");
    harness.gate.acquired();
    let (stop, task) = spawn(harness.writer());
    append(&path, &native(&fixture()));
    wait_posts(&harness, &task, 1).await;
    let saved = harness.port.history.lock().unwrap()[0].clone();
    append(
        &path,
        &native(&json!({"type":"user","uuid":"next","message":{"content":"next"}})),
    );
    append(&path, &row("next-answer", "next answer"));
    append(
        &path,
        &native(&json!({"type":"system","subtype":"turn_duration"})),
    );
    wait_posts(&harness, &task, 3).await;
    polls(3).await;
    halt(stop, task).await;
    let history = harness.port.history.lock().unwrap();
    assert_eq!(history[0].id, saved.id);
    assert_eq!(history[0].content, saved.content);
    assert_eq!(
        harness
            .port
            .posts()
            .iter()
            .filter(|post| **post == expected())
            .count(),
        1
    );
}
