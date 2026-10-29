use super::*;
use serde_json::{Value, json};
use std::io::Write;

struct Fixture {
    _runtime: crate::config::TestRuntimeRootGuard,
    _dir: tempfile::TempDir,
    path: PathBuf,
    session: String,
    frame: String,
}

impl Fixture {
    fn new() -> Self {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.jsonl");
        std::fs::write(&path, "").unwrap();
        Self {
            _runtime: runtime,
            _dir: dir,
            path,
            session: format!("append-{}", uuid::Uuid::new_v4()),
            frame: "[User: ann (ID: 7)] same words".into(),
        }
    }
    fn register(&self, id: &str, message: Option<u64>, separator: bool) {
        register(
            (&self.session, id, "7"),
            &self.path,
            std::fs::metadata(&self.path).unwrap().len(),
            &self.frame,
            "target",
            message,
            separator,
        )
        .unwrap();
    }
    fn record(&self, row: &str, prompt: &str) -> (u64, Value) {
        let record = json!({"type":"event_msg", "payload":{"type":"item_completed", "turn_id":"native",
            "item":{"type":"UserMessage", "id":row, "content":[{"type":"text", "text":prompt}]}}});
        let offset = std::fs::metadata(&self.path).unwrap().len();
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap(),
            "{record}"
        )
        .unwrap();
        (offset, record)
    }
    fn bind(&self, start: u64, record: &Value) -> Receipt {
        bind_record(&self.session, &self.path, start, record).unwrap()
    }
}

#[test]
fn append_mixed_prompt_is_deferred_until_its_completion_row_and_then_known() {
    let f = Fixture::new();
    f.register("A", Some(1), true);
    let prompt = format!("draft\n{}", f.frame);
    assert!(pending_candidate(
        "codex",
        &f.session,
        std::slice::from_ref(&prompt),
        None
    ));
    assert!(
        !super::super::known(&prompt),
        "mixed suffix never broadens plain classification"
    );
    let response = json!({"type":"response_item", "payload":{"type":"message", "role":"user", "id":"response-a", "content":[{"type":"input_text","text":prompt}]}});
    let (start, record) = f.record("row-a", &prompt);
    assert!(bind_record(&f.session, &f.path, start, &response).is_none());
    assert!(!projection("codex", &f.session, "A").unwrap().bound);
    let receipt = f.bind(start, &record);
    assert_eq!(receipt.attempt.id, "A");
    assert_eq!(receipt.prompt, prompt);
    assert!(
        matches!(super::super::observed_attempt("codex", &f.session, std::slice::from_ref(&prompt), Some("row-a")), ObservedAttempt::Known(id) if id == "A")
    );
    assert!(pending_candidate(
        "codex",
        &f.session,
        &[prompt],
        Some("response-a")
    ));
}

#[test]
fn append_admission_is_monotonic_and_keeps_unbound_identical_input_reserved() {
    let f = Fixture::new();
    f.register("A", None, false);
    mark("A", &f.path, Admission::EffectPossible).unwrap();
    mark("A", &f.path, Admission::Admitted).unwrap();
    mark("A", &f.path, Admission::Uncertain).unwrap();
    assert!(projection("codex", &f.session, "A").unwrap().admitted);
    assert!(
        register(
            (&f.session, "B", "7"),
            &f.path,
            0,
            &f.frame,
            "target",
            None,
            false
        )
        .is_err()
    );
    super::super::withdraw("A", &f.path);
    assert!(
        projection("codex", &f.session, "A").is_some(),
        "admission cannot be withdrawn"
    );
    assert_eq!(replay_start(&f.session, &f.path), Some(0));
}

#[test]
fn append_bound_first_and_late_admission_do_not_erase_row_or_decision() {
    let f = Fixture::new();
    f.register("A", Some(11), false);
    let (start, record) = f.record("row-a", &f.frame);
    f.bind(start, &record);
    assert!(settle("A", &f.session, "row-a", Decision::Joined));
    mark("A", &f.path, Admission::Admitted).unwrap();
    let projection = projection("codex", &f.session, "A").unwrap();
    assert!(projection.admitted && projection.bound);
    assert_eq!(projection.message_id, Some(11));
    assert_eq!(projection.row.as_deref(), Some("row-a"));
    assert_eq!(projection.decision, Some(Decision::Joined));
    assert!(!settle("A", &f.session, "row-a", Decision::HandedPending));
    assert!(!settle("A", &f.session, "other-row", Decision::Joined));
    assert_eq!(
        replay_start(&f.session, &f.path),
        None,
        "completed Joined does not extend cold replay"
    );
}

#[test]
fn append_distinct_same_body_attempts_cannot_take_each_others_completed_row() {
    let f = Fixture::new();
    f.register("A", Some(1), false);
    let (start_a, row_a) = f.record("row-a", &f.frame);
    f.bind(start_a, &row_a);
    f.register("B", Some(2), false);
    let replay = f.bind(start_a, &row_a);
    assert_eq!(replay.attempt.id, "A");
    assert!(!projection("codex", &f.session, "B").unwrap().bound);
    let (start_b, row_b) = f.record("row-b", &f.frame);
    assert_eq!(f.bind(start_b, &row_b).attempt.id, "B");
    assert_eq!(f.bind(start_a, &row_a).attempt.id, "A");
    assert!(
        bind_record(&f.session, &f.path, start_b, &row_a).is_none(),
        "same id at another physical row is not a new receipt"
    );
}

#[test]
fn append_suffix_is_exact_and_does_not_accept_middle_repeated_or_transformed_text() {
    let f = Fixture::new();
    f.register("A", Some(1), true);
    for prompt in [
        format!("draft\n{}\n", f.frame),
        format!("draft\n{} more", f.frame),
        format!("{}\ndraft\n{}", f.frame, f.frame),
        format!("draft{}", f.frame),
        format!("draft\n{}", f.frame.replace("same words", "else words")),
    ] {
        let (start, record) = f.record("wrong", &prompt);
        assert!(
            bind_record(&f.session, &f.path, start, &record).is_none(),
            "{prompt}"
        );
    }
    assert!(!projection("codex", &f.session, "A").unwrap().bound);
    let (start, record) = f.record("right", &format!("draft\n{}", f.frame));
    assert_eq!(f.bind(start, &record).attempt.id, "A");
}

#[test]
fn append_accepts_native_image_text_shape_without_relaxing_unknown_blocks() {
    let f = Fixture::new();
    f.register("A", Some(1), true);
    let (start, mut record) = f.record("image-row", &format!("[Image #1] \ndraft\n{}", f.frame));
    record["payload"]["item"]["content"][0]["text_elements"] =
        json!([{"byte_range":{"start":0,"end":10},"placeholder":"[Image #1]"}]);
    let text = record["payload"]["item"]["content"][0].clone();
    for block in [
        json!({"type":"input_image","image_url":"x"}),
        json!({"type":"tool_result","text":"x"}),
        json!({"type":"text","text":null}),
    ] {
        record["payload"]["item"]["content"] = json!([block, text]);
        assert!(bind_record(&f.session, &f.path, start, &record).is_none());
    }
    record["payload"]["item"]["content"] =
        json!([{"type":"local_image","path":"/lane/image.png"},text]);
    assert_eq!(f.bind(start, &record).row, "image-row");
}

// Telling a replaced path from the registered file needs Unix device/inode identity.
#[cfg(unix)]
#[test]
fn append_replaced_or_truncated_source_and_other_session_cannot_bind() {
    let f = Fixture::new();
    f.register("A", None, false);
    let (start, record) = f.record("row-a", &f.frame);
    assert!(bind_record("another-session", &f.path, start, &record).is_none());
    let old = f.path.with_extension("old");
    std::fs::rename(&f.path, &old).unwrap();
    std::fs::write(&f.path, format!("{record}\n")).unwrap();
    assert!(bind_record(&f.session, &f.path, start, &record).is_none());
    assert!(projection("codex", &f.session, "A").is_none());
    std::fs::remove_file(&f.path).unwrap();
    std::fs::rename(old, &f.path).unwrap();
    f.bind(start, &record);
    f.register("B", Some(2), true);
    std::fs::write(&f.path, "").unwrap();
    assert!(projection("codex", &f.session, "B").is_none());
}

#[test]
fn append_old_exact_entries_do_not_gain_suffix_or_replay_authority() {
    let f = Fixture::new();
    super::super::register(("codex", &f.session, "old", "7"), &f.path, 0, &f.frame).unwrap();
    assert!(projection("codex", &f.session, "old").is_none());
    assert_eq!(replay_start(&f.session, &f.path), None);
    assert!(!pending_candidate(
        "codex",
        &f.session,
        &[format!("draft\n{}", f.frame)],
        None
    ));
    let (start, record) = f.record("row-a", &f.frame);
    assert!(bind_record(&f.session, &f.path, start, &record).is_none());
}

#[test]
fn append_handed_pending_survives_reload_without_changing_its_row_decision() {
    let f = Fixture::new();
    f.register("A", None, false);
    let (start, record) = f.record("row-a", &f.frame);
    f.bind(start, &record);
    assert!(settle("A", &f.session, "row-a", Decision::HandedPending));
    let state = projection("codex", &f.session, "A").unwrap();
    assert_eq!(state.decision, Some(Decision::HandedPending));
    assert!(!settle("A", &f.session, "row-a", Decision::Joined));
    assert_eq!(
        f.bind(start, &record).attempt.decision,
        Some(Decision::HandedPending)
    );
}

#[test]
fn append_candidate_does_not_hide_an_old_exact_rows_owner() {
    let f = Fixture::new();
    super::super::register(("codex", &f.session, "old", "7"), &f.path, 0, &f.frame).unwrap();
    let (start, _) = f.record("old-row", &f.frame);
    assert_eq!(
        super::super::receipt("old", &f.path, start, "codex", Some("old-row"), &f.frame),
        Some(true)
    );
    f.register("new", Some(2), false);
    assert!(!pending_candidate(
        "codex",
        &f.session,
        std::slice::from_ref(&f.frame),
        Some("old-row")
    ));
    assert!(
        matches!(super::super::observed_attempt("codex", &f.session, std::slice::from_ref(&f.frame), Some("old-row")), ObservedAttempt::Known(id) if id == "old")
    );
    assert!(!projection("codex", &f.session, "new").unwrap().bound);
}

#[test]
fn append_path_only_binder_requires_one_current_session_owner() {
    let f = Fixture::new();
    f.register("A", Some(1), false);
    register(
        ("another-owner", "B", "7"),
        &f.path,
        0,
        "another frame",
        "target",
        None,
        false,
    )
    .unwrap();
    let (start, record) = f.record("row-a", &f.frame);
    assert!(bind_record_at(&f.path, start, &record).is_none());
    assert_eq!(f.bind(start, &record).attempt.id, "A");
}

#[test]
fn append_open_file_binder_requires_one_current_source_owner() {
    let f = Fixture::new();
    f.register("A", Some(1), false);
    let file = std::fs::File::open(&f.path).unwrap();
    let (start, record) = f.record("row-a", &f.frame);
    assert!(bind_open_record(&file, start, &json!({"type":"response_item"})).is_none());
    assert_eq!(
        bind_open_record(&file, start, &record).unwrap().attempt.id,
        "A"
    );
    register(
        ("another-owner", "B", "7"),
        &f.path,
        start,
        "another frame",
        "target",
        None,
        false,
    )
    .unwrap();
    assert!(bind_open_record(&file, start, &record).is_none());
}

// Telling a replaced path from the registered file needs Unix device/inode identity.
#[cfg(unix)]
#[test]
fn append_open_file_binder_rejects_an_atomically_replaced_registered_path() {
    let f = Fixture::new();
    f.register("A", None, false);
    let file = std::fs::File::open(&f.path).unwrap();
    let (start, record) = f.record("row-a", &f.frame);
    let old = f.path.with_extension("old");
    std::fs::rename(&f.path, &old).unwrap();
    std::fs::write(&f.path, format!("{record}\n")).unwrap();
    assert!(bind_open_record(&file, start, &record).is_none());
    assert!(projection("codex", &f.session, "A").is_none());
}

#[test]
fn append_does_not_invent_a_prompt_by_joining_multiple_native_text_blocks() {
    let f = Fixture::new();
    f.register("A", Some(1), true);
    let record = json!({"type":"event_msg", "payload":{"type":"item_completed", "turn_id":"native",
        "item":{"type":"UserMessage", "id":"row-a", "content":[{"type":"text", "text":"draft"},{"type":"text", "text":f.frame}]}}});
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.path)
            .unwrap(),
        "{record}"
    )
    .unwrap();
    assert!(bind_record(&f.session, &f.path, 0, &record).is_none());
    assert!(!projection("codex", &f.session, "A").unwrap().bound);
}
