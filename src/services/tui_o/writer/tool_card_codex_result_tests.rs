use super::super::pieces::UnitDeriver;
use super::*;
use crate::services::tui_o::shadow::{CapturedRecord, UnitKind};
use serde_json::json;

fn record(value: Value) -> CapturedRecord {
    CapturedRecord {
        start: 0,
        end: 1,
        line: serde_json::to_vec(&value).unwrap(),
    }
}

fn deriver() -> UnitDeriver {
    UnitDeriver::new(7, ShadowProvider::Codex).with_tool_cards(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::new(),
        switch: Arc::new(|| true),
    })
}

fn call(id: &str) -> CapturedRecord {
    record(json!({"type":"response_item","payload":{
        "type":"function_call","id":format!("native-{id}"),"call_id":id,
        "name":"functions.exec_command","arguments":"{\"cmd\":\"cargo check\"}"
    }}))
}

fn output(id: &str, signal: Option<(&str, Value)>) -> CapturedRecord {
    let mut value = json!({"type":"response_item","payload":{
        "type":"function_call_output","call_id":id,
        "output":"Process exited with code 1\nPRIVATE_RESULT_BODY"
    }});
    if let Some((field, signal)) = signal {
        value["payload"][field] = signal;
    }
    record(value)
}

fn end() -> CapturedRecord {
    record(json!({"type":"event_msg","payload":{
        "type":"task_complete","turn_id":"turn-1"
    }}))
}

fn cards(work: &[Derived]) -> Vec<&PieceWork> {
    work.iter()
        .filter_map(|work| match work {
            Derived::Piece(piece) if piece.unit_key.native_key.ends_with("#card") => Some(piece),
            _ => None,
        })
        .collect()
}

#[test]
fn rollout_shaped_codex_outputs_never_claim_success_or_leak_result_text() {
    let mut derive = deriver();
    let mut work = derive.derive(&call("function"));
    work.extend(derive.derive(&output("function", None)));
    work.extend(derive.derive(&record(json!({"type":"response_item","payload":{
        "type":"custom_tool_call","id":"native-custom","call_id":"custom",
        "name":"apply_patch","input":"*** Begin Patch\n*** Add File: src/a.rs\n+x\n*** End Patch"
    }}))));
    work.extend(
        derive.derive(&record(json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"custom","output":"PRIVATE_RESULT_BODY"
        }}))),
    );
    work.extend(derive.derive(&end()));
    let posted = cards(&work);
    assert_eq!(posted.len(), 1, "the fixture must produce an actual card");
    assert_eq!(posted[0].payload.matches('✓').count(), 0);
    assert_eq!(posted[0].payload.matches('·').count(), 2);
    assert!(!posted[0].payload.contains('✗'));
    assert!(!posted[0].payload.contains("PRIVATE_RESULT_BODY"));
    assert!(!posted[0].payload.contains("Process exited"));
}

#[test]
fn codex_error_aliases_require_an_explicit_true_failure_signal() {
    for field in ["is_error", "isError"] {
        for (signal, expected) in [
            (Some(json!(true)), '✗'),
            (Some(json!(false)), '·'),
            (Some(json!("true")), '·'),
            (None, '·'),
        ] {
            let mut derive = deriver();
            derive.derive(&call("signal"));
            derive.derive(&output("signal", signal.map(|value| (field, value))));
            let work = derive.derive(&end());
            let posted = cards(&work);
            assert_eq!(posted.len(), 1, "{field} must produce an actual card");
            assert_eq!(
                posted[0].payload,
                format!("-# ▸ Bash  cargo check  {expected}"),
                "only a structural true {field} can report failure"
            );
        }
    }
}

#[test]
fn eight_unknown_codex_results_flush_and_fold_before_a_native_boundary() {
    let mut derive = deriver();
    for index in 0..8 {
        assert!(cards(&derive.derive(&call(&format!("call-{index}")))).is_empty());
    }
    for index in 0..7 {
        assert!(cards(&derive.derive(&output(&format!("call-{index}"), None))).is_empty());
    }
    let work = derive.derive(&output("call-7", None));
    let posted = cards(&work);
    assert_eq!(
        posted.len(),
        1,
        "eight resolved rows must flush without an idle record"
    );
    assert_eq!(posted[0].unit_key.native_key, "native-call-7#card");
    assert_eq!(posted[0].payload, "-# ▸ Bash ×8  cargo check  ·");
    assert_eq!(posted[0].payload.matches('✓').count(), 0);
    assert!(cards(&derive.derive(&end())).is_empty());
}

#[test]
fn folding_never_combines_success_unknown_pending_or_failed_statuses() {
    let mut run = ToolCardRun::new(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::new(),
        switch: Arc::new(|| true),
    });
    let statuses = ['✓', '✓', '·', '·', '·', '✓', '·', '?', '?', '✗', '✗'];
    for index in 0..statuses.len() {
        run.tool(
            UnitKey {
                channel_id: 7,
                provider: ShadowProvider::Codex,
                native_key: format!("native-{index}"),
                kind: UnitKind::Tool,
            },
            ToolMeta {
                id: index.to_string(),
                label: if index == 4 { "Read" } else { "Bash" }.into(),
                summary: None,
            },
        );
    }
    for (index, status) in statuses.into_iter().enumerate() {
        match status {
            '✓' => run.result(&index.to_string(), false),
            '✗' => run.result(&index.to_string(), true),
            '·' => run.result_unknown(&index.to_string()),
            _ => Vec::new(),
        };
    }
    let work = run.close();
    let posted = cards(&work);
    assert_eq!(posted.len(), 1);
    assert_eq!(
        posted[0].payload,
        "-# ▸ Bash ×2  ✓\n-# ▸ Bash ×2  ·\n-# ▸ Read  ·\n-# ▸ Bash  ✓\n-# ▸ Bash  ·\n-# ▸ Bash  ?\n-# ▸ Bash  ?\n-# ▸ Bash  ✗\n-# ▸ Bash  ✗"
    );
}

#[test]
fn code_mode_exec_status_line_resolves_the_card_result() {
    // A completed script can still hold a failed command, so only a script failure is decisive.
    let failed_command = "{\"exit_code\":1,\"output\":\"error: could not compile\"}";
    for (header, is_error, expected) in [
        (
            "Script completed\nWall time 0.1 seconds\nOutput:",
            None,
            '·',
        ),
        (
            "Script completed\nWall time 0.1 seconds\nOutput:",
            Some(true),
            '✗',
        ),
        ("Script failed\nWall time 0.1 seconds\nOutput:", None, '✗'),
        ("Script completed later", None, '·'),
    ] {
        let mut derive = deriver();
        derive.derive(&record(json!({"type":"response_item","payload":{
            "type":"custom_tool_call","id":"native-exec","call_id":"exec","name":"exec",
            "input":"const r = await tools.exec_command({cmd: \"cargo check\"});"
        }})));
        let mut output = json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"exec",
            "output":[{"type":"input_text","text":header},{"type":"input_text","text":failed_command},
                {"type":"input_text","text":"PRIVATE_RESULT_BODY"}]
        }});
        if let Some(is_error) = is_error {
            output["payload"]["is_error"] = json!(is_error);
        }
        derive.derive(&record(output));
        let work = derive.derive(&end());
        let posted = cards(&work);
        assert_eq!(posted.len(), 1, "{header}");
        assert!(
            posted[0].payload.ends_with(&format!("  {expected}")),
            "{header}: {}",
            posted[0].payload
        );
        assert!(!posted[0].payload.contains("PRIVATE_RESULT_BODY"));
    }
}
