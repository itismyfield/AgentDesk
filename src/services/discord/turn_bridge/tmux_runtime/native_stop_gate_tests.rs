//! A stop judged against a fresh writer's first-key gate and its not yet pinned input.
use super::super::super::stop_host::tests::unpinned_generating_turn;
use super::native_contract::running;
use super::*;
use crate::services::claude_tui::host_input::InputRun;
use crate::services::claude_tui::input::UserReceipt;
use std::io::Write;

fn append(path: &std::path::Path, line: serde_json::Value) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn escapes(fx: &Fixture) -> usize {
    let calls = fx.take_calls();
    let escape = |call: &&String| call.starts_with("send-keys") && call.ends_with("Escape");
    calls.iter().filter(escape).count()
}

fn tool_use() -> serde_json::Value {
    serde_json::json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"tool-g","name":"Bash","input":{"command":"sleep 60"}}]}})
}

/// The writer's own user row for the receipt taken before it, as Claude records the prompt.
fn own_row(session: &str) -> serde_json::Value {
    serde_json::json!({"type":"user","uuid":format!("{session}-own"),"sessionId":"fixture-sid",
        "message":{"role":"user","content":"go"}})
}

/// A stop before a fresh writer's first key, warm or cold, main-cancels and takes the gate; after
/// the writer keyed, no pin means neither an Escape nor main's cancel, even on an idle transcript.
#[test]
fn a_stop_takes_a_fresh_writers_gate_only_before_its_first_key() {
    let fx = Fixture::new();
    run(async {
        let cases = [
            ("stop-first", true),
            ("stop-first-cold", true),
            ("writer-first", false),
        ];
        for (n, (case, stop_first)) in cases.into_iter().enumerate() {
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_754_100 + n as u64);
            let session = format!("native-gate-{case}");
            let (token, path) = unpinned_generating_turn(&fx, &session);
            let sink = token.native_stop.receipt_sink();
            sink.arm_first_key();
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            if case == "stop-first-cold" {
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            }
            if !stop_first {
                assert!(sink.writer_first_key(), "{case}: the writer keys first");
                append(
                    &path,
                    serde_json::json!({"type":"system","subtype":"turn_duration"}),
                );
            }
            let _ = fx.take_calls();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            assert_eq!(escapes(&fx), 0, "{case}");
            let cancelled = token.cancelled.load(Ordering::Acquire);
            match stop_first {
                true => {
                    assert!(
                        matches!(result, CommandStop::Stop(_)),
                        "{case}: main cancels"
                    );
                    assert!(cancelled, "{case}");
                    assert!(!sink.writer_first_key(), "{case}: the writer sends no key");
                }
                false => {
                    assert!(
                        matches!(result, CommandStop::Native(_)),
                        "{case}: unconfirmed"
                    );
                    assert!(!cancelled, "{case}: no main cancel over a possible write");
                    assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
                    assert_eq!(tombstone(channel), None, "{case}");
                }
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

/// An applied input is pinned within the stop budget, from a row no reader saw or one landing
/// later, and escaped; one that never lands gets no Escape and no main cancel.
#[test]
fn an_applied_input_is_pinned_within_the_stop_budget_before_the_stop_judges() {
    let fx = Fixture::new();
    run(async {
        let cases = [("unread-row", 1), ("row-lands-later", 1), ("no-row", 0)];
        for (n, (case, sent)) in cases.into_iter().enumerate() {
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_754_200 + n as u64);
            let session = format!("native-gate-pin-{case}");
            let (token, path) = unpinned_generating_turn(&fx, &session);
            let sink = token.native_stop.receipt_sink();
            sink.arm_first_key();
            assert!(sink.writer_first_key());
            let receipt = UserReceipt::before(&session, "go", Some("fixture-sid")).unwrap();
            sink.receipt(receipt);
            sink.finished(&InputRun::Applied);
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let lands = match case {
                "unread-row" => {
                    append(&path, own_row(&session));
                    append(&path, tool_use());
                    None
                }
                "row-lands-later" => {
                    let (path, row) = (path.clone(), own_row(&session));
                    Some(tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        append(&path, row);
                        append(&path, tool_use());
                    }))
                }
                _ => None,
            };
            let _ = fx.take_calls();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            if let Some(lands) = lands {
                lands.await.unwrap();
            }
            assert!(matches!(result, CommandStop::Native(_)), "{case}");
            assert_eq!(escapes(&fx), sent, "{case}");
            assert!(
                !token.cancelled.load(Ordering::Acquire),
                "{case}: no raw cancel"
            );
            assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
            assert_eq!(tombstone(channel), None, "{case}");
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}
