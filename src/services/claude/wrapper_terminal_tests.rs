use super::*;
use std::io::{BufRead, BufReader, Write};

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

    std::thread::scope(|scope| {
        let wrapper = scope.spawn(|| {
            let mut command = String::new();
            BufReader::new(std::fs::File::open(&fifo).unwrap())
                .read_line(&mut command)
                .unwrap();
            assert!(command.contains("run once"));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&output)
                .unwrap();
            file.write_all(format!("{result}{later}").as_bytes())
                .unwrap();
        });
        assert_eq!(
            send_followup_to_tmux("run once", &output, &fifo, sender, Some(token), &pane).unwrap(),
            ClaudeFollowupResult::Delivered
        );
        wrapper.join().unwrap();
    });
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
