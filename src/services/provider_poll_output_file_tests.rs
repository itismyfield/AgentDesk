use super::{CancelToken, ReadOutputResult, poll_output_file_until_result};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[test]
fn missing_output_file_reports_session_died_when_process_already_exited() {
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("missing.jsonl");
    let mut state = ();

    let result = poll_output_file_until_result(
        missing_path.to_str().unwrap(),
        12,
        None,
        &mut state,
        || false,
        || false,
        |_| {},
        |_, _| true,
        |_| false,
        |_| true,
        |_| {},
        |_| {},
    )
    .unwrap();

    assert_eq!(result, ReadOutputResult::SessionDied { offset: 12 });
}

#[test]
fn existing_output_file_emits_start_offset_before_new_bytes() {
    #[derive(Default)]
    struct TestState {
        saw_done: bool,
        lines: Vec<String>,
    }

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("stream.jsonl");
    let previous = "old\n";
    std::fs::write(&output_path, format!("{previous}DONE\n")).unwrap();
    let start_offset = previous.len() as u64;

    let mut state = TestState::default();
    let mut offsets = Vec::new();
    let result = poll_output_file_until_result(
        output_path.to_str().unwrap(),
        start_offset,
        None,
        &mut state,
        || true,
        || false,
        |offset| offsets.push(offset),
        |line: &str, state| {
            state.lines.push(line.to_string());
            state.saw_done = line == "DONE";
            true
        },
        |state| state.saw_done,
        |_| true,
        |_| {},
        |file| {
            assert_eq!(
                file.metadata().unwrap().len(),
                (previous.len() + "DONE\n".len()) as u64
            );
            // The reader must keep the descriptor it reported even when
            // another producer replaces the pathname before the first read.
            std::fs::rename(&output_path, output_path.with_extension("captured")).unwrap();
            std::fs::write(&output_path, "FOREIGN\n").unwrap();
        },
    )
    .unwrap();

    let file_len = std::fs::metadata(output_path.with_extension("captured"))
        .unwrap()
        .len();
    assert_eq!(result, ReadOutputResult::Completed { offset: file_len });
    assert_eq!(state.lines, vec!["DONE".to_string()]);
    assert_eq!(offsets, vec![start_offset, file_len]);
}

#[test]
fn unterminated_tail_bytes_do_not_advance_committed_offset() {
    #[derive(Default)]
    struct TestState {
        lines: Vec<String>,
    }

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("stream.jsonl");
    let complete = "complete\n";
    std::fs::write(&output_path, format!("{complete}unterminated")).unwrap();
    let safe_offset = complete.len() as u64;
    let cancel_token = Arc::new(CancelToken::new());
    let cancel_from_line = cancel_token.clone();

    let mut state = TestState::default();
    let mut offsets = Vec::new();
    let result = poll_output_file_until_result(
        output_path.to_str().unwrap(),
        0,
        Some(cancel_token),
        &mut state,
        || true,
        || false,
        |offset| offsets.push(offset),
        |line: &str, state| {
            state.lines.push(line.to_string());
            cancel_from_line.cancelled.store(true, Ordering::Relaxed);
            true
        },
        |_| false,
        |_| true,
        |_| {},
        |_| {},
    )
    .unwrap();

    assert_eq!(
        result,
        ReadOutputResult::Cancelled {
            offset: safe_offset
        }
    );
    assert_eq!(state.lines, vec!["complete".to_string()]);
    assert_eq!(offsets, vec![safe_offset]);
}
