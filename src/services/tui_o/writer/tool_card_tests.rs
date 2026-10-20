use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde_json::{Value, json};

use super::pieces::{Derived, TOOL_CALL_CARD, TOOL_CALL_PANEL, ToolCardInputs, UnitDeriver};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKind};

fn record(value: Value) -> CapturedRecord {
    CapturedRecord {
        start: 0,
        end: 1,
        line: serde_json::to_vec(&value).unwrap(),
    }
}

fn row(id: &str, blocks: Vec<Value>) -> CapturedRecord {
    record(
        json!({"type":"assistant", "cwd":"/work/project", "apiBlockIndex":0,
        "message":{"id":id, "content":blocks}}),
    )
}

fn tool(id: &str, name: &str, input: Value) -> CapturedRecord {
    row(
        id,
        vec![json!({"type":"tool_use", "id":id, "name":name, "input":input})],
    )
}

fn result(id: &str, error: bool) -> CapturedRecord {
    record(
        json!({"type":"user", "message":{"content":[{"type":"tool_result",
        "tool_use_id":id,"is_error":error,"content":"SECRET_RESULT_BODY"}]}}),
    )
}

fn body(id: &str, text: &str) -> CapturedRecord {
    row(id, vec![json!({"type":"text", "text":text})])
}

fn enabled(provider: ShadowProvider) -> (UnitDeriver, Arc<AtomicBool>) {
    let on = Arc::new(AtomicBool::new(true));
    let read = Arc::clone(&on);
    let inputs = ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::new(),
        switch: Arc::new(move || read.load(Ordering::SeqCst)),
    };
    (UnitDeriver::new(7, provider).with_tool_cards(inputs), on)
}

fn pieces(work: &[Derived]) -> Vec<(String, String)> {
    work.iter()
        .filter_map(|work| match work {
            Derived::Piece(piece) => {
                Some((piece.unit_key.native_key.clone(), piece.payload.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn body_tool_run_body_order_and_adjacent_success_folding() {
    let (mut derive, _) = enabled(ShadowProvider::Claude);
    let mut work = derive.derive(&body("before", "before"));
    for (id, name, error) in [
        ("t1", "Bash", false),
        ("t2", "Bash", false),
        ("t3", "Read", false),
        ("t4", "Bash", true),
    ] {
        work.extend(derive.derive(&tool(
            id,
            name,
            json!({"command":"cargo test", "file_path":"/work/project/src/main.rs"}),
        )));
        work.extend(derive.derive(&result(id, error)));
    }
    work.extend(derive.derive(&body("after", "after")));
    let posted = pieces(&work);
    assert_eq!(posted.len(), 3);
    assert_eq!(posted[0].1, "before");
    assert_eq!(posted[1].0, "t4:0#card");
    assert_eq!(
        posted[1].1,
        "-# ▸ Bash ×2  cargo test  ✓\n-# ▸ Read  src/main.rs  ✓\n-# ▸ Bash  cargo test  ✗"
    );
    assert_eq!(posted[2].1, "after");
    assert_eq!(
        work.iter()
            .filter(|item| matches!(item, Derived::Excluded{reason,..} if reason==TOOL_CALL_CARD))
            .count(),
        4
    );
    assert!(!format!("{work:?}").contains("SECRET_RESULT_BODY"));
}

#[test]
fn toggles_only_apply_after_record_boundaries_and_panel_keys_stay_known() {
    let (mut derive, on) = enabled(ShadowProvider::Claude);
    derive.derive(&tool("t1", "Bash", json!({"cmd":"one"})));
    on.store(false, Ordering::SeqCst);
    let second = derive.derive(&tool("t2", "Read", json!({"path":"two"})));
    assert!(matches!(&second[0],Derived::Excluded{reason,..} if reason==TOOL_CALL_CARD));
    on.store(true, Ordering::SeqCst);
    derive.derive(&tool("t3", "Write", json!({"path":"three"})));
    assert_eq!(pieces(&derive.derive(&body("b", "body")))[0].0, "t3:0#card");
    on.store(false, Ordering::SeqCst);
    let panel = derive.derive(&tool("t4", "Bash", json!({"cmd":"four"})));
    assert!(matches!(&panel[0],Derived::Excluded{reason,..} if reason==TOOL_CALL_PANEL));
    assert!(
        derive
            .derive(&tool("t4", "Bash", json!({"cmd":"four"})))
            .is_empty()
    );
    on.store(true, Ordering::SeqCst);
    derive.derive(&tool("t5", "Bash", json!({"cmd":"five"})));
    assert_eq!(pieces(&derive.derive(&body("c", "body")))[0].0, "t5:0#card");
}

#[test]
fn repeat_excluded_body_and_result_do_not_close_or_duplicate_a_run() {
    let (mut derive, _) = enabled(ShadowProvider::Claude);
    let call = tool("t", "Bash", json!({"cmd":"safe"}));
    derive.derive(&call);
    assert!(derive.derive(&call).is_empty());
    let mut excluded = body("e", "No response requested.");
    let mut value: Value = serde_json::from_slice(&excluded.line).unwrap();
    value["message"]["model"] = json!("<synthetic>");
    value.as_object_mut().unwrap().remove("apiBlockIndex");
    value["uuid"] = json!("excluded");
    excluded.line = serde_json::to_vec(&value).unwrap();
    assert!(pieces(&derive.derive(&excluded)).is_empty());
    derive.derive(&result("t", false));
    assert!(derive.derive(&result("t", false)).is_empty());
    let closing = record(json!({"type":"result","subtype":"success"}));
    let work = derive.derive(&closing);
    assert_eq!(pieces(&work).len(), 1);
    assert!(pieces(&work)[0].1.ends_with('✓'));
}

#[test]
fn bounded_runs_flush_at_fourteen_and_at_eight_resolved_rows() {
    let (mut derive, _) = enabled(ShadowProvider::Claude);
    let mut all = Vec::new();
    for n in 0..15 {
        all.extend(derive.derive(&tool(&format!("t{n}"), "Read", json!({"path":"x"}))));
    }
    assert_eq!(pieces(&all).len(), 1);
    assert_eq!(pieces(&all)[0].1.lines().count(), 14);
    all.extend(derive.derive(&body("end", "end")));
    assert_eq!(pieces(&all).len(), 3);
    let (mut derive, _) = enabled(ShadowProvider::Claude);
    for n in 0..8 {
        derive.derive(&tool(&format!("t{n}"), "Read", json!({"path":"x"})));
    }
    for n in 0..7 {
        assert!(pieces(&derive.derive(&result(&format!("t{n}"), false))).is_empty());
    }
    assert_eq!(pieces(&derive.derive(&result("t7", false))).len(), 1);
}

#[test]
fn utf16_budget_escaped_summaries_unknown_tools_and_body_bytes() {
    let (mut derive, _) = enabled(ShadowProvider::Claude);
    let mut plain = UnitDeriver::new(7, ShadowProvider::Claude);
    let mut on = Vec::new();
    let mut off = Vec::new();
    let mut rows = Vec::new();
    for n in 0..70 {
        rows.push(tool(
            &format!("t{n}"),
            if n % 2 == 0 {
                "Bash"
            } else {
                "unknown @everyone"
            },
            json!({"cmd":"😀_*[]()".repeat(80)}),
        ));
    }
    rows.push(body(
        "body",
        &format!("한글\n```rust\n{}\n```", "😀".repeat(2000)),
    ));
    for row in rows {
        on.extend(derive.derive(&row));
        off.extend(plain.derive(&row));
    }
    let bodies = |work: Vec<Derived>| {
        work.into_iter()
            .filter(|item| matches!(item,Derived::Piece(p) if p.unit_key.kind==UnitKind::Body))
            .collect::<Vec<_>>()
    };
    assert_eq!(bodies(on.clone()), bodies(off));
    assert_eq!(
        on.iter()
            .filter(|d| matches!(d,Derived::Excluded{reason,..} if reason==TOOL_CALL_CARD))
            .count(),
        70
    );
    let cards = pieces(&on)
        .into_iter()
        .filter(|(key, _)| key.ends_with("#card"))
        .collect::<Vec<_>>();
    assert!(!cards.is_empty(), "budget validation requires actual cards");
    for (key, payload) in cards {
        assert!(!key.is_empty() && !payload.is_empty());
        assert!(payload.encode_utf16().count() <= 1800);
        assert!(!payload.chars().any(|c| c.is_control() && c != '\n'));
        assert!(!payload.contains("@everyone") && !payload.contains("://"));
    }
}

#[test]
fn codex_native_call_id_results_and_error_alias_are_supported() {
    let (mut derive, _) = enabled(ShadowProvider::Codex);
    for (kind, id, name, input) in [
        (
            "function_call",
            "f",
            "functions.exec_command",
            json!({"cmd":"cargo test"}),
        ),
        (
            "custom_tool_call",
            "c",
            "apply_patch",
            json!("*** Begin Patch\n*** Add File: src/a.rs\n+x\n*** End Patch"),
        ),
        (
            "tool_search_call",
            "s",
            "unknown",
            json!({"query":"secret"}),
        ),
    ] {
        let field = match kind {
            "custom_tool_call" => "input",
            "tool_search_call" => "action",
            _ => "arguments",
        };
        let mut value = json!({"type":"response_item","payload":{"type":kind,"id":id,"call_id":format!("call-{id}"),"name":name}});
        value["payload"][field] = match &input {
            Value::String(text) => text.clone().into(),
            _ => input.to_string().into(),
        };
        let call = record(value);
        let first = derive.derive(&call);
        assert!(matches!(&first[0],Derived::Excluded{reason,..} if reason==TOOL_CALL_CARD));
        let output = record(
            json!({"type":"response_item","payload":{"type":if kind=="function_call" {"function_call_output"} else if kind=="custom_tool_call" {"custom_tool_call_output"} else {"tool_search_output"},"call_id":format!("call-{id}"),"isError":id=="c","output":"SECRET_RESULT_BODY"}}),
        );
        derive.derive(&output);
    }
    let closed = derive.derive(&record(
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t"}}),
    ));
    let posted = pieces(&closed);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0, "s#card");
    assert!(posted[0].1.contains("Bash  cargo test  ·"));
    assert!(
        posted[0].1.contains("Edit  src/a.rs (+1 −0)  ✗"),
        "{}",
        posted[0].1
    );
    assert!(!posted[0].1.contains("SECRET_RESULT_BODY"));
}

#[test]
fn secret_corpus_672_card_payloads_hide_each_sentinel() {
    let tool_record = |name: &str, input: Value| tool("secret", name, input);
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
    for (command, secrets) in cases {
        for tool in ["Bash", "exec_command", "Task", "WebSearch"] {
            let input =
                json!({"command":command,"cmd":command,"description":command,"query":command});
            for value in [input.clone(), json!(input.to_string())] {
                let (mut derive, _) = enabled(ShadowProvider::Claude);
                derive.derive(&tool_record(tool, value));
                derive.derive(&result("secret", true));
                let output = pieces(&derive.derive(&body("end", "body")))[0].1.clone();
                assert!(!output.contains("SECRET_RESULT_BODY"));
                let visible = output.replace(['\\', '\u{200b}'], "");
                for secret in *secrets {
                    assert!(
                        !output.contains(secret) && !visible.contains(secret),
                        "{tool}: {secret:?} survived in {output:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn reserved_native_keys_cannot_alias_a_synthetic_card() {
    for provider in [ShadowProvider::Claude, ShadowProvider::Codex] {
        let (mut derive, _) = enabled(provider);
        let record = match provider {
            ShadowProvider::Claude => tool("t#card", "Bash", json!({"cmd":"safe"})),
            ShadowProvider::Codex => record(json!({"type":"response_item","payload":{
                "type":"function_call","id":"t#card","call_id":"call","name":"exec_command","arguments":"{}"}})),
        };
        assert!(matches!(
            &derive.derive(&record)[..],
            [Derived::Blocked { .. }]
        ));
    }
}

#[test]
fn on_off_on_without_tools_between_toggles_replays_the_same_single_card() {
    let (mut live, on) = enabled(ShadowProvider::Claude);
    let calls = [
        tool("one", "Read", json!({"path":"one"})),
        tool("two", "Write", json!({"path":"two"})),
        tool("three", "Bash", json!({"cmd":"three"})),
    ];
    let mut work = live.derive(&calls[0]);
    work.extend(live.derive(&calls[1]));
    on.store(false, Ordering::SeqCst);
    on.store(true, Ordering::SeqCst);
    work.extend(live.derive(&calls[2]));
    work.extend(live.derive(&body("end", "body")));
    let recorded_cards = work
        .iter()
        .filter_map(|item| match item {
            Derived::Excluded { unit_key, reason } if reason == TOOL_CALL_CARD => {
                Some(unit_key.clone())
            }
            _ => None,
        })
        .collect();
    let mut replay = UnitDeriver::new(7, ShadowProvider::Claude).with_tool_cards(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards,
        switch: Arc::new(|| false),
    });
    let mut replayed = Vec::new();
    for call in &calls {
        replayed.extend(replay.derive(call));
    }
    replayed.extend(replay.derive(&body("end", "body")));
    assert_eq!(work, replayed);
    assert_eq!(pieces(&work).len(), 2);
    assert_eq!(pieces(&work)[0].0, "three:0#card");
}

#[test]
fn an_unrecorded_tool_joins_a_recorded_open_run_despite_a_downtime_toggle() {
    let key = crate::services::tui_o::shadow::UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        kind: UnitKind::Tool,
        native_key: "one:0".into(),
    };
    let mut replay = UnitDeriver::new(7, ShadowProvider::Claude).with_tool_cards(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::from([key]),
        switch: Arc::new(|| false),
    });
    replay.derive(&tool("one", "Read", json!({"path":"one"})));
    let work = replay.derive(&tool("two", "Write", json!({"path":"two"})));
    assert!(matches!(&work[0],Derived::Excluded{reason,..} if reason==TOOL_CALL_CARD));
    assert_eq!(
        pieces(&replay.derive(&body("end", "body")))[0].0,
        "two:0#card"
    );
    let after = replay.derive(&tool("after", "Read", json!({"path":"after"})));
    assert!(matches!(&after[0],Derived::Excluded{reason,..} if reason==TOOL_CALL_PANEL));
}

fn cap_toggle(name: &str, input: Value, count: usize) {
    let (mut derive, on) = enabled(ShadowProvider::Claude);
    for n in 0..count {
        assert!(pieces(&derive.derive(&tool(&format!("t{n}"), name, input.clone()))).is_empty());
    }
    on.store(false, Ordering::SeqCst);
    let overflow = derive.derive(&tool("off", name, input.clone()));
    let posted = pieces(&overflow);
    assert_eq!(posted.len(), 1, "{name}: {overflow:?}");
    assert_eq!(posted[0].0, format!("t{}:0#card", count - 1));
    assert_eq!(posted[0].1.lines().count(), count);
    assert!(
        matches!(overflow.last(),Some(Derived::Excluded{unit_key,reason}) if reason==TOOL_CALL_PANEL && unit_key.native_key=="off:0")
    );
    on.store(true, Ordering::SeqCst);
    assert!(pieces(&derive.derive(&tool("new", name, input))).is_empty());
    let closed = pieces(&derive.derive(&body("end", "body")));
    assert_eq!(closed[0].0, "new:0#card");
    assert_eq!(closed[0].1.lines().count(), 1);
}

#[test]
fn fourteen_row_cap_applies_off_before_starting_a_new_run() {
    cap_toggle("Read", json!({"path":"x"}), 14);
}

#[test]
fn utf16_cap_applies_off_before_starting_a_new_run() {
    cap_toggle("Bash", json!({"cmd":"😀".repeat(80)}), 10);
}

#[test]
fn recorded_cards_still_start_a_new_run_at_a_hard_cap_while_currently_off() {
    let records = (0..15)
        .map(|n| tool(&format!("t{n}"), "Read", json!({"path":"x"})))
        .collect::<Vec<_>>();
    let keys = (0..15)
        .map(|n| crate::services::tui_o::shadow::UnitKey {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            kind: UnitKind::Tool,
            native_key: format!("t{n}:0"),
        })
        .collect();
    let mut replay = UnitDeriver::new(7, ShadowProvider::Claude).with_tool_cards(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: keys,
        switch: Arc::new(|| false),
    });
    let mut work = Vec::new();
    for record in records {
        work.extend(replay.derive(&record));
    }
    work.extend(replay.derive(&body("end", "body")));
    let posted = pieces(&work);
    assert_eq!(posted.len(), 3);
    assert_eq!(posted[0].0, "t13:0#card");
    assert_eq!(posted[1].0, "t14:0#card");
    assert_eq!(
        work.iter()
            .filter(|d| matches!(d,Derived::Excluded{reason,..} if reason==TOOL_CALL_CARD))
            .count(),
        15
    );
}

#[test]
fn adjacent_failed_tools_never_fold_with_failure_or_success() {
    for errors in [[true, true], [false, true], [true, false]] {
        let (mut derive, _) = enabled(ShadowProvider::Claude);
        for (n, failed) in errors.into_iter().enumerate() {
            let id = format!("adjacent-{n}");
            derive.derive(&tool(&id, "Bash", json!({"cmd":format!("command-{n}")})));
            derive.derive(&result(&id, failed));
        }
        let output = pieces(&derive.derive(&body("adjacent-end", "answer")))[0]
            .1
            .clone();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "{errors:?}: {output}");
        assert!(
            !output.contains('×'),
            "failed rows must stay separate: {output}"
        );
        for (line, failed) in lines.iter().zip(errors) {
            assert!(line.ends_with(if failed { '✗' } else { '✓' }), "{output}");
        }
    }
}
