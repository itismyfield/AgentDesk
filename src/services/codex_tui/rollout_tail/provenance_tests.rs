use super::*;
use crate::services::tui_o::shadow::capture::file_identity;
use std::io::Write;

struct Fixture {
    _dir: tempfile::TempDir,
    source: SourceId,
    header_end: u64,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let header = serde_json::json!({"type":"session_meta", "payload":{
            "id":"parent", "cwd":dir.path(), "source":"cli", "originator":"codex-tui"
        }})
        .to_string()
            + "\n";
        std::fs::write(&path, &header).unwrap();
        let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
        Self {
            _dir: dir,
            source: SourceId {
                session_id: "parent".into(),
                path,
                dev,
                ino,
            },
            header_end: header.len() as u64,
        }
    }

    fn append(&self, value: Value) -> (u64, u64) {
        let start = self.len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.source.path)
            .unwrap();
        writeln!(file, "{value}").unwrap();
        (start, self.len())
    }

    fn event(&self, kind: &str, id: Option<&str>) -> (u64, u64) {
        self.append(serde_json::json!({"type":"event_msg", "payload":{"type":kind,"turn_id":id}}))
    }

    fn len(&self) -> u64 {
        std::fs::metadata(&self.source.path).unwrap().len()
    }

    fn anchor(&self) -> NativeTurnAnchor {
        scan_anchor(&self.source, self.header_end, self.len())
            .unwrap()
            .unwrap()
    }
}

#[test]
fn t0a_no_submit_opener_has_no_attribution() {
    let fixture = Fixture::new();
    assert_eq!(
        scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
        Ok(None)
    );
    fixture.event("task_started", None);
    assert_eq!(
        scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
        Err("anonymous_native_opener")
    );
}

#[test]
fn anonymous_opener_mixed_with_named_opener_is_unknown() {
    for anonymous_first in [false, true] {
        let fixture = Fixture::new();
        for id in if anonymous_first {
            [None, Some("old")]
        } else {
            [Some("old"), None]
        } {
            fixture.event("task_started", id);
        }
        assert_eq!(
            scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
            Err("anonymous_native_opener")
        );
    }
}

#[test]
fn codex_0162_confirmed_metadata_reaches_native_anchor_and_terminal() {
    let fixture = Fixture::new();
    for line in
        include_str!("../../../../tests/fixtures/codex_provenance/0162_metadata_turn.jsonl").lines()
    {
        fixture.append(serde_json::from_str(line).unwrap());
    }
    let anchor = fixture.anchor();
    assert_eq!(anchor.native_turn_id, "old");
    assert_eq!(anchor.start, fixture.header_end);
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Ok(Some(fixture.len()))
    );
}

#[test]
fn confirmed_metadata_rejects_malformed_unknown_agent_trigger_and_foreign_turn() {
    for (kind, payload, reason) in [
        (
            "future_metadata",
            serde_json::json!({}),
            "unknown_native_schema",
        ),
        (
            "token_usage_record",
            serde_json::json!({}),
            "unknown_native_schema",
        ),
        (
            "token_usage_record",
            serde_json::json!({"turn_id":" "}),
            "unknown_native_schema",
        ),
        (
            "token_usage_record",
            serde_json::json!({"turn_id":4}),
            "unknown_native_schema",
        ),
        (
            "token_usage_record",
            serde_json::json!({"turn_id":"foreign"}),
            "foreign_native_record",
        ),
        (
            "world_state",
            serde_json::json!([]),
            "unknown_native_schema",
        ),
        (
            "world_state",
            serde_json::json!({"turn_id":null}),
            "unknown_native_schema",
        ),
        (
            "world_state",
            serde_json::json!({"full":"true"}),
            "unknown_native_schema",
        ),
        (
            "inter_agent_communication_metadata",
            serde_json::json!({}),
            "unknown_native_schema",
        ),
        (
            "inter_agent_communication_metadata",
            serde_json::json!({"trigger_turn":1}),
            "unknown_native_schema",
        ),
        (
            "inter_agent_communication_metadata",
            serde_json::json!({"trigger_turn":true}),
            "agent_triggered_native_turn",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.event("task_started", Some("old"));
        let anchor = fixture.anchor();
        fixture.append(serde_json::json!({"type":kind,"payload":payload}));
        fixture.event("task_complete", Some("old"));
        assert_eq!(
            terminal_end(&fixture.source, &anchor, fixture.len()),
            Err(reason),
            "{kind}"
        );
        assert_eq!(
            scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
            Err(reason),
            "{kind}"
        );
    }
}

#[test]
fn compact_fixture_second_turn_passes_and_child_review_turn_has_explicit_reason() {
    let fixture = Fixture::new();
    let mut second_start = 0;
    for (index, line) in
        include_str!("../../../../tests/fixtures/codex_busy_inject/compact_turn.jsonl")
            .lines()
            .enumerate()
    {
        let (start, _) = fixture.append(serde_json::from_str(line).unwrap());
        if index == 8 {
            second_start = start;
        }
    }
    assert_eq!(
        scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
        Err("multiple_native_openers")
    );
    let anchor = scan_anchor(&fixture.source, second_start, fixture.len())
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Ok(Some(fixture.len()))
    );
    let fixture = Fixture::new();
    for line in include_str!("../../../../tests/fixtures/codex_busy_inject/review_turn.jsonl")
        .lines()
        .take(10)
    {
        fixture.append(serde_json::from_str(line).unwrap());
    }
    assert_eq!(
        scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
        Err("child_native_turn")
    );
}

#[test]
fn t0b_submit_before_first_poll_recovers_named_opener_and_terminal() {
    let fixture = Fixture::new();
    let (start, _) = fixture.event("task_started", Some("old"));
    fixture.append(serde_json::json!({"type":"response_item", "payload":{
        "type":"message", "role":"assistant", "content":[{"type":"output_text","text":"original answer"}]
    }}));
    let (_, end) = fixture.event("task_complete", Some("old"));
    let anchor = fixture.anchor();
    assert_eq!(
        anchor,
        NativeTurnAnchor {
            native_turn_id: "old".into(),
            start
        }
    );
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Ok(Some(end))
    );
}

#[test]
fn t0c_observed_opener_before_span_save_uses_same_native_anchor() {
    let fixture = Fixture::new();
    let (start, boot_eof) = fixture.event("task_started", Some("old"));
    let observed = NativeTurnAnchor {
        native_turn_id: "old".into(),
        start,
    };
    let (_, end) = fixture.event("turn_aborted", Some("old"));
    let recovered = scan_anchor(&fixture.source, fixture.header_end, boot_eof)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, observed);
    assert_eq!(
        terminal_end(&fixture.source, &recovered, fixture.len()),
        Ok(Some(end))
    );
}

#[test]
fn t0d_two_named_openers_are_unknown_even_when_ids_repeat() {
    for second in ["old", "direct-input"] {
        let fixture = Fixture::new();
        fixture.event("task_started", Some("old"));
        fixture.event("task_started", Some(second));
        assert_eq!(
            scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
            Err("multiple_native_openers")
        );
    }
}

#[test]
fn boot_snapshot_eof_excludes_later_opener_and_offset_excludes_previous_turn() {
    let fixture = Fixture::new();
    fixture.event("task_started", Some("previous"));
    let (_, turn_start) = fixture.event("task_complete", Some("previous"));
    let (start, boot_eof) = fixture.event("task_started", Some("old"));
    fixture.event("task_complete", Some("old"));
    fixture.event("task_started", Some("after-boot"));
    assert_eq!(
        scan_anchor(&fixture.source, turn_start, boot_eof),
        Ok(Some(NativeTurnAnchor {
            native_turn_id: "old".into(),
            start,
        }))
    );
}

#[test]
fn native_coordinates_require_complete_record_boundaries() {
    let fixture = Fixture::new();
    let (start, end) = fixture.event("task_started", Some("old"));
    assert_eq!(
        scan_anchor(&fixture.source, start + 1, end),
        Err("nonboundary_native_offset")
    );
    assert_eq!(
        scan_anchor(&fixture.source, start, end - 1),
        Err("incomplete_native_record")
    );
    assert_eq!(scan_anchor(&fixture.source, end, end), Ok(None));
    assert_eq!(
        scan_anchor(&fixture.source, end, start),
        Err("invalid_native_bounds")
    );
}

#[test]
fn t5_eof_and_composer_ready_leave_span_open() {
    let fixture = Fixture::new();
    fixture.event("task_started", Some("old"));
    fixture.event("composer_ready", Some("old"));
    let anchor = fixture.anchor();
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Ok(None)
    );
}

#[test]
fn strict_terminal_rejects_anonymous_foreign_and_synthetic_closers() {
    for kind in ["task_complete", "turn_aborted"] {
        for id in [None, Some(""), Some(" \t "), Some("foreign")] {
            let fixture = Fixture::new();
            fixture.event("task_started", Some("old"));
            let anchor = fixture.anchor();
            fixture.event(kind, id);
            assert_eq!(
                terminal_end(&fixture.source, &anchor, fixture.len()),
                Err("unnamed_or_foreign_terminal")
            );
        }
        let fixture = Fixture::new();
        fixture.event("task_started", Some("old"));
        let anchor = fixture.anchor();
        fixture.append(serde_json::json!({"type":"event_msg", "payload":{"type":kind,"turn_id":"old","synthetic":true}}));
        assert_eq!(
            terminal_end(&fixture.source, &anchor, fixture.len()),
            Err("unknown_native_schema")
        );
    }
}

#[test]
fn replay_rejects_successor_duplicate_unknown_schema_and_torn_record() {
    for id in [None, Some("old"), Some("successor")] {
        let fixture = Fixture::new();
        fixture.event("task_started", Some("old"));
        let anchor = fixture.anchor();
        fixture.event("task_started", id);
        fixture.event("task_complete", Some("old"));
        assert_eq!(
            terminal_end(&fixture.source, &anchor, fixture.len()),
            Err("successor_or_duplicate_opener")
        );
    }
    let fixture = Fixture::new();
    fixture.event("task_started", Some("old"));
    let anchor = fixture.anchor();
    fixture.event("future_boundary", Some("old"));
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Err("unknown_native_schema")
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.source.path)
        .unwrap();
    write!(file, "{{\"type\":\"event_msg\"").unwrap();
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Err("incomplete_native_record")
    );
}

#[test]
fn native_anchor_and_source_identity_are_exact() {
    let fixture = Fixture::new();
    fixture.event("task_started", Some("old"));
    fixture.event("task_complete", Some("old"));
    let mut anchor = fixture.anchor();
    anchor.native_turn_id = "foreign".into();
    assert_eq!(
        terminal_end(&fixture.source, &anchor, fixture.len()),
        Err("native_anchor_mismatch")
    );
    let mut source = fixture.source.clone();
    source.session_id = "other-source".into();
    assert_eq!(
        scan_anchor(&source, fixture.header_end, fixture.len()),
        Err("source_session_mismatch")
    );
    #[cfg(unix)]
    {
        let replacement = fixture.source.path.with_extension("replacement");
        std::fs::write(&replacement, std::fs::read(&fixture.source.path).unwrap()).unwrap();
        std::fs::rename(replacement, &fixture.source.path).unwrap();
        assert_eq!(
            scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
            Err("source_descriptor_mismatch")
        );
    }
}

#[cfg(unix)]
#[test]
fn dev_only_renumber_retains_original_source_with_checked_prefix() {
    let fixture = Fixture::new();
    let (start, _) = fixture.event("task_started", Some("old"));
    let _renumber =
        crate::services::tui_o::shadow::capture::renumber::shift(&fixture.source.path, 0x4000);
    assert_eq!(
        scan_anchor(&fixture.source, fixture.header_end, fixture.len()),
        Ok(Some(NativeTurnAnchor {
            native_turn_id: "old".into(),
            start,
        }))
    );
}
