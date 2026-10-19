//! Tool output stays internal even when a command failed or an older writer owed a retry.

use super::*;
use crate::services::tui_o::store::ledger::LocalRetryMark;

fn record(value: serde_json::Value) -> CapturedRecord {
    let line = serde_json::to_vec(&value).unwrap();
    CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    }
}

fn result(id: &str, content: serde_json::Value, error: bool) -> CapturedRecord {
    record(serde_json::json!({"type":"user", "uuid":format!("u-{id}"),
        "message":{"content":[{"type":"tool_result", "tool_use_id":id,
        "is_error":error, "content":content}, {"type":"text", "text":"user followup"}]}}))
}

fn key(id: &str) -> UnitKey {
    UnitKey {
        kind: UnitKind::ToolResult,
        ..unit(id)
    }
}

#[tokio::test]
async fn tool_results_are_excluded_durably_for_strings_arrays_success_and_replay() {
    let cases = [
        (
            "trace",
            serde_json::json!("Exit code 1\nTraceback: private path"),
            true,
        ),
        (
            "array",
            serde_json::json!([{ "type":"text", "text":"private API response" },
            {"type":"image", "source":{"data":"private"}},
            {"type":"text", "text":"JSONDecodeError"}]),
            true,
        ),
        (
            "object",
            serde_json::json!({"error":{"secret":"private"}}),
            true,
        ),
        ("success", serde_json::json!("private success data"), false),
    ];
    let harness = Harness::new();
    harness.gate.acquired();
    for _ in 0..2 {
        let mut writer = harness.writer();
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
        for (id, content, error) in &cases {
            let row = result(id, content.clone(), *error);
            let items = deriver.derive(&row);
            assert!(
                matches!(items.as_slice(), [Derived::Excluded { unit_key, reason }]
                if unit_key == &key(id) && reason == "tool_result_internal"),
                "{id}: {items:?}"
            );
            assert!(deriver.knows(id, UnitKind::ToolResult));
            assert!(deriver.sealed(id, UnitKind::ToolResult));
            assert!(!deriver.has_unsealed());
            for item in items {
                assert_eq!(writer.deliver(&item).await, Step::Done);
            }
            assert!(deriver.derive(&row).is_empty(), "fork copy stays sealed");
            assert_eq!(
                writer.store().ledger().excluded(&key(id)),
                Some("tool_result_internal")
            );
        }
        assert_eq!(writer.store().ledger().next_serial(), 0);
        assert_eq!(writer.store().ledger().violation(), None);
    }
    assert!(harness.port.posts().is_empty());
}

#[test]
fn tool_result_exclusion_keeps_changed_payload_conflicts_visible() {
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    deriver.derive(&result("failed", serde_json::json!("error one"), true));
    let changed = deriver.derive(&result("failed", serde_json::json!("error two"), true));
    assert!(matches!(changed.as_slice(), [Derived::Blocked { .. }]));
}

#[tokio::test]
async fn tool_result_suppression_preserves_assistant_text_and_provider_api_errors() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let rows = [
        result("failed", serde_json::json!("raw traceback"), true),
        record(
            serde_json::json!({"type":"assistant", "uuid":"body", "apiBlockIndex":0,
            "message":{"id":"body", "content":[{"type":"text", "text":"Exit code 1: I fixed the command."},
            {"type":"tool_use", "id":"tool", "name":"Bash", "input":{"command":"private"}},
            {"type":"text", "text":"The check now passes."}]}}),
        ),
        record(
            serde_json::json!({"type":"assistant", "uuid":"api-error", "isApiErrorMessage":true,
            "message":{"model":"<synthetic>", "content":[{"type":"text", "text":"API Error: 529 overloaded"}]}}),
        ),
    ];
    for row in rows {
        for item in deriver.derive(&row) {
            assert_eq!(writer.deliver(&item).await, Step::Done);
        }
    }
    assert_eq!(
        harness.port.posts(),
        [
            "Exit code 1: I fixed the command.",
            "The check now passes.",
            "API Error: 529 overloaded"
        ]
    );
    assert_eq!(harness.alarms.taken(), []);
}

#[test]
fn codex_tool_results_are_excluded_for_each_supported_output_shape() {
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
    for kind in [
        "function_call_output",
        "custom_tool_call_output",
        "tool_search_output",
    ] {
        let row = record(serde_json::json!({"type":"response_item", "payload":{
            "type":kind, "call_id":kind, "output":"Exit code 1: private data"}}));
        let items = deriver.derive(&row);
        assert!(
            matches!(items.as_slice(), [Derived::Excluded { unit_key, reason }]
            if unit_key.kind == UnitKind::ToolResult && reason == "tool_result_internal")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_tool_result_retries_and_refusals_are_excluded_across_restart() {
    for case in [
        "budget", "prepared", "landed", "posted", "rejected", "approved",
    ] {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.say(BOT + 1, "history is readable");
        let landed =
            (case == "landed").then(|| harness.port.say(BOT, "legacy private traceback").id);
        let mut store = harness.channel();
        store
            .append_ledger(LedgerEntry::Prepared {
                serial: 0,
                unit_key: key("legacy"),
                piece_index: 0,
                payload: "legacy private traceback".into(),
                anchor_id: 100,
                epoch: 1,
                local_retry: Some(LocalRetryMark::Budget),
            })
            .unwrap();
        let outcome = match case {
            "budget" => Some(LedgerEntry::NotFound { serial: 0 }),
            "posted" => Some(LedgerEntry::Posted {
                serial: 0,
                msg_id: 999,
            }),
            "rejected" | "approved" => Some(LedgerEntry::Rejected {
                serial: 0,
                status: 403,
            }),
            _ => None,
        };
        if let Some(outcome) = outcome {
            store.append_ledger(outcome).unwrap();
        }
        drop(store);
        if case == "approved" {
            harness
                .store
                .record_operator_resume(CHANNEL, 0, "operator", "restored")
                .unwrap();
        }
        let old = Derived::Piece(PieceWork {
            unit_key: key("legacy"),
            index: 0,
            payload: "legacy private traceback".into(),
        });
        for _ in 0..2 {
            let mut writer = harness.writer();
            assert!(!writer.is_stopped(), "{case}");
            assert_eq!(
                writer.store().ledger().excluded(&key("legacy")),
                Some("tool_result_internal"),
                "{case}: excluded at startup"
            );
            assert!(
                writer.store().ledger().resume_pieces().next().is_none(),
                "{case}: no operator resend"
            );
            assert_eq!(
                writer
                    .retry_local(Utc::now() + chrono::Duration::days(1))
                    .await,
                Step::Done,
                "{case}"
            );
            assert_eq!(writer.deliver(&old).await, Step::Done, "{case}");
            assert!(!writer.has_local_retry(), "{case}");
            assert_eq!(
                writer.store().ledger().excluded(&key("legacy")),
                Some("tool_result_internal"),
                "{case}"
            );
            assert_eq!(writer.store().ledger().violation(), None, "{case}");
            assert!(writer.store().ledger().unresolved().is_none(), "{case}");
            assert_eq!(writer.store().ledger().next_serial(), 1, "{case}");
        }
        let mut writer = harness.writer();
        assert_eq!(
            writer
                .deliver(&piece("following", "assistant following"))
                .await,
            Step::Done,
            "{case}"
        );
        assert_eq!(harness.port.posts(), ["assistant following"], "{case}");
        let original = writer.store().ledger().piece(0).unwrap();
        match case {
            "landed" => assert_eq!(
                original.outcome,
                Some(PieceOutcome::Posted(landed.unwrap()))
            ),
            "posted" => assert_eq!(original.outcome, Some(PieceOutcome::Posted(999))),
            "rejected" | "approved" => {
                assert_eq!(original.outcome, Some(PieceOutcome::Rejected(403)))
            }
            _ => assert_eq!(original.outcome, Some(PieceOutcome::NotFound)),
        }
        if case == "approved" {
            assert_eq!(
                writer.store().ledger().approval(0).unwrap().consumed_serial,
                None
            );
        }
    }
}
