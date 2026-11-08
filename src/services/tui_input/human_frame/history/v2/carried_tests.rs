use super::*;
use crate::config::TestRuntimeRootGuard;
use std::io::Write;

struct Fixture {
    _guard: TestRuntimeRootGuard,
    _root: tempfile::TempDir,
    transcript: PathBuf,
    registry: Registry,
    frame: String,
}

impl Fixture {
    fn new() -> Self {
        let guard = TestRuntimeRootGuard::new();
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("native.jsonl");
        std::fs::write(&transcript, "").unwrap();
        Self {
            _guard: guard,
            _root: root,
            transcript,
            registry: Registry::runtime().unwrap(),
            frame: crate::services::tui_input::human_frame::idle("ann", "7", "same body"),
        }
    }

    fn register(&self, primary: u64) -> Value {
        let key = json!({"identity":{"primary":primary,"queued_generation":1,
            "sources":[{"message_id":primary,"queued_generation":1}]},
            "attempt":{"nonce":format!("attempt-{primary}"),"provider":"claude",
                "target":{"host":"tmux","owner":"carried-pane","root":"/tmp","execution_nonce":"execution"},
                "source":{"kind":"existing","session_id":"native","transcript":self.transcript,
                    "file":file_id(&std::fs::metadata(&self.transcript).unwrap()),"offset":self.len()}}});
        self.registry
            .prepare(
                "channel",
                &json!({"identity":key["identity"],
            "attempt":key["attempt"],"original":{"author_id":7},"payload":{"frame":self.frame}}),
            )
            .unwrap();
        key
    }

    fn len(&self) -> u64 {
        std::fs::metadata(&self.transcript).unwrap().len()
    }

    fn user(&self, uuid: &str) -> Value {
        json!({"type":"user","uuid":uuid,"sessionId":"native",
            "message":{"role":"user","content":self.frame}})
    }

    fn append(&self, record: &Value) -> (u64, u64) {
        let start = self.len();
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.transcript)
                .unwrap(),
            "{record}"
        )
        .unwrap();
        (start, self.len())
    }

    fn edit(&self, apply: impl FnOnce(&mut Value)) {
        let mut snapshot = self.registry.json().unwrap();
        apply(&mut snapshot);
        std::fs::write(path().unwrap(), serde_json::to_vec(&snapshot).unwrap()).unwrap();
    }

    fn lookup(&self, primary: u64) -> Option<Consumed> {
        lookup(
            primary,
            "channel",
            "carried-pane",
            "native",
            &self.transcript,
            self.len(),
        )
    }

    fn assert_user(&self, primary: u64, uuid: &str, range: (u64, u64)) {
        let Some(Consumed::User { row }) = self.lookup(primary) else {
            panic!("registered actual user must be consumed");
        };
        assert_eq!(row.namespace, "channel");
        assert_eq!(row.key["identity"]["primary"], primary);
        assert_eq!(row.source.transcript, self.transcript);
        assert_eq!(row.source.target["execution_nonce"], "execution");
        assert_eq!(row.uuid, uuid);
        assert_eq!(row.range, range);
        assert_eq!(
            row.file,
            file_id(&std::fs::metadata(&self.transcript).unwrap())
        );
        let raw = std::fs::read(&self.transcript).unwrap();
        assert_eq!(
            row.row_digest,
            blake3::hash(&raw[range.0 as usize..range.1 as usize])
                .to_hex()
                .as_str()
        );
        assert_eq!(
            row.frame_digest,
            blake3::hash(self.frame.as_bytes()).to_hex().as_str()
        );
    }
}

#[test]
fn carried_unique_user_is_verified_from_real_history() {
    let f = Fixture::new();
    f.register(100);
    let range = f.append(&f.user("actual-a"));
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_user_preserves_registered_trailing_whitespace() {
    let mut f = Fixture::new();
    f.frame.push_str(" \n");
    f.register(100);
    let range = f.append(&f.user("actual-a"));
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_wrapped_user_verifies_exact_frame_bytes() {
    let f = Fixture::new();
    f.register(100);
    let mut record = f.user("actual-a");
    record["message"]["content"] = json!(format!(
        "\n\n<pasted_content id=\"cafe\">\n{}\n</pasted_content id=\"cafe\">\n",
        f.frame
    ));
    let range = f.append(&record);
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_historical_device_renumbering_is_accepted() {
    let f = Fixture::new();
    f.register(100);
    let range = f.append(&f.user("actual-a"));
    f.edit(|s| {
        let entry = &mut s["live"][0];
        let dev = entry["file"][0].as_u64().unwrap() ^ 1;
        entry["file"][0] = json!(dev);
        entry["registration"]["source"]["file"][0] = json!(dev);
        entry["registration"]["key"]["attempt"]["source"]["file"][0] = json!(dev);
    });
    let source: Source = serde_json::from_value(
        f.registry.json().unwrap()["live"][0]["registration"]["source"].clone(),
    )
    .unwrap();
    assert!(read_native_row(&f.transcript, source.file, range.0, CAP, false).is_none());
    assert_eq!(
        super::super::super::attempt_row("claude", "carried-pane", "attempt-100"),
        None
    );
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_human_command_has_separate_evidence() {
    let f = Fixture::new();
    f.register(100);
    let range = f.append(
        &json!({"type":"attachment","uuid":"command-a","sessionId":"native",
        "attachment":{"type":"queued_command","commandMode":"prompt","prompt":f.frame}}),
    );
    let Some(Consumed::Command { row }) = f.lookup(100) else {
        panic!("human queued command must have command evidence");
    };
    assert_eq!(row.range, range);
    assert_eq!(row.uuid, "command-a");
}

#[test]
fn carried_duplicate_primary_is_preserved_before_owner_filtering() {
    let f = Fixture::new();
    f.register(100);
    f.append(&f.user("actual-a"));
    f.edit(|s| {
        let mut duplicate = s["live"][0]["registration"].clone();
        duplicate["namespace"] = json!("foreign-channel");
        duplicate["key"]["attempt"]["nonce"] = json!("foreign-attempt");
        s["planned"].as_array_mut().unwrap().push(duplicate);
    });
    assert!(
        f.lookup(100).is_none(),
        "duplicate primary cannot be hidden by filtering"
    );
}

#[test]
fn carried_entry_attempt_nonce_mismatch_is_preserved() {
    let f = Fixture::new();
    f.register(100);
    f.append(&f.user("actual-a"));
    f.edit(|s| s["live"][0]["id"] = json!("wrong-attempt"));
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_same_nonce_on_another_full_key_is_preserved() {
    let f = Fixture::new();
    f.register(100);
    f.append(&f.user("actual-a"));
    f.register(200);
    f.edit(|s| s["live"][1]["registration"]["key"]["attempt"]["nonce"] = json!("attempt-100"));
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_foreign_session_is_preserved() {
    let f = Fixture::new();
    f.register(100);
    let mut record = f.user("foreign");
    record["sessionId"] = json!("other");
    f.append(&record);
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_unbound_pre_source_is_preserved() {
    let f = Fixture::new();
    let key = f.register(100);
    f.edit(|s| {
        let mut registration = s["live"][0]["registration"].clone();
        registration["source"] = Value::Null;
        registration["key"]["attempt"]["source"] = json!({"kind":"pre_source"});
        s["live"] = json!([]);
        s["planned"] = json!([registration]);
    });
    f.append(&f.user("actual-a"));
    assert_eq!(key["identity"]["primary"], 100);
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_v1_without_registration_is_preserved() {
    let f = Fixture::new();
    f.register(100);
    let s = f.registry.json().unwrap();
    std::fs::write(
        path().unwrap(),
        serde_json::to_vec(&json!({"entries":[s["live"][0]]})).unwrap(),
    )
    .unwrap();
    f.append(&f.user("actual-a"));
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_competitor_sidechain_cannot_give_actual_b_to_a() {
    let f = Fixture::new();
    f.register(200);
    f.append(&json!({"type":"progress"}));
    f.register(100);
    let mut sidechain = f.user("sidechain-b");
    sidechain["isSidechain"] = json!(true);
    f.append(&sidechain);
    f.append(&f.user("actual-b"));
    assert!(
        f.lookup(100).is_none(),
        "actual B cannot become A after a sidechain row"
    );
}

#[test]
fn carried_competitor_foreign_sid_cannot_give_actual_b_to_a() {
    let f = Fixture::new();
    f.register(200);
    f.append(&json!({"type":"progress"}));
    f.register(100);
    let mut foreign = f.user("foreign-b");
    foreign["sessionId"] = json!("other");
    f.append(&foreign);
    f.append(&f.user("actual-b"));
    assert!(
        f.lookup(100).is_none(),
        "actual B cannot become A after a foreign SID row"
    );
}

#[test]
fn carried_competitor_partial_paste_cannot_give_actual_b_to_a() {
    let f = Fixture::new();
    f.register(200);
    f.append(&json!({"type":"progress"}));
    f.register(100);
    let mut partial = f.user("partial-b");
    partial["message"]["content"] = json!(format!("{}\n</pasted_content id=\"cafe\">", f.frame));
    f.append(&partial);
    f.append(&f.user("actual-b"));
    assert!(
        f.lookup(100).is_none(),
        "a partial paste cannot consume B's slot and give its actual row to A"
    );
}

#[test]
fn carried_passive_competitor_at_equal_offset_preserves_a() {
    let f = Fixture::new();
    let b = f.register(200);
    f.register(100);
    f.registry
        .project(&json!({"namespace":"channel", "head_injection":null,
        "retained_inputs":[{"input":{"identity":b["identity"],"attempt":b["attempt"]}}],
        "terminal_identities":[]}))
        .unwrap();
    f.append(&f.user("actual-b"));
    assert!(
        f.lookup(100).is_none(),
        "live/passive ordering cannot reassign B to A"
    );
}

#[test]
fn carried_pairing_keeps_consumption_before_a_offset() {
    let f = Fixture::new();
    f.register(200);
    f.append(&f.user("actual-b"));
    f.register(100);
    let range = f.append(&f.user("actual-a"));
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_distinct_raw_frames_with_same_class_keep_their_owners() {
    let mut f = Fixture::new();
    let plain = f.frame.clone();
    f.frame.push(' ');
    f.register(200);
    f.append(&f.user("actual-b"));
    f.frame = plain;
    f.register(100);
    let range = f.append(&f.user("actual-a"));
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_only_b_consumed_before_a_offset_preserves_a() {
    let f = Fixture::new();
    f.register(200);
    f.append(&f.user("actual-b"));
    f.register(100);
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_exact_scan_and_row_cap_is_accepted() {
    let f = Fixture::new();
    f.register(100);
    let mut record = f.user("actual-a");
    record["padding"] = json!("");
    let padding = CAP as usize - serde_json::to_vec(&record).unwrap().len() - 1;
    record["padding"] = json!("x".repeat(padding));
    let range = f.append(&record);
    assert_eq!(range.1, CAP);
    f.assert_user(100, "actual-a", range);
}

#[test]
fn carried_row_over_cap_is_preserved() {
    let f = Fixture::new();
    f.register(100);
    let mut record = f.user("actual-a");
    record["padding"] = json!("x".repeat(CAP as usize));
    f.append(&record);
    assert!(f.lookup(100).is_none());
}

#[test]
fn carried_cap_truncated_scan_discards_early_match() {
    let f = Fixture::new();
    f.register(100);
    f.append(&f.user("actual-a"));
    let filler = json!({"type":"progress","padding":"x".repeat(CAP as usize)});
    f.append(&filler);
    assert!(
        f.lookup(100).is_none(),
        "early match cannot bypass the full scan cap"
    );
}

#[test]
fn carried_partial_row_at_sealed_boundary_is_preserved() {
    let f = Fixture::new();
    f.register(100);
    f.append(&f.user("actual-a"));
    assert!(
        lookup(
            100,
            "channel",
            "carried-pane",
            "native",
            &f.transcript,
            f.len() - 1
        )
        .is_none()
    );
}

#[test]
fn carried_invalid_source_owner_path_inode_and_frame_are_preserved() {
    for field in [
        "owner",
        "path",
        "inode",
        "hash",
        "execution",
        "provider",
        "offset",
    ] {
        let f = Fixture::new();
        f.register(100);
        f.append(&f.user("actual-a"));
        f.edit(|s| {
            let e = &mut s["live"][0];
            match field {
                "owner" => e["registration"]["source"]["target"]["owner"] = json!("other"),
                "path" => e["path"] = json!(f.transcript.with_extension("foreign")),
                "inode" => e["file"][1] = json!(0),
                "hash" => e["hash"] = json!("wrong"),
                "execution" => e["registration"]["source"]["target"]["execution_nonce"] = json!(""),
                "provider" => e["registration"]["key"]["attempt"]["provider"] = json!("codex"),
                "offset" => e["offset"] = json!(1),
                _ => unreachable!(),
            }
        });
        assert!(f.lookup(100).is_none(), "invalid {field}");
    }
}

#[test]
fn carried_nonhuman_and_unidentified_rows_are_preserved() {
    let f = Fixture::new();
    f.register(100);
    for mut record in [
        json!({"type":"queue-operation","operation":"enqueue","content":f.frame}),
        json!({"type":"attachment","uuid":"bash","sessionId":"native","attachment":{"type":"queued_command","commandMode":"bash","prompt":f.frame}}),
        json!({"type":"attachment","uuid":"task","sessionId":"native","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"<task-notification>done</task-notification>"}}),
        f.user("meta"),
        f.user("summary"),
        f.user(""),
        f.user(" padded-uuid "),
    ] {
        if record["uuid"] == "meta" {
            record["isMeta"] = json!(true);
        }
        if record["uuid"] == "summary" {
            record["isCompactSummary"] = json!(true);
        }
        f.append(&record);
    }
    assert!(f.lookup(100).is_none());
}
