use super::{RotatingJsonlWriter, run_turn};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

fn run_stream(events: &[Value], exit_code: i32) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("fake-codex");
    std::fs::write(
        &cli,
        format!("#!/bin/sh\ncat \"$0.jsonl\"\nexit {exit_code}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let input = events
        .iter()
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    std::fs::write(cli.with_extension("jsonl"), input).unwrap();
    let path = dir.path().join("output.jsonl");
    let mut output = RotatingJsonlWriter::open(&path).unwrap();
    let mut thread_id = None;
    let outcome = run_turn(
        &mut output,
        cli.to_str().unwrap(),
        None,
        None,
        None,
        dir.path().to_str().unwrap(),
        "test",
        &mut thread_id,
        None,
        None,
        None,
        &[],
    );
    assert!(
        outcome.is_ok(),
        "event-reported turn must end once: {outcome:?}"
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn only_result(rows: &[Value]) -> &Value {
    let results = rows
        .iter()
        .filter(|row| row["type"] == "result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "one terminal result per turn: {rows:?}");
    results[0]
}

#[test]
fn reconnect_error_then_completed_emits_one_success_with_final_text() {
    let rows = run_stream(
        &[
            json!({"type":"thread.started","thread_id":"reconnected"}),
            json!({"type":"error","message":"Reconnecting... 1/5 (stream disconnected)"}),
            json!({"type":"item.completed","item":{"type":"agent_message","text":"final"}}),
            json!({"type":"turn.completed"}),
        ],
        0,
    );
    let result = only_result(&rows);
    assert_eq!(
        result["subtype"], "success",
        "a recovered turn must complete successfully"
    );
    assert_eq!(result["result"], "final");
    assert_eq!(result["session_id"], "reconnected");
    assert_eq!(
        rows.iter().filter(|row| row["type"] == "assistant").count(),
        1
    );
}

#[test]
fn error_at_eof_emits_one_error_even_on_nonzero_exit() {
    for exit_code in [0, 1] {
        let rows = run_stream(
            &[
                json!({"type":"thread.started","thread_id":"failed"}),
                json!({"type":"error","message":"connection failed"}),
            ],
            exit_code,
        );
        let result = only_result(&rows);
        assert_eq!(result["subtype"], "error_during_execution");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["errors"], json!(["connection failed"]));
    }
}

#[test]
fn repeated_errors_then_turn_failed_emit_one_existing_failure_result() {
    let rows = run_stream(
        &[
            json!({"type":"thread.started","thread_id":"failed"}),
            json!({"type":"error","message":"Reconnecting... 1/5"}),
            json!({"type":"error","message":"connection failed"}),
            json!({"type":"turn.failed","error":{"message":"connection failed"}}),
        ],
        1,
    );
    let result = only_result(&rows);
    assert_eq!(result["subtype"], "error_during_execution");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["errors"], json!(["connection failed"]));
}

#[test]
fn completion_aliases_and_trailing_errors_do_not_duplicate_result() {
    for terminal in [
        json!({"type":"task_complete","last_agent_message":"final"}),
        json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"final"}}),
    ] {
        let rows = run_stream(
            &[
                json!({"type":"error","message":"Reconnecting... 1/5"}),
                terminal,
                json!({"type":"turn.completed"}),
                json!({"type":"error","message":"late transport noise"}),
            ],
            0,
        );
        let result = only_result(&rows);
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["result"], "final");
    }
}

#[test]
fn eof_without_error_keeps_synthetic_success() {
    let rows = run_stream(
        &[json!({"type":"item.completed","item":{"type":"agent_message","text":"final"}})],
        0,
    );
    let result = only_result(&rows);
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["result"], "final");
}

mod reader {
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::{CancelToken, ReadOutputResult, SessionProbe};
    use crate::services::session_backend::read_output_file_until_result_tracked;
    use crate::services::tmux_common::{WrapperSentinel, emit_wrapper_sentinel};
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    #[test]
    fn recovered_error_completes_reader_while_old_shape_stays_pending() {
        let recovered = super::run_stream(
            &[
                json!({"type":"thread.started","thread_id":"thread-reader"}),
                json!({"type":"error","message":"Reconnecting... 1/5"}),
                json!({"type":"item.completed","item":{"type":"agent_message","text":"final"}}),
                json!({"type":"turn.completed"}),
            ],
            0,
        );
        let old = vec![
            json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":["Reconnecting... 1/5"]}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"final"}]}}),
        ];

        for (lines, should_complete) in [(recovered, true), (old, false)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("reader.jsonl");
            let body = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(&path, &body).unwrap();
            emit_wrapper_sentinel(
                path.to_str().unwrap(),
                WrapperSentinel::ReadyForInput { provider: "codex" },
            );
            let cancel = Arc::new(CancelToken::new());
            let reader_cancel = cancel.clone();
            let (sender, messages) = mpsc::channel();
            let (finished_sender, finished) = mpsc::channel();
            let reader_path = path.to_string_lossy().into_owned();
            let reader = std::thread::spawn(move || {
                let result = read_output_file_until_result_tracked(
                    &reader_path,
                    0,
                    sender,
                    Some(reader_cancel),
                    SessionProbe::new(|| true, || false),
                );
                let _ = finished_sender.send(());
                result
            });

            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut saw_text = false;
            while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                let Ok(message) = messages.recv_timeout(remaining) else {
                    break;
                };
                saw_text =
                    matches!(&message, StreamMessage::Text { content } if content == "final");
                received.push(message);
                if saw_text {
                    break;
                }
            }
            let wait = if should_complete {
                Duration::from_secs(3)
            } else {
                Duration::from_millis(100)
            };
            let before_cancel = finished.recv_timeout(wait);
            // Always stop and join the reader before asserting, including a regressed fast path.
            cancel.cancelled.store(true, Ordering::Relaxed);
            let joined = reader.join();
            received.extend(messages.try_iter());

            assert!(
                saw_text,
                "reader must consume the assistant before its completion is judged"
            );
            let result = joined.expect("reader panicked").expect("reader failed");
            assert_eq!(received.iter().filter(|message| matches!(message, StreamMessage::Text { content } if content == "final")).count(), 1);
            assert!(
                !received
                    .iter()
                    .any(|message| matches!(message, StreamMessage::Error { .. }))
            );
            if should_complete {
                assert!(
                    before_cancel.is_ok(),
                    "success must complete without a pane readiness probe"
                );
                assert_eq!(
                    result,
                    ReadOutputResult::Completed {
                        offset: body.len() as u64
                    }
                );
                assert_eq!(received.iter().filter(|message| matches!(message, StreamMessage::Done { result, .. } if result == "final")).count(), 1);
            } else {
                assert!(
                    matches!(before_cancel, Err(mpsc::RecvTimeoutError::Timeout)),
                    "old error and sentinel must leave the live reader pending"
                );
                assert!(matches!(result, ReadOutputResult::Cancelled { .. }));
                assert!(
                    !received
                        .iter()
                        .any(|message| matches!(message, StreamMessage::Done { .. }))
                );
            }
        }
    }
}
