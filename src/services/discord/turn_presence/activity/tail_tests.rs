use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::io_trace;
use serde_json::{Value, json};
use std::io::Write;

fn append(path: &std::path::Path, rows: &[Value]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for row in rows {
        writeln!(f, "{row}").unwrap();
    }
}
fn event(kind: &str, id: Option<&str>) -> Value {
    let mut row = json!({"type":"event_msg","payload":{"type":kind}});
    if let Some(id) = id {
        row["payload"]["turn_id"] = id.into();
    }
    row
}
fn blocked() -> Value {
    json!({"type":"response_item","payload":{"type":"unknown_item"}})
}
fn fixture(provider: ShadowProvider, rows: &[Value]) -> (tempfile::TempDir, SourceBinding) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    std::fs::write(&path, "").unwrap();
    if provider == ShadowProvider::Codex {
        append(
            &path,
            &[json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}})],
        );
    }
    append(&path, rows);
    let binding = SourceBinding {
        channel_id: 61,
        provider,
        source: source_id_for("parent", &path).unwrap(),
    };
    (root, binding)
}
fn limits(worker: u64) -> Limits {
    Limits {
        full: 1,
        tail: 4096,
        poll: 4096,
        worker,
    }
}
fn idle(reader: &mut TailReader) -> bool {
    let fact = reader.poll_installed().unwrap();
    reader.meta.allows_idle(
        reader.facts.binding().provider,
        &fact,
        reader.facts.caught_up().unwrap(),
    )
}

#[test]
fn s1_tail_shrink_ahead_of_cursor_is_not_eof() {
    let rows = [event("task_started", Some("A")), event("task_complete", Some("A")),
        event("task_started", Some("B"))];
    let (_root, binding) = fixture(ShadowProvider::Codex, &rows);
    let mut reader = TailReader::open(binding.clone(), limits(1)).unwrap();
    let end = std::fs::metadata(&binding.source.path).unwrap().len();
    std::fs::OpenOptions::new().write(true).open(&binding.source.path).unwrap()
        .set_len(end - serde_json::to_vec(&rows[2]).unwrap().len() as u64 - 1).unwrap();
    assert!(reader.bootstrap().is_err(), "snapshot shrink ahead of cursor was accepted");
}

#[test]
fn s1_resume_revalidates_old_guard_before_carry() {
    for tamper in [false, true] {
        let (_root, binding) = fixture(ShadowProvider::Codex, &[
            event("task_started", Some("A")), blocked(), event("task_complete", Some("A"))]);
        let mut reader = TailReader::open(binding.clone(), limits(4096)).unwrap();
        assert!(reader.poll(4096, Some(reader.meta.bootstrap_end)).is_err());
        assert!(reader.facts.resume_point().is_some());
        if tamper {
            let mut bytes = std::fs::read(&binding.source.path).unwrap();
            let at = bytes.windows(3).position(|w| w == b"\"A\"").unwrap();
            bytes[at + 1] = b'B';
            std::fs::write(&binding.source.path, bytes).unwrap();
        }
        let resumed = reader.resume();
        assert_eq!(resumed.is_err(), tamper, "old observation must authenticate the handoff");
        if !tamper { assert!(idle(&mut reader)); }
    }
}

#[test]
fn s0o_codex_lifetime_across_worker_installed_and_typed_resume() {
    let start = event("task_started", Some("A"));
    let close = event("task_complete", Some("A"));
    for rows in [
        vec![start.clone(), close.clone()],
        vec![
            start.clone(),
            blocked(),
            blocked(),
            event("turn_aborted", Some("B")),
            close.clone(),
        ],
    ] {
        for worker in [1, 100, u64::MAX] {
            let (_root, binding) = fixture(ShadowProvider::Codex, &rows);
            let mut reader = TailReader::open(binding, limits(worker)).unwrap();
            reader.bootstrap().unwrap();
            assert!(idle(&mut reader), "worker={worker} rows={rows:?}");
            let CodexBoundary::Closed { id, opener, closer } = &reader.meta.codex else {
                panic!("missing closed evidence")
            };
            assert_eq!(id, "A");
            assert!(opener.1 <= closer.0);
            assert!(idle(&mut reader), "empty poll keeps evidence");
        }
    }
    let (_root, binding) = fixture(ShadowProvider::Codex, &[start]);
    let mut reader = TailReader::open(binding.clone(), limits(100)).unwrap();
    reader.bootstrap().unwrap();
    assert!(!idle(&mut reader));
    append(&binding.source.path, &[event("turn_aborted", Some("B"))]);
    assert!(!idle(&mut reader));
    append(&binding.source.path, std::slice::from_ref(&close));
    assert!(idle(&mut reader));
    append(
        &binding.source.path,
        &[close, event("task_complete", Some("B"))],
    );
    assert!(!idle(&mut reader), "B cannot reuse A's opener");
    append(
        &binding.source.path,
        &[
            event("task_started", Some("C")),
            event("task_complete", Some("C")),
        ],
    );
    assert!(idle(&mut reader));
}

#[test]
fn s0o_anonymous_boundary_invalidates_closed_before_repeated_a_close() {
    for anonymous in ["task_started", "task_complete"] {
        let rows = [
            event("task_started", Some("A")),
            event("task_complete", Some("A")),
        ];
        let (_root, binding) = fixture(ShadowProvider::Codex, &rows);
        let mut reader = TailReader::open(binding.clone(), limits(4096)).unwrap();
        reader.bootstrap().unwrap();
        assert!(idle(&mut reader));
        append(&binding.source.path, &[event(anonymous, None)]);
        assert!(!idle(&mut reader), "{anonymous}");
        append(&binding.source.path, &[event("task_complete", Some("A"))]);
        assert!(!idle(&mut reader), "old A evidence must stay invalidated");
        assert_eq!(reader.meta.codex, CodexBoundary::Unproven);
    }
}

#[test]
fn s0o_resume_mismatch_and_new_source_discard_evidence() {
    let (_root, binding) = fixture(ShadowProvider::Codex, &[event("task_started", Some("A"))]);
    let mut reader = TailReader::open(binding, limits(4096)).unwrap();
    reader.bootstrap().unwrap();
    reader.meta.carry(&Resume {
        after: 0,
        carried: TurnState::Open {
            native_turn_id: Some("B".into()),
        },
    });
    assert_eq!(reader.meta.codex, CodexBoundary::Unproven);
    let (_new, binding) = fixture(ShadowProvider::Codex, &[event("task_complete", Some("A"))]);
    let mut next = TailReader::open(binding, limits(4096)).unwrap();
    next.bootstrap().unwrap();
    assert!(!idle(&mut next));
}

#[test]
fn s0o_safety_open_empty_metadata_and_partial_eof() {
    let end = json!({"type":"system","subtype":"turn_duration"});
    let start = json!({"type":"user","uuid":"A","message":{"content":"go"}});
    for rows in [
        vec![],
        vec![json!({"type":"summary","summary":"metadata"})],
        vec![start.clone()],
        vec![end.clone(), start.clone()],
    ] {
        let (_root, binding) = fixture(ShadowProvider::Claude, &rows);
        let mut reader = TailReader::open(binding, limits(4096)).unwrap();
        reader.bootstrap().unwrap();
        assert!(!idle(&mut reader));
    }
    let (_root, binding) = fixture(ShadowProvider::Claude, &[end]);
    let mut reader = TailReader::open(binding.clone(), limits(4096)).unwrap();
    reader.bootstrap().unwrap();
    assert!(idle(&mut reader));
    let text = start.to_string();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&binding.source.path)
        .unwrap();
    f.write_all(&text.as_bytes()[..10]).unwrap();
    assert!(!idle(&mut reader));
    f.write_all(&text.as_bytes()[10..]).unwrap();
    f.write_all(b"\n").unwrap();
    assert!(!idle(&mut reader));
    assert!(matches!(
        reader.facts.poll(4096).unwrap().state,
        TurnState::Open { .. }
    ));
}

#[test]
fn s0o_snapshot_budget_counts_alignment_and_resume_rereads() {
    let (_root, binding) = fixture(ShadowProvider::Codex, &[event("task_started", Some("A"))]);
    let mut reader = TailReader::open(binding.clone(), limits(100)).unwrap();
    let snapshot = reader.meta.bootstrap_end;
    append(&binding.source.path, &[event("task_complete", Some("A"))]);
    io_trace::take();
    reader.bootstrap().unwrap();
    assert!(io_trace::take().iter().all(|(_, end, _)| *end <= snapshot));
    assert!(!reader.facts.caught_up().unwrap());
    assert!(
        idle(&mut reader),
        "installed reader catches append after snapshot"
    );

    let rows = [
        event("task_started", Some("A")),
        blocked(),
        blocked(),
        event("task_complete", Some("A")),
    ];
    let (_root, binding) = fixture(ShadowProvider::Codex, &rows);
    let len = std::fs::metadata(&binding.source.path).unwrap().len();
    let small = Limits {
        full: 1,
        tail: len,
        poll: 4096,
        worker: len,
    };
    let mut reader = TailReader::open(binding, small).unwrap();
    io_trace::take();
    reader.bootstrap().unwrap();
    assert_eq!(reader.remaining, 0);
    assert!(!reader.facts.caught_up().unwrap());
    assert!(io_trace::take().iter().all(|(_, _, hash)| !hash));
    assert!(
        !reader.facts.awaiting_boundary()
            || matches!(reader.meta.codex, CodexBoundary::Open { .. })
    );
    assert!(reader.poll_installed().is_err());
    reader.resume().unwrap();
    assert!(idle(&mut reader));
}

#[test]
fn s0o_limits_and_dormant_activity_source_are_unchanged() {
    use sha2::{Digest, Sha256};
    let limits = Limits::default();
    assert_eq!(limits.tail, 32 * 1024 * 1024);
    assert_eq!(limits.tail_start(limits.full), None);
    assert_eq!(
        limits.tail_start(limits.full + 1),
        Some(limits.full + 1 - limits.tail)
    );
    let source = include_str!("../activity.rs").replace("\r\n", "\n");
    let production = source
        .strip_suffix("\n#[cfg(all(test, unix))]\nmod tail;\n")
        .unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(production.as_bytes())),
        "8c42aa4c34e6c5f8b0f5e61973751ad451637cf94a86312a8bfd58a017bdda0b"
    );
}

#[test]
fn s0o_closed_resume_waits_and_parse_errors_discard_proof() {
    let rows = [
        event("task_started", Some("A")),
        event("task_complete", Some("A")),
        blocked(),
    ];
    let (_root, binding) = fixture(ShadowProvider::Codex, &rows);
    let mut reader = TailReader::open(binding.clone(), limits(4096)).unwrap();
    assert_eq!(reader.meta.window_start, 0);
    reader.bootstrap().unwrap();
    assert!(matches!(reader.meta.codex, CodexBoundary::Closed { .. }));
    assert!(reader.facts.awaiting_boundary());
    assert!(
        !idle(&mut reader),
        "closed proof cannot bypass awaiting_boundary"
    );
    append(&binding.source.path, &[event("task_complete", Some("A"))]);
    assert!(idle(&mut reader));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&binding.source.path)
        .unwrap()
        .write_all(b"broken\n")
        .unwrap();
    assert!(reader.poll_installed().is_err());
    assert!(reader.facts.resume_point().is_none());
    assert_eq!(reader.meta.codex, CodexBoundary::Unproven);
}
