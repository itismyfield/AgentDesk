use super::{frame, transcript_submission};
use crate::services::tui_input::human_frame::history;
use serde_json::json;
use std::io::Write;

#[test]
fn a_previous_same_author_drain_does_not_contaminate_the_next_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("receipt-interleave-{}", uuid::Uuid::new_v4());
    let first = frame("discord", "720205", "ann", "ca720251", "first body");
    let second = frame("discord", "720205", "ann", "ca720252", "second body");
    let append = |record: serde_json::Value| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    };
    history::register(("claude", &session, "ca720251", "720205"), &path, 0, &first).unwrap();
    append(json!({
        "type": "queue-operation", "operation": "enqueue",
        "uuid": "enqueue-a", "content": first,
    }));
    assert_eq!(
        transcript_submission(&path, 0, "ca720251", Some(&first)),
        Some(true)
    );
    let offset = std::fs::metadata(&path).unwrap().len();
    history::register(
        ("claude", &session, "ca720252", "720205"),
        &path,
        offset,
        &second,
    )
    .unwrap();

    append(json!({
        "type": "attachment",
        "attachment": {
            "type": "queued_command", "commandMode": "prompt",
            "source_uuid": "command-a", "prompt": first,
        },
    }));
    append(json!({
        "type": "user", "uuid": "late-user-a", "message": { "content": first },
    }));
    assert_eq!(
        transcript_submission(&path, offset, "ca720252", Some(&second)),
        None,
        "a prior same-author drain must not contaminate the next frame"
    );
    assert_eq!(history::receipt_row("ca720252", &path), None);

    append(json!({
        "type": "queue-operation", "operation": "enqueue",
        "uuid": "enqueue-b", "content": second,
    }));
    for _ in 0..2 {
        assert_eq!(
            transcript_submission(&path, offset, "ca720252", Some(&second)),
            Some(true),
            "B must confirm from its own enqueue despite A's earlier drain"
        );
    }
    assert_eq!(
        history::receipt_row("ca720251", &path).as_deref(),
        Some("enqueue-a")
    );
    assert_eq!(
        history::receipt_row("ca720252", &path).as_deref(),
        Some("enqueue-b")
    );
    let history_path = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(history_path).unwrap()).unwrap();
    let receipts = stored["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "ca720252")
        .unwrap()["events"]
        .as_array()
        .unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "B's receipt must be persisted exactly once"
    );
    assert_eq!(receipts[0]["row"], "enqueue-b");
}

/// A previous frame that extends the next one is that attempt's drain, never the next receipt.
#[test]
fn a_previous_frame_containing_the_next_one_is_not_its_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("receipt-prefix-{}", uuid::Uuid::new_v4());
    let body = "첫 줄\n둘째 줄\n셋째 줄";
    let first = frame(
        "discord",
        "720205",
        "ann",
        "ca720261",
        &format!("{body}\n추가 지시"),
    );
    let second = frame("discord", "720205", "ann", "ca720262", body);
    assert!(first.contains(&second));
    let append = |record: serde_json::Value| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    };
    history::register(("claude", &session, "ca720261", "720205"), &path, 0, &first).unwrap();
    append(json!({
        "type": "queue-operation", "operation": "enqueue",
        "uuid": "enqueue-a", "content": first,
    }));
    let offset = std::fs::metadata(&path).unwrap().len();
    history::register(
        ("claude", &session, "ca720262", "720205"),
        &path,
        offset,
        &second,
    )
    .unwrap();
    append(json!({
        "type": "attachment",
        "attachment": {
            "type": "queued_command", "commandMode": "prompt",
            "source_uuid": "command-a", "prompt": first,
        },
    }));
    assert_eq!(
        transcript_submission(&path, offset, "ca720262", Some(&second)),
        None,
        "A's drain contains B's frame but is A's"
    );
    append(json!({
        "type": "queue-operation", "operation": "enqueue",
        "uuid": "enqueue-b", "content": second,
    }));
    assert_eq!(
        transcript_submission(&path, offset, "ca720262", Some(&second)),
        Some(true)
    );
}

/// A's frame submitted behind a person's draft is A's mixed receipt; its later drain contains B's
/// frame but stays A's, while B's own row behind another draft is still B's receipt.
#[test]
fn a_mixed_row_an_earlier_attempt_took_is_not_the_next_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("receipt-mixed-{}", uuid::Uuid::new_v4());
    let body = "첫 줄\n둘째 줄\n셋째 줄";
    let first = frame(
        "discord",
        "720205",
        "ann",
        "ca720271",
        &format!("{body}\n추가 지시"),
    );
    let second = frame("discord", "720205", "ann", "ca720272", body);
    let append = |record: serde_json::Value| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    };
    history::register(("claude", &session, "ca720271", "720205"), &path, 0, &first).unwrap();
    let mixed = format!("사람 초안\n{first}");
    append(json!({"type": "queue-operation", "operation": "enqueue", "content": mixed}));
    assert_eq!(
        transcript_submission(&path, 0, "ca720271", Some(&first)),
        Some(false)
    );
    let offset = std::fs::metadata(&path).unwrap().len();
    history::register(
        ("claude", &session, "ca720272", "720205"),
        &path,
        offset,
        &second,
    )
    .unwrap();
    append(json!({"type": "attachment", "attachment": {
        "type": "queued_command", "commandMode": "prompt", "prompt": mixed}}));
    assert_eq!(
        transcript_submission(&path, offset, "ca720272", Some(&second)),
        None,
        "A's mixed drain stays A's"
    );
    append(json!({"type": "queue-operation", "operation": "enqueue",
        "content": format!("다른 초안\n{second}")}));
    assert_eq!(
        transcript_submission(&path, offset, "ca720272", Some(&second)),
        Some(false),
        "B's own row behind a draft is B's"
    );
}

/// The current frame recorded with an extra non-text block is still this attempt's submission.
#[test]
fn the_current_frame_beside_an_image_is_its_own_mixed_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("receipt-image-{}", uuid::Uuid::new_v4());
    let text = frame("discord", "720205", "ann", "ca720281", "그림과 함께");
    history::register(("claude", &session, "ca720281", "720205"), &path, 0, &text).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let content = json!([{"type": "text", "text": text},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ""}}]);
    let record = json!({"type": "user", "message": {"content": content}});
    writeln!(file, "{record}").unwrap();
    assert_eq!(
        transcript_submission(&path, 0, "ca720281", Some(&text)),
        Some(false)
    );
}
