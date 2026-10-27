use super::*;
use crate::services::tui_o::repost::admission::{AdmittedKeys, OriginalGate};
use crate::services::tui_o::repost::identity::piece_of;
use crate::services::tui_o::repost::provenance::ProvenanceEntry;
use crate::services::tui_o::store::ledger::{LocalRetryMark, PieceDisposition};

fn captured(value: serde_json::Value) -> CapturedRecord {
    let line = serde_json::to_vec(&value).unwrap();
    CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    }
}

fn tool(id: &str, name: &str) -> CapturedRecord {
    captured(serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "cwd": "/work", "message": {"id": id, "content": [{
            "type": "tool_use", "id": format!("tool-{id}"), "name": name,
            "input": {"command": "cargo check", "file_path": "/work/src/lib.rs"}
        }]}
    }))
}

fn result(id: &str) -> CapturedRecord {
    captured(serde_json::json!({
        "type": "user", "uuid": format!("result-{id}"), "message": {"content": [{
            "type": "tool_result", "tool_use_id": format!("tool-{id}"),
            "is_error": false, "content": "private tool output"
        }]}
    }))
}

fn body(id: &str) -> CapturedRecord {
    captured(serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "text", "text": "answer"}]}
    }))
}

fn tool_key(id: &str) -> UnitKey {
    UnitKey {
        native_key: format!("{id}:0"),
        kind: UnitKind::Tool,
        ..unit("")
    }
}

fn card_key(id: &str) -> UnitKey {
    UnitKey {
        native_key: format!("{id}:0#card"),
        ..tool_key(id)
    }
}

fn deriver(writer: &mut Writer, enabled: Arc<AtomicBool>) -> UnitDeriver {
    let ledger = writer.store().ledger();
    UnitDeriver::new(CHANNEL, ShadowProvider::Claude).with_tool_cards(pieces::ToolCardInputs {
        known: ledger.non_card_tool_keys(),
        recorded_cards: ledger.card_tool_keys(),
        switch: Arc::new(move || enabled.load(Ordering::SeqCst)),
    })
}

async fn deliver_all(writer: &mut Writer, items: &[Derived]) {
    for item in items {
        assert_eq!(writer.deliver(item).await, Step::Done, "{item:?}");
    }
}

fn card(items: &[Derived]) -> PieceWork {
    let cards: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Derived::Piece(piece) if piece.unit_key.native_key.ends_with("#card") => {
                Some(piece.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(cards.len(), 1, "{items:?}");
    cards.into_iter().next().unwrap()
}

#[tokio::test(start_paused = true)]
async fn an_old_panel_disposition_is_never_backfilled_when_cards_default_on() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let old = Derived::Excluded {
        unit_key: tool_key("m1"),
        reason: pieces::TOOL_CALL_PANEL.into(),
    };
    deliver_all(&mut writer, &[old]).await;
    drop(writer);
    let mut writer = harness.writer();
    let mut replay = deriver(&mut writer, Arc::new(AtomicBool::new(true)));
    for record in [tool("m1", "Bash"), result("m1"), body("m2")] {
        deliver_all(&mut writer, &replay.derive(&record)).await;
    }
    assert_eq!(harness.port.posts(), ["answer"]);
    let ledger = writer.store().ledger();
    assert_eq!(
        ledger.excluded(&tool_key("m1")),
        Some(pieces::TOOL_CALL_PANEL)
    );
    assert!(ledger.latest_piece(&card_key("m1"), 0).is_none());
    assert_eq!(ledger.violation(), None);
}

#[tokio::test(start_paused = true)]
async fn off_then_restart_on_keeps_the_old_tool_out_of_the_new_card() {
    let harness = Harness::new();
    harness.gate.acquired();
    let enabled = Arc::new(AtomicBool::new(false));
    let mut writer = harness.writer();
    let mut before = deriver(&mut writer, enabled.clone());
    deliver_all(&mut writer, &before.derive(&tool("m1", "Bash"))).await;
    drop((writer, before));
    enabled.store(true, Ordering::SeqCst);
    let mut writer = harness.writer();
    let mut replay = deriver(&mut writer, enabled);
    for record in [tool("m1", "Bash"), tool("m2", "Read"), result("m2")] {
        deliver_all(&mut writer, &replay.derive(&record)).await;
    }
    let closing = replay.derive(&body("m3"));
    assert_eq!(card(&closing).unit_key, card_key("m2"));
    assert!(!card(&closing).payload.contains("Bash"));
    deliver_all(&mut writer, &closing).await;
    assert_eq!(harness.port.posts().len(), 2);
    let ledger = writer.store().ledger();
    assert!(ledger.latest_piece(&card_key("m1"), 0).is_none());
    assert!(matches!(
        ledger.disposition(&card_key("m2"), 0),
        PieceDisposition::Settled
    ));
}

async fn recorded_card_survives_off_restart(global_off: bool) {
    let harness = Harness::new();
    harness.gate.acquired();
    let global = Arc::new(AtomicBool::new(true));
    let channel = Arc::new(AtomicBool::new(true));
    let mut writer = harness.writer();
    let mut before = deriver(&mut writer, channel.clone());
    let excluded = before.derive(&tool("m1", "Bash"));
    deliver_all(&mut writer, &excluded).await;
    assert_eq!(
        writer.store().ledger().excluded(&tool_key("m1")),
        Some(pieces::TOOL_CALL_CARD)
    );
    if global_off {
        global.store(false, Ordering::SeqCst);
    } else {
        channel.store(false, Ordering::SeqCst);
    }
    drop((writer, before));
    let mut writer = harness.writer();
    let ledger = writer.store().ledger();
    let (global, channel) = (global.clone(), channel.clone());
    let mut replay =
        UnitDeriver::new(CHANNEL, ShadowProvider::Claude).with_tool_cards(pieces::ToolCardInputs {
            known: ledger.non_card_tool_keys(),
            recorded_cards: ledger.card_tool_keys(),
            switch: Arc::new(move || {
                global.load(Ordering::SeqCst) && channel.load(Ordering::SeqCst)
            }),
        });
    for record in [tool("m1", "Bash"), result("m1")] {
        deliver_all(&mut writer, &replay.derive(&record)).await;
    }
    let closing = replay.derive(&body("m2"));
    assert_eq!(card(&closing).unit_key, card_key("m1"));
    deliver_all(&mut writer, &closing).await;
    let next = replay.derive(&tool("m3", "Read"));
    assert!(
        matches!(next.as_slice(), [Derived::Excluded { unit_key, reason }]
        if *unit_key == tool_key("m3") && reason == pieces::TOOL_CALL_PANEL)
    );
    deliver_all(&mut writer, &next).await;
    assert_eq!(harness.port.posts().len(), 2);
    assert_eq!(harness.port.posts()[1], "answer");
    let ledger = writer.store().ledger();
    assert_eq!(
        ledger.latest_piece(&card_key("m1"), 0).unwrap().1.outcome,
        Some(PieceOutcome::Posted(1000))
    );
    drop(writer);
    let mut writer = harness.writer();
    let mut replay = deriver(&mut writer, Arc::new(AtomicBool::new(false)));
    for record in [
        tool("m1", "Bash"),
        result("m1"),
        body("m2"),
        tool("m3", "Read"),
    ] {
        deliver_all(&mut writer, &replay.derive(&record)).await;
    }
    assert_eq!(
        harness.port.posts().len(),
        2,
        "second restart must not duplicate the card"
    );
}

#[tokio::test(start_paused = true)]
async fn a_durable_card_disposition_survives_channel_off_before_restart() {
    recorded_card_survives_off_restart(false).await;
}

#[tokio::test(start_paused = true)]
async fn a_durable_card_disposition_survives_global_off_before_restart() {
    recorded_card_survives_off_restart(true).await;
}

#[tokio::test(start_paused = true)]
async fn a_folded_run_is_reconstructed_once_at_every_durable_crash_boundary() {
    for crash in ["open", "excluded", "prepared", "posted"] {
        let harness = Harness::new();
        harness.gate.acquired();
        let on = Arc::new(AtomicBool::new(true));
        let mut writer = harness.writer();
        let mut before = deriver(&mut writer, on.clone());
        let records = [
            tool("m1", "Bash"),
            result("m1"),
            tool("m2", "Bash"),
            result("m2"),
        ];
        for record in &records {
            let items = before.derive(record);
            if crash != "open" {
                deliver_all(&mut writer, &items).await;
            }
        }
        let closing = before.derive(&body("m3"));
        let expected = card(&closing);
        assert!(expected.payload.contains("×2"), "{expected:?}");
        if crash == "prepared" {
            writer
                .store()
                .append_ledger(LedgerEntry::Prepared {
                    local_retry: Some(LocalRetryMark::Budget),
                    serial: 0,
                    unit_key: expected.unit_key.clone(),
                    piece_index: 0,
                    payload: expected.payload.clone(),
                    anchor_id: 100,
                    epoch: 1,
                })
                .unwrap();
            harness.port.say(BOT, &expected.payload);
        } else if crash == "posted" {
            deliver_all(&mut writer, &[Derived::Piece(expected.clone())]).await;
        }
        drop((writer, before));
        let mut writer = harness.writer();
        let mut replay = deriver(&mut writer, on);
        for record in records.iter().chain(std::iter::once(&body("m3"))) {
            deliver_all(&mut writer, &replay.derive(record)).await;
        }
        let history = harness.port.history.lock().unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|m| m.content == expected.payload)
                .count(),
            1,
            "{crash}"
        );
        assert_eq!(
            history.iter().filter(|m| m.content == "answer").count(),
            1,
            "{crash}"
        );
        let ledger = writer.store().ledger();
        assert!(
            matches!(
                ledger
                    .latest_piece(&expected.unit_key, 0)
                    .unwrap()
                    .1
                    .outcome,
                Some(PieceOutcome::Posted(_))
            ),
            "{crash}"
        );
        assert_eq!(ledger.violation(), None, "{crash}");
    }
}

#[tokio::test(start_paused = true)]
async fn rollback_identity_mode_does_not_recreate_synthetic_card_or_turn_work() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut enabled = deriver(&mut writer, Arc::new(AtomicBool::new(true)));
    let records = [tool("m1", "Bash"), result("m1"), body("m2")];
    for record in &records {
        deliver_all(&mut writer, &enabled.derive(record)).await;
    }
    writer
        .store()
        .append_ledger(LedgerEntry::Excluded {
            unit_key: UnitKey {
                native_key: "m1:0#turn".into(),
                ..tool_key("m1")
            },
            reason: "turn_summary_open".into(),
        })
        .unwrap();
    let summary = PieceWork {
        output_origin: None,
        unit_key: UnitKey {
            native_key: "m1:0#turn_end".into(),
            ..tool_key("m1")
        },
        index: 0,
        payload: "-# ✅ 1초 · 📦 1%".into(),
    };
    deliver_all(&mut writer, &[Derived::Piece(summary.clone())]).await;
    let before = std::fs::read(ledger_path(&harness)).unwrap();
    let posts = harness.port.posts();
    drop(writer);
    let mut writer = harness.writer();
    let mut rollback = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    for record in &records {
        let items = rollback.derive(record);
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, Derived::Blocked { .. }))
        );
        deliver_all(&mut writer, &items).await;
    }
    // Old identity derivation never emits either synthetic kind, while the old ledger reader
    // accepts them without changing the native tool's disposition.
    assert_eq!(harness.port.posts(), posts);
    assert_eq!(std::fs::read(ledger_path(&harness)).unwrap(), before);
    assert_eq!(writer.store().ledger().violation(), None);
    assert!(harness.alarms.taken().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_card_has_no_shared_repost_identity_and_spends_only_one_local_retry() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness.port.say(BOT + 1, "readable history");
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Unsent);
    let mut writer = harness.writer();
    let mut derive = deriver(&mut writer, Arc::new(AtomicBool::new(true)));
    deliver_all(&mut writer, &derive.derive(&tool("m1", "Bash"))).await;
    let work = card(&derive.derive(&body("m2")));
    assert_eq!(
        writer.deliver(&Derived::Piece(work.clone())).await,
        Step::Done
    );
    let record = writer.store().ledger().piece(0).unwrap().clone();
    assert_eq!(record.outcome, Some(PieceOutcome::NotFound));
    assert_eq!(record.local_retry, Some(LocalRetryMark::Budget));
    assert!(
        piece_of(&record).is_none(),
        "there can be no PG admission key"
    );
    assert!(
        ProvenanceEntry::intent(0, &record, 1).is_none(),
        "no shared intent"
    );
    assert_eq!(
        AdmittedKeys::Unreadable("PG unavailable".into()).original(&work.unit_key, 0),
        OriginalGate::SendLocal
    );
    let due = super::super::local_retry::pending(writer.store().ledger())
        .unwrap()
        .due;
    assert_eq!(writer.retry_local(due).await, Step::Done);
    assert_eq!(writer.retry_local(due).await, Step::Done);
    assert_eq!(
        writer.deliver(&Derived::Piece(work.clone())).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), [work.payload.clone(), work.payload]);
    assert_eq!(
        writer.store().ledger().piece(1).unwrap().local_retry,
        Some(LocalRetryMark::Attempt { root_serial: 0 })
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_card_stays_blocked_when_off_until_approval_resends_saved_bytes_once() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(400));
    let mut writer = harness.writer();
    let mut before = deriver(&mut writer, Arc::new(AtomicBool::new(true)));
    deliver_all(&mut writer, &before.derive(&tool("m1", "Bash"))).await;
    let closing = before.derive(&body("m2"));
    let saved = card(&closing);
    assert_eq!(
        writer.deliver(&Derived::Piece(saved.clone())).await,
        Step::Stopped
    );
    assert_eq!(
        writer.store().ledger().piece(0).unwrap().outcome,
        Some(PieceOutcome::Rejected(400))
    );
    assert_eq!(writer.deliver(closing.last().unwrap()).await, Step::Stopped);
    drop(writer);
    let mut writer = harness.writer();
    assert!(writer.is_stopped());
    let mut replay = deriver(&mut writer, Arc::new(AtomicBool::new(false)));
    let _ = replay.derive(&tool("m1", "Bash"));
    let replayed = replay.derive(&body("m2"));
    assert_eq!(card(&replayed), saved, "off cannot change the refused card");
    assert_eq!(
        writer.deliver(&Derived::Piece(saved.clone())).await,
        Step::Stopped
    );
    assert_eq!(harness.port.posts(), [saved.payload.clone()]);
    drop(writer);
    harness
        .store
        .record_operator_resume(CHANNEL, 0, "operator", "retry exact card")
        .unwrap();
    let mut writer = harness.writer();
    let changed = PieceWork {
        payload: "different derived bytes".into(),
        ..saved.clone()
    };
    assert_eq!(writer.deliver(&Derived::Piece(changed)).await, Step::Done);
    assert_eq!(
        writer.deliver(&Derived::Piece(saved.clone())).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), [saved.payload.clone(), saved.payload]);
    assert_eq!(
        writer.store().ledger().approval(0).unwrap().consumed_serial,
        Some(1)
    );
    assert_eq!(writer.store().ledger().violation(), None);
}

#[test]
fn identical_native_records_ledger_and_switch_history_derive_identical_bytes() {
    let harness = Harness::new();
    let mut writer = harness.writer();
    let switch_a = Arc::new(AtomicBool::new(true));
    let switch_b = Arc::new(AtomicBool::new(true));
    let mut a = deriver(&mut writer, switch_a.clone());
    let mut b = deriver(&mut writer, switch_b.clone());
    let records = [
        tool("m1", "Bash"),
        result("m1"),
        tool("m2", "Read"),
        body("m3"),
        tool("m4", "Bash"),
        body("m5"),
    ];
    let mut all = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if index == 2 {
            switch_a.store(false, Ordering::SeqCst);
            switch_b.store(false, Ordering::SeqCst);
        }
        let left = a.derive(record);
        let right = b.derive(record);
        assert_eq!(left, right);
        assert_eq!(
            format!("{left:?}").into_bytes(),
            format!("{right:?}").into_bytes()
        );
        all.extend(left);
    }
    assert_eq!(card(&all).unit_key, card_key("m2"));
}

#[tokio::test(start_paused = true)]
async fn secret_corpus_672_actual_card_posts_hide_sentinels_and_error_result_bodies() {
    let cases: &[(&str, &[&str])] = &[
        (
            "curl -H 'Authorization: Basic dXNlcjpwYXNz' https://example.test",
            &["dXNlcjpwYXNz"],
        ),
        (
            "curl -H 'aUtHoRiZaTiOn: Bearer HEADERLEAK' https://example.test",
            &["HEADERLEAK"],
        ),
        (
            "curl -H 'Authorization: CUSTOMLEAK' https://example.test",
            &["CUSTOMLEAK"],
        ),
        ("echo Basic BASIC_LEAK", &["BASIC_LEAK"]),
        ("echo Bearer BEARER_LEAK", &["BEARER_LEAK"]),
        (
            "mysql --password 'FLAG_HEAD FLAG_TAIL'",
            &["FLAG_HEAD", "FLAG_TAIL"],
        ),
        ("mysql -pSHORTPWD", &["SHORTPWD"]),
        (
            "cat '/Users/SPACE_HEAD SPACE_TAIL/file.rs'",
            &["SPACE_HEAD", "SPACE_TAIL"],
        ),
        (
            "mysql --pass\"word\" OBFUSCATED_VALUE",
            &["OBFUSCATED_VALUE"],
        ),
        ("echo to\"ken\"=OBFUSCATED_TOKEN", &["OBFUSCATED_TOKEN"]),
        ("echo gh\"p\"_QUOTE_LEAK9", &["QUOTE_LEAK9"]),
        (
            "curl ht\"tps\"://QUOTE_USER:QUOTE_PASS@example.test/path",
            &["QUOTE_USER", "QUOTE_PASS"],
        ),
        ("cat /Us\"ers\"/FAKE_ACCOUNT/file.rs", &["FAKE_ACCOUNT"]),
        (
            "mysql --PaSsWoRd='FLAG_HEAD'\"FLAG_TAIL\"",
            &["FLAG_HEAD", "FLAG_TAIL"],
        ),
        (
            "echo \"token=WRAPPED_HEAD WRAPPED_TAIL\"",
            &["WRAPPED_HEAD", "WRAPPED_TAIL"],
        ),
        (
            "password = 'SPACED_HEAD SPACED_TAIL' echo safe",
            &["SPACED_HEAD", "SPACED_TAIL"],
        ),
        ("passwd PASSWD_LEAK", &["PASSWD_LEAK"]),
        ("token=TOKEN_LEAK", &["TOKEN_LEAK"]),
        ("api_key=API_LEAK", &["API_LEAK"]),
        ("secret=SECRET_LEAK", &["SECRET_LEAK"]),
        (
            "export SERVICE_KEY=ENV_KEY_LEAK echo safe",
            &["ENV_KEY_LEAK"],
        ),
        ("export SERVICE_TOKEN=ENV_TOKEN_LEAK", &["ENV_TOKEN_LEAK"]),
        (
            "export SERVICE_SECRET=ENV_SECRET_LEAK",
            &["ENV_SECRET_LEAK"],
        ),
        (
            "export MY_PASSWORD_VALUE=ENV_PASSWORD_LEAK",
            &["ENV_PASSWORD_LEAK"],
        ),
        (
            "curl https://SHORTUSER:SHORTPASS@example.test/path",
            &["SHORTUSER", "SHORTPASS"],
        ),
        (
            "curl 'https://example.test/path?X-Amz-Signature=AMZLEAK&sig=SIGLEAK&token=QUERYLEAK'",
            &["AMZLEAK", "SIGLEAK", "QUERYLEAK"],
        ),
        ("echo ghp_ShortFakeLeak9", &["ghp_ShortFakeLeak9"]),
        ("echo gho_ShortFakeLeak9", &["gho_ShortFakeLeak9"]),
        (
            "echo github_pat_ShortFakeLeak9",
            &["github_pat_ShortFakeLeak9"],
        ),
        ("echo sk-FakeLeak123", &["sk-FakeLeak123"]),
        ("echo sk-ant-FakeLeak123", &["sk-ant-FakeLeak123"]),
        ("echo xoxb-FakeLeak123", &["xoxb-FakeLeak123"]),
        ("echo xoxp-FakeLeak123", &["xoxp-FakeLeak123"]),
        ("echo AKIAFAKELEAK123", &["AKIAFAKELEAK123"]),
        ("echo ASIAFAKELEAK123", &["ASIAFAKELEAK123"]),
        ("echo AIzaFakeLeak123", &["AIzaFakeLeak123"]),
        (
            "echo eyJhbGciOiJub25lIn0.eyJpZCI6MTIzfQ.fake",
            &["eyJhbGciOiJub25lIn0", "eyJpZCI6MTIzfQ", ".fake"],
        ),
        (
            "echo abcdef0123456789abcdef0123456789",
            &["abcdef0123456789abcdef0123456789"],
        ),
        (
            "echo QWxwaGFCZXRhR2FtbWFEZWx0YUVwc2lsb24=",
            &["QWxwaGFCZXRhR2FtbWFEZWx0YUVwc2lsb24="],
        ),
        (
            "echo -----BEGIN PRIVATE KEY-----\nPEMLEAK\n-----END PRIVATE KEY-----",
            &["PEMLEAK", "BEGIN PRIVATE KEY"],
        ),
        (
            "TOKEN=$(security find-generic-password -s KEYCHAINLEAK -w)",
            &["KEYCHAINLEAK"],
        ),
        (
            "curl https://ENCUSER:ENCPASS@example.test/%67%68%70_LEAK",
            &["ENCUSER", "ENCPASS", "LEAK"],
        ),
        ("echo safe\necho SECOND_LINE_LEAK", &["SECOND_LINE_LEAK"]),
        ("cat <<EOF\nHEREDOC_LEAK\nEOF", &["HEREDOC_LEAK"]),
        (
            "echo token='UNTERMINATED_HEAD UNTERMINATED_TAIL",
            &["UNTERMINATED_HEAD", "UNTERMINATED_TAIL"],
        ),
        (
            "echo TOKEN=$'ANSI_HEAD ANSI_TAIL'",
            &["ANSI_HEAD", "ANSI_TAIL"],
        ),
        (
            "curl --header 'Cookie: COOKIE_HEAD COOKIE_TAIL'",
            &["COOKIE_HEAD", "COOKIE_TAIL"],
        ),
        (
            "curl -u admin:PairLeak9 example.test",
            &["admin", "PairLeak9"],
        ),
        (
            "curl --user admin:PairLeak9 example.test",
            &["admin", "PairLeak9"],
        ),
        ("sshpass -p CmdLeak9 ssh host", &["CmdLeak9"]),
        ("MYSQL_PWD=EnvLeak9 client", &["EnvLeak9"]),
        ("DB_PASS=EnvLeak9 client", &["EnvLeak9"]),
        ("redis-cli -a CmdLeak9", &["CmdLeak9"]),
        ("export GITLAB=glpat-SmallLeak9", &["glpat-SmallLeak9"]),
        ("htpasswd -b f admin CmdLeak9", &["CmdLeak9"]),
        (
            "echo CmdLeak9 | docker login --password-stdin",
            &["CmdLeak9"],
        ),
        ("the password is ProseLeak9", &["ProseLeak9"]),
        ("SERVICE_AUTH=EnvLeak9 client", &["EnvLeak9"]),
        ("SERVICE_CRED=EnvLeak9 client", &["EnvLeak9"]),
        ("client --api-key APILEAK9", &["APILEAK9"]),
        ("inspect glptt-SmallLeak9", &["glptt-SmallLeak9"]),
        ("inspect npm_SmallLeak9", &["npm_SmallLeak9"]),
        ("inspect pypi-SmallLeak9", &["pypi-SmallLeak9"]),
        ("inspect hf_SmallLeak9", &["hf_SmallLeak9"]),
        ("inspect xapp-SmallLeak9", &["xapp-SmallLeak9"]),
        ("inspect xoxa-SmallLeak9", &["xoxa-SmallLeak9"]),
        ("inspect xoxr-SmallLeak9", &["xoxr-SmallLeak9"]),
        ("inspect shpat_SmallLeak9", &["shpat_SmallLeak9"]),
        ("inspect SG.SmallLeak9.part", &["SG.SmallLeak9.part"]),
        ("echo $HOME ${PROJECT} ghp_RefLeak9", &["ghp_RefLeak9"]),
        ("DB_PASS=RefLeak9 echo $HOME", &["RefLeak9"]),
        ("echo $(client --password SubLeak9)", &["SubLeak9"]),
        ("echo `client --password TickLeak9`", &["TickLeak9"]),
        (
            "echo $HOME https://UsrLeak9:PwdLeak9@example.test",
            &["UsrLeak9", "PwdLeak9"],
        ),
        ("curl $HOST/path?sig=SignedLeak9", &["SignedLeak9"]),
        ("npm config set _authToken SpacedLeak9", &["SpacedLeak9"]),
        (
            "rclone config secret_access_key SpacedLeak9",
            &["SpacedLeak9"],
        ),
        ("the password was ProseLeak9", &["ProseLeak9"]),
        ("비밀번호는 KoreanLeak9", &["KoreanLeak9"]),
        ("비밀번호: KoreanLeak9", &["KoreanLeak9"]),
        ("echo $ghp_RefTokenLeak9 $HOME", &["RefTokenLeak9"]),
        ("echo ${ghp_RefTokenLeak9} $HOME", &["RefTokenLeak9"]),
        ("echo $sk-RefTokenLeak9 $HOME", &["RefTokenLeak9"]),
        ("export $KEY=RefAssignLeak9", &["RefAssignLeak9"]),
    ];
    assert_eq!(cases.len(), 84);
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut derive = deriver(&mut writer, Arc::new(AtomicBool::new(true)));
    let mut card_posts = 0;
    for (case_index, (command, secrets)) in cases.iter().enumerate() {
        for (tool_index, name) in ["Bash", "exec_command", "Task", "WebSearch"]
            .into_iter()
            .enumerate()
        {
            let input = serde_json::json!({
                "command":command, "cmd":command, "description":command, "query":command,
            });
            for (encoding, value) in [input.clone(), serde_json::json!(input.to_string())]
                .into_iter()
                .enumerate()
            {
                let id = format!("corpus-{case_index}-{tool_index}-{encoding}");
                let call_id = format!("call-{id}");
                let tool = captured(serde_json::json!({
                    "type":"assistant", "uuid":format!("uuid-{id}"), "apiBlockIndex":0,
                    "cwd":"/work/project", "message":{"id":id, "content":[{
                        "type":"tool_use", "id":call_id, "name":name, "input":value,
                    }]},
                }));
                deliver_all(&mut writer, &derive.derive(&tool)).await;
                let result = captured(serde_json::json!({
                    "type":"user", "uuid":format!("result-{id}"), "message":{"content":[{
                        "type":"tool_result", "tool_use_id":call_id, "is_error":true,
                        "content":format!("SECRET_RESULT_BODY: {command}"),
                    }]},
                }));
                deliver_all(&mut writer, &derive.derive(&result)).await;
                let closing = derive.derive(&body(&format!("body-{id}")));
                let expected = card(&closing);
                assert_eq!(expected.unit_key, card_key(&id));
                deliver_all(&mut writer, &closing).await;
                let posts = harness.port.posts.lock().unwrap();
                let posted = &posts[card_posts * 2];
                assert_eq!(
                    posted, &expected.payload,
                    "the checked bytes must reach POST"
                );
                assert_eq!(posts[card_posts * 2 + 1], "answer");
                assert_eq!(posts.len(), (card_posts + 1) * 2);
                assert!(posted.starts_with("-# ▸ ") && posted.ends_with('✗'));
                let visible = posted.replace(['\\', '\u{200b}'], "");
                assert!(
                    !posted.contains("SECRET_RESULT_BODY")
                        && !visible.contains("SECRET_RESULT_BODY")
                );
                for secret in *secrets {
                    assert!(
                        !posted.contains(secret) && !visible.contains(secret),
                        "case {case_index}, {name}, encoding {encoding}: {secret:?} survived POST {posted:?}",
                    );
                }
                card_posts += 1;
            }
        }
    }
    assert_eq!(card_posts, 672);
    assert_eq!(harness.port.posts().len(), 1344);
    assert_eq!(writer.store().ledger().next_serial(), 1344);
    assert_eq!(writer.store().ledger().violation(), None);
}
