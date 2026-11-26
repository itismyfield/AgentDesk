use super::*;
use std::io::{BufRead, BufReader, Write};

#[test]
fn warm_wrapper_terminal_proof_is_visible_to_the_result_callback() {
    use crate::services::provider::{WrapperTerminalCapture, poll_output_file_until_result};

    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let pane = format!("wrapper-reader-order-{}", uuid::Uuid::new_v4().simple());
    let path = |ext| crate::services::tmux_common::session_temp_path(&pane, ext);
    let output = path("jsonl");
    std::fs::write(&output, "").unwrap();
    std::fs::write(path("generation"), "wrapper generation").unwrap();
    let capture = WrapperTerminalCapture::before_input(&pane, &output, 0, "callback-nonce")
        .expect("input source capture");
    let line = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"answer\"}\n";
    std::fs::write(&output, line).unwrap();
    let end = line.len() as u64;
    let outcome = WrapperTerminalCapture::around_read(Some(capture), || {
        poll_output_file_until_result(
            &output,
            0,
            None,
            &mut false,
            || true,
            || false,
            |_| {},
            |_, done| {
                let bytes = std::fs::read(path("headless-terminal")).expect("proof before Done");
                let ledger: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    ledger["reads"],
                    serde_json::json!([{ "start": 0, "end": end, "nonce": "callback-nonce" }])
                );
                *done = true;
                true
            },
            |done| *done,
            |_| panic!("expected a real terminal result"),
            |_| {},
            |_| {},
        )
    })
    .unwrap();
    assert_eq!(outcome, ReadOutputResult::Completed { offset: end });
}

#[test]
fn warm_wrapper_records_only_the_completed_result_line_before_handoff() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let pane = format!("wrapper-reader-{}", uuid::Uuid::new_v4().simple());
    let path = |ext| crate::services::tmux_common::session_temp_path(&pane, ext);
    let output = path("jsonl");
    let fifo = path("input");
    let history = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fake-session\"}\n";
    let result = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"answer\"}\n";
    let later = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"later\"}\n";
    std::fs::write(&output, history).unwrap();
    std::fs::write(path("generation"), b"wrapper generation").unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let nonce = "33333333333333333333333333333333";
    let token = std::sync::Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce.into())));

    let wrapper_fifo = fifo.clone();
    let wrapper_output = output.clone();
    let wrapper = std::thread::spawn(move || {
        let mut command = String::new();
        BufReader::new(std::fs::File::open(wrapper_fifo).unwrap())
            .read_line(&mut command)
            .unwrap();
        assert!(command.contains("run once"));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(wrapper_output)
            .unwrap();
        file.write_all(format!("{result}{later}").as_bytes())
            .unwrap();
    });
    assert_eq!(
        send_followup_to_tmux("run once", &output, &fifo, sender, Some(token), &pane).unwrap(),
        ClaudeFollowupResult::Delivered
    );
    crate::test_support::wait::join_within(wrapper, Duration::from_secs(5));
    assert!(
        receiver
            .try_iter()
            .any(|frame| matches!(frame, StreamMessage::Done { result, .. } if result == "answer"))
    );
    let reads: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path("headless-terminal")).expect("wrapper terminal evidence"),
    )
    .unwrap();
    assert_eq!(
        reads["reads"],
        serde_json::json!([{
            "start": history.len(), "end": history.len() + result.len(), "nonce": nonce,
        }])
    );
    assert!(std::fs::metadata(&output).unwrap().len() > (history.len() + result.len()) as u64);
}
