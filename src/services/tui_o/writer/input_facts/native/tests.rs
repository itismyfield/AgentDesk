use super::*;
use crate::services::tui_input::human_frame::history::v2::{Registry, Source};
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::writer::input_facts::InputFacts;
use serde_json::json;
use std::io::Write;

struct Fixture {
    _runtime: crate::config::TestRuntimeRootGuard,
    root: tempfile::TempDir,
    binding: SourceBinding,
    history: Registry,
}
impl Fixture {
    fn new() -> Self {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let base = runtime.path().join("runtime");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let path = root.path().join("parent.jsonl");
        std::fs::write(&path, "").unwrap();
        let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
        let history = Registry::at(root.path().join("frames.json"));
        Self {
            root,
            _runtime: runtime,
            history,
            binding: SourceBinding {
                channel_id: 814811,
                provider: ShadowProvider::Claude,
                source: SourceId {
                    session_id: "parent".into(),
                    path,
                    dev,
                    ino,
                },
            },
        }
    }
    fn register(&self, nonce: &str, body: &str) -> Ticket {
        let source = Source {
            target: json!({"host":"herdr","owner":"fixture","execution_nonce":"birth-1"}),
            provider: "claude".into(),
            session_id: "parent".into(),
            transcript: self.binding.source.path.clone(),
            file: [self.binding.source.dev, self.binding.source.ino],
            offset: 0,
            binding_watermark: None,
        };
        let key = json!({"identity":{"primary":8000000000000814811_u64,"sources":[{"message_id":8000000000000814811_u64,"queued_generation":1}]},
            "attempt":{"nonce":nonce,"provider":"claude","target":source.target,
                "source":{"kind":"existing","session_id":"parent","transcript":source.transcript,"file":source.file,"offset":0}}});
        self.history
            .prepare(
                "native-b1",
                &json!({"identity":key["identity"],"attempt":key["attempt"],
            "original":{"author_id":7},"payload":{"frame":body}}),
            )
            .unwrap();
        let ticket = Ticket {
            key,
            channel_id: self.binding.channel_id,
            source: Some(source),
            owning_episode: None,
            extra: Default::default(),
        };
        self.history.prepare_native("native-b1", &ticket).unwrap();
        ticket
    }
    fn append(&self, rows: &[Value]) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.binding.source.path)
            .unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
    }
    fn read(&self) -> Vec<Evidence> {
        let mut facts = InputFacts::open(self.binding.clone()).unwrap();
        facts.observe_native().unwrap();
        facts.poll(u64::MAX).unwrap();
        facts
            .native_evidence(&self.history.native_evidence_registrations().unwrap())
            .unwrap()
    }
}
fn queue(operation: &str, body: &str) -> Value {
    json!({"type":"queue-operation","operation":operation,"content":body})
}
fn user(id: &str, body: &str) -> Value {
    json!({"type":"user","uuid":id,"sessionId":"parent","message":{"content":body}})
}
fn close() -> Value {
    json!({"type":"system","subtype":"turn_duration"})
}
fn command(id: &str, body: &str) -> Value {
    json!({"type":"attachment","uuid":id,"attachment":{"type":"queued_command","prompt":body,"commandMode":"prompt","origin":{"kind":"human"}}})
}
fn absorbed(body: &str) -> Value {
    let mut row = queue("remove", body);
    row["reason"] = json!("absorbed_mid_turn");
    row
}

#[test]
fn native_8148_b1_fifo_external_inputs_and_strict_closer_share_one_capture() {
    let f = Fixture::new();
    let ticket = f.register("fifo", "our input");
    f.append(&[
        user("foreign", "scheduled_task_fire"),
        queue("enqueue", "background notification"),
        queue("enqueue", "our input"),
        close(),
        queue("dequeue", ""),
        user("notification", "background notification"),
        close(),
        queue("dequeue", ""),
        user("ours", "our input"),
    ]);
    assert!(
        f.read().is_empty(),
        "enqueue and another turn's closer do not execute our input"
    );
    f.append(&[json!({"type":"assistant","uuid":"answer","apiBlockIndex":0,"message":{"id":"message","content":[{"type":"text","text":"answer"}],"stop_reason":"end_turn"}})]);
    assert!(
        f.read().is_empty(),
        "assistant end_turn is not the strict closer"
    );
    f.append(&[close(), close()]);
    let proof = f.read();
    assert_eq!(proof.len(), 1);
    assert_eq!(proof[0].ticket, ticket);
    assert_eq!(proof[0].namespace, "native-b1");
    assert_eq!(proof[0].membership_row.as_deref(), Some("ours"));
    assert_eq!(proof[0].membership_range.unwrap().1, proof[0].range.0);
    assert_eq!(proof[0].closer_range.unwrap().1, proof[0].range.1);
    assert_eq!(
        proof[0].run.as_ref().unwrap().native_id.as_deref(),
        Some("ours")
    );
    assert_eq!(proof[0].disposition, Disposition::Executed);
    assert!(proof[0].range.0 > proof[0].run.as_ref().unwrap().start);
}

#[test]
fn native_8148_b1_absorption_and_command_order_never_changes_membership() {
    for order in 0..3 {
        let f = Fixture::new();
        f.register("join", "ours");
        f.append(&[
            user("external", "direct TUI input"),
            queue("enqueue", "ours"),
        ]);
        match order {
            0 => f.append(&[absorbed("ours"), command("command", "ours")]),
            1 => f.append(&[command("command", "ours"), absorbed("ours")]),
            _ => f.append(&[absorbed("ours")]),
        }
        assert!(f.read().is_empty());
        f.append(&[close()]);
        let proof = f.read();
        assert_eq!(proof.len(), 1, "order={order}");
        assert_eq!(
            proof[0].run.as_ref().unwrap().native_id.as_deref(),
            Some("external")
        );
        assert_eq!(proof[0].disposition, Disposition::Executed);
    }
}

#[test]
fn native_8148_b1_withdraw_transfer_popall_and_late_dequeue_keep_next_input() {
    for operation in ["remove", "popAll", "delivered_to_agent"] {
        let f = Fixture::new();
        let first = f.register("withdraw", "first");
        f.register("next", "second");
        f.append(&[user("external", "scheduled"), queue("enqueue", "first")]);
        let mut remove = queue(
            if operation == "popAll" {
                "popAll"
            } else {
                "remove"
            },
            "first",
        );
        if operation == "delivered_to_agent" {
            remove["reason"] = json!(operation);
        }
        f.append(&[remove, queue("enqueue", "second")]);
        if operation == "remove" {
            f.append(&[queue("dequeue", "")]);
        }
        f.append(&[
            close(),
            queue("dequeue", ""),
            user("second", "second"),
            close(),
        ]);
        let proof = f.read();
        assert_eq!(proof.len(), 2, "{operation}");
        let removed = proof.iter().find(|p| p.ticket == first).unwrap();
        assert_eq!(
            removed.disposition,
            if operation == "delivered_to_agent" {
                Disposition::TransferredUnverified
            } else {
                Disposition::WithdrawnNotExecuted
            }
        );
        let next = proof.iter().find(|p| p.ticket != first).unwrap();
        assert_eq!(
            next.run.as_ref().unwrap().native_id.as_deref(),
            Some("second")
        );
    }
}

#[test]
fn native_8148_b2_known_receipt_rejects_the_first_foreign_row() {
    let f = Fixture::new();
    let ticket = f.register("receipt", "same");
    f.append(&[
        queue("enqueue", "same"),
        queue("dequeue", ""),
        user("one", "same"),
        close(),
    ]);
    assert_eq!(
        f.history.receipt(
            "native-b1",
            &ticket.key,
            (&f.binding.source.path, 0, "user", Some("two"), "same")
        ),
        Some(true)
    );
    assert!(
        f.read().is_empty(),
        "the first close cannot override the registered receipt"
    );
    f.append(&[user("two", "same"), close()]);
    let proof = f.read();
    assert_eq!(proof.len(), 1);
    assert_eq!(
        proof[0].run.as_ref().unwrap().native_id.as_deref(),
        Some("two")
    );
    assert_eq!(proof[0].disposition, Disposition::Executed);
}

#[test]
fn native_8148_b2_popall_does_not_create_a_dequeue_debt() {
    let f = Fixture::new();
    let first = f.register("popall", "first");
    f.register("after-popall", "second");
    f.append(&[
        user("ext", "external"),
        queue("enqueue", "first"),
        queue("popAll", ""),
        queue("enqueue", "second"),
        close(),
        queue("dequeue", ""),
        user("second", "second"),
        close(),
    ]);
    let proof = f.read();
    assert_eq!(proof.len(), 2);
    assert_eq!(
        proof
            .iter()
            .find(|p| p.ticket == first)
            .unwrap()
            .disposition,
        Disposition::WithdrawnNotExecuted
    );
    assert_eq!(
        proof
            .iter()
            .find(|p| p.ticket != first)
            .unwrap()
            .run
            .as_ref()
            .unwrap()
            .native_id
            .as_deref(),
        Some("second")
    );
}

#[test]
fn native_8148_b2_taken_and_attached_join_the_next_opener() {
    for taken in [false, true] {
        let f = Fixture::new();
        f.register("next-run", "b");
        f.append(&[user("a", "first"), close(), queue("enqueue", "b")]);
        if taken {
            f.append(&[queue("dequeue", "")]);
        } else {
            f.append(&[absorbed("b")]);
        }
        assert!(f.read().is_empty());
        f.append(&[user("u-b", "a notification turn"),
            json!({"type":"assistant","uuid":"answer","apiBlockIndex":0,"message":{"id":"answer","content":[{"type":"text","text":"answer"}]}}), close()]);
        let proof = f.read();
        assert_eq!(proof.len(), 1, "taken={taken}");
        assert_eq!(
            proof[0].run.as_ref().unwrap().native_id.as_deref(),
            Some("u-b")
        );
        assert_eq!(proof[0].disposition, Disposition::Executed);
    }
}

#[test]
fn native_8148_b1_identical_twins_need_unique_receipt_and_full_source() {
    let f = Fixture::new();
    let ticket = f.register("twin", "same");
    f.append(&[
        queue("enqueue", "same"),
        queue("enqueue", "same"),
        queue("dequeue", ""),
        user("one", "same"),
        close(),
        queue("dequeue", ""),
        user("two", "same"),
        close(),
    ]);
    assert!(f.read().is_empty(), "a first body match is not an owner");
    assert_eq!(
        f.history.receipt(
            "native-b1",
            &ticket.key,
            (&f.binding.source.path, 0, "user", Some("two"), "same")
        ),
        Some(true)
    );
    let proof = f.read();
    assert_eq!(proof.len(), 1);
    assert_eq!(
        proof[0].run.as_ref().unwrap().native_id.as_deref(),
        Some("two")
    );
    let registrations = f.history.native_evidence_registrations().unwrap();
    let mut facts = InputFacts::open(f.binding.clone()).unwrap();
    facts.observe_native().unwrap();
    facts.poll(u64::MAX).unwrap();
    for axis in 0..5 {
        let mut wrong = registrations.clone();
        match axis {
            0 => wrong[0].ticket.channel_id += 1,
            1 => wrong[0].ticket.source.as_mut().unwrap().file[1] += 1,
            2 => wrong[0].ticket.source.as_mut().unwrap().session_id = "other".into(),
            3 => {
                wrong[0].ticket.source.as_mut().unwrap().target["execution_nonce"] =
                    json!("birth-2")
            }
            _ => wrong[0].ticket.source.as_mut().unwrap().offset = u64::MAX,
        }
        assert!(
            facts.native_evidence(&wrong).unwrap().is_empty(),
            "axis={axis}"
        );
    }
    let duplicate = vec![registrations[0].clone(), registrations[0].clone()];
    assert!(
        facts.native_evidence(&duplicate).unwrap().is_empty(),
        "receipt collision stays pending"
    );
}

#[test]
fn native_8148_b1_replay_poll_boundaries_and_replaced_source_are_fail_closed() {
    let f = Fixture::new();
    f.register("replay", "ours");
    f.append(&[
        user("outside", "scheduled"),
        queue("enqueue", "ours"),
        absorbed("ours"),
        close(),
    ]);
    let registered = f.history.native_evidence_registrations().unwrap();
    let mut facts = InputFacts::open(f.binding.clone()).unwrap();
    facts.observe_native().unwrap();
    while !facts.caught_up().unwrap() {
        facts.poll(1).unwrap();
    }
    assert_eq!(facts.native_evidence(&registered).unwrap(), f.read());
    assert_eq!(f.read().len(), 1);
    let mut partial = InputFacts::open_at(
        f.binding.clone(),
        std::fs::metadata(&f.binding.source.path).unwrap().len(),
    )
    .unwrap();
    partial.poll(u64::MAX).unwrap();
    assert!(partial.native_evidence(&registered).is_err());
    let replacement = f.root.path().join("replacement");
    std::fs::write(&replacement, "\n").unwrap();
    std::fs::rename(replacement, &f.binding.source.path).unwrap();
    assert!(facts.poll(u64::MAX).is_err());
    assert!(facts.native_evidence(&registered).is_err());
}

#[test]
fn native_8148_b1_wrapped_frames_reuse_history_rules_and_foreign_twins_stay_pending() {
    for shape in 0..5 {
        let f = Fixture::new();
        let body = "[User: ann] our original";
        f.register("direct", body);
        let wrapped =
            format!("<pasted_content id=\"abcd\">\n{body}\n</pasted_content id=\"abcd\">");
        let mut row = user("direct", &wrapped);
        match shape {
            1 => row["isMeta"] = json!(true),
            2 => row["isCompactSummary"] = json!(true),
            3 => {
                row = command("background", body);
                row["attachment"]["origin"]["kind"] = json!("task_notification");
            }
            _ => {}
        }
        f.append(&[row]);
        if shape == 4 {
            f.append(&[user("twin", body)]);
        }
        f.append(&[close()]);
        assert_eq!(f.read().len(), usize::from(shape == 0), "shape={shape}");
    }
}

#[test]
fn native_8148_b1_no_key_and_stored_outcomes_are_not_fresh_membership_candidates() {
    for not_sent in [true, false] {
        let f = Fixture::new();
        let ticket = f.register("not-a-new-run", "original");
        if not_sent {
            let input = json!({"identity":ticket.key["identity"],"attempt":ticket.key["attempt"],
                "phase":"reserved", "original":{"author_id":7},"payload":{"frame":"original"}});
            f.history.project(&json!({"namespace":"native-b1","head_injection":null,
                "retained_inputs":[{"input":input,"disposition":"known_not_sent"}],"terminal_identities":[]})).unwrap();
        } else {
            f.history
                .record_native_outcome(
                    "native-b1",
                    &ticket,
                    &crate::services::tui_input::human_frame::history::v2::native::Outcome {
                        disposition: Disposition::Executed,
                        range: Some([0, 1]),
                        extra: Default::default(),
                    },
                )
                .unwrap();
        }
        f.append(&[user("copy", "original"), close()]);
        assert!(
            f.history
                .native_evidence_registrations()
                .unwrap()
                .is_empty()
        );
        assert!(f.read().is_empty());
        let boot = crate::services::tui_input::human_frame::history::v2::native::boot_records(
            &f.root.path().join("frames.json"),
            "claude",
        )
        .unwrap();
        assert_eq!(
            boot.len(),
            1,
            "B2 still owns stored result/boot custody consumption"
        );
    }
}
