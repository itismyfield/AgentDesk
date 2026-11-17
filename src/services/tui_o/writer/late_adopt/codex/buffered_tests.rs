use super::super::{PrefixClass, buffered};
use super::*;
use std::io::Write;

fn first() -> Vec<String> {
    vec![meta("parent"),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}).to_string(),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}).to_string()]
}

fn body() -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"body","content":[{"type":"output_text","text":"answer"}]}})
}

fn end(turn: Option<&str>) -> Value {
    json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}})
}

#[test]
fn buffered_classes_require_a_named_first_turn_and_native_output_identity() {
    for output in [
        body(),
        json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"AgentMessage","id":"body"}}}),
        json!({"type":"response_item","payload":{"type":"function_call","id":"tool","name":"read","arguments":"{}"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"tool","output":"excluded"}}),
        end(Some("t1")),
    ] {
        let mut lines = first();
        lines.push(output.to_string());
        let result = buffered(&rollout(&lines, "").source);
        assert!(
            matches!(result, Verdict::Eligible(ref p) if matches!(&p.class, PrefixClass::Buffered {turn,..} if turn == "t1")),
            "{result:?}"
        );
        for opener in [Value::Null, json!("")] {
            let mut invalid = lines.clone();
            invalid[1] = edited(&invalid[1], |v| v["payload"]["turn_id"] = opener);
            assert!(matches!(
                buffered(&rollout(&invalid, "").source),
                Verdict::Refuse(_)
            ));
        }
    }
}

#[test]
fn every_unsafe_raw_record_is_refused_initially_and_after_a_checked_buffered_prefix() {
    let mut missing_id = body();
    missing_id["payload"].as_object_mut().unwrap().remove("id");
    let mut other_turn = body();
    other_turn["payload"]["turn_id"] = "old".into();
    let bad = [
        serde_json::from_str(&meta("parent")).unwrap(),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"body","content":[{"type":"image"}]}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"another input"}}),
        missing_id,
        other_turn,
        json!({"type":"response_item","payload":{"type":"unknown_output","id":"unsupported"}}),
        json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"body"}}}),
        json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"old","item":{"type":"AgentMessage","id":"body"}}}),
        json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"AgentMessage"}}}),
        end(None),
        end(Some("old")),
        json!({"type":"event_msg","payload":{"type":"turn_aborted"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t2"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}}),
    ];
    for record in bad {
        let mut initial = first();
        initial.push(record.to_string());
        assert!(
            matches!(buffered(&rollout(&initial, "").source), Verdict::Refuse(_)),
            "{record}"
        );
        let mut lines = first();
        lines.push(body().to_string());
        let file = rollout(&lines, "");
        let Verdict::Eligible(checked) = buffered(&file.source) else {
            panic!("valid initial prefix")
        };
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&file.source.path)
                .unwrap(),
            "{record}"
        )
        .unwrap();
        checked.preserved_in(&file.source).unwrap();
        assert!(
            matches!(buffered(&file.source), Verdict::Refuse(_)),
            "{record}"
        );
    }
}

#[test]
fn buffered_growth_keeps_known_bytes_and_a_closed_turn_never_accepts_new_activity() {
    let mut lines = first();
    lines.push(body().to_string());
    let file = rollout(&lines, "");
    let Verdict::Eligible(checked) = buffered(&file.source) else {
        panic!("valid prefix")
    };
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file.source.path)
            .unwrap(),
        "{}",
        end(Some("t1"))
    )
    .unwrap();
    checked.preserved_in(&file.source).unwrap();
    assert!(
        matches!(buffered(&file.source), Verdict::Eligible(ref now) if now != &checked && matches!(now.class, PrefixClass::Buffered { closed: true, .. }))
    );
    for activity in [
        body(),
        end(Some("t1")),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"hi"}}),
    ] {
        let mut closed = lines.clone();
        closed.push(end(Some("t1")).to_string());
        closed.push(activity.to_string());
        assert!(matches!(
            buffered(&rollout(&closed, "").source),
            Verdict::Refuse(_)
        ));
    }
    let changed = std::fs::read_to_string(&file.source.path)
        .unwrap()
        .replace("answer", "mutate");
    std::fs::write(&file.source.path, changed).unwrap();
    assert!(matches!(
        checked.preserved_in(&file.source),
        Err(Verdict::Refuse(_))
    ));
}

#[test]
fn buffered_header_identity_and_parent_checks_hold_initially_and_at_final_read() {
    for header in [
        meta("other"),
        edited(&meta("parent"), |v| {
            v["payload"]["parent_thread_id"] = "another-session".into();
        }),
    ] {
        let mut invalid = first();
        invalid[0] = header.clone();
        invalid.push(body().to_string());
        assert!(matches!(
            buffered(&rollout(&invalid, "").source),
            Verdict::Refuse(_)
        ));
        let mut good = first();
        good.push(body().to_string());
        let file = rollout(&good, "");
        let Verdict::Eligible(checked) = buffered(&file.source) else {
            panic!("valid header")
        };
        std::fs::write(
            &file.source.path,
            invalid.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        assert!(matches!(
            checked.preserved_in(&file.source),
            Err(Verdict::Refuse(_))
        ));
        assert!(matches!(buffered(&file.source), Verdict::Refuse(_)));
    }
}
