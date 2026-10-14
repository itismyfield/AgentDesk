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

fn activity_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'"' {
            let hashes = bytes[..i].iter().rev().take_while(|b| **b == b'#').count();
            let raw = i > hashes && bytes[i - hashes - 1] == b'r';
            i += 1;
            while i < bytes.len() {
                if !raw && bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"'
                    && (!raw || bytes[i + 1..].starts_with(&vec![b'#'; hashes]))
                {
                    i += 1 + if raw { hashes } else { 0 };
                    break;
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'\'' {
            let mut end = i + 1;
            if bytes.get(end) == Some(&b'\\') {
                end += 2;
                if bytes.get(i + 2) == Some(&b'u') && bytes.get(end) == Some(&b'{') {
                    while end < bytes.len() && bytes[end] != b'}' {
                        end += 1;
                    }
                    end += 1;
                } else if bytes.get(i + 2) == Some(&b'x') {
                    end += 2;
                }
            } else if let Some(ch) = source[end..].chars().next() {
                end += ch.len_utf8();
            }
            if bytes.get(end) != Some(&b'\'') {
                i += 1;
                continue;
            }
            i = end + 1;
        } else {
            i += 1;
            continue;
        }
        for byte in &mut code[start..i] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(code).unwrap()
}

fn test_activity_item_end(source: &str, start: usize) -> Option<usize> {
    let head = &source[start..];
    let prefix = r"^\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?";
    let body = regex::Regex::new(&format!(
        r"{prefix}(?:(?:async|unsafe|const)\s+)*(?:fn|mod|impl|struct|enum|trait|union|extern|macro_rules|if|match|loop|while|for)\b|^\s*\{{"
    ))
    .unwrap()
    .is_match(head);
    let statement = regex::Regex::new(&format!(r"{prefix}(?:const|static|type|use|let)\b"))
        .unwrap()
        .is_match(head);
    let field = regex::Regex::new(&format!(r"{prefix}[A-Za-z_][A-Za-z_0-9]*\s*[:,]"))
        .unwrap()
        .is_match(head);
    if !body && !statement && !field {
        return None;
    }
    let tokens = regex::Regex::new(r"[A-Za-z_][A-Za-z_0-9]*|[^\s]").unwrap();
    let mut closers = Vec::new();
    let mut body_opened = false;
    // Only Rust delimiters nest; comparison and shift operators do not.
    for token in tokens.find_iter(head) {
        match token.as_str() {
            "(" => closers.push(")"),
            "[" => closers.push("]"),
            "{" => {
                body_opened |= body && closers.is_empty();
                closers.push("}");
            }
            ")" | "]" | "}" => {
                if closers.pop() != Some(token.as_str()) {
                    return None;
                }
                if body_opened && closers.is_empty() {
                    return Some(start + token.end());
                }
            }
            ";" if closers.is_empty() => return Some(start + token.end()),
            "," if field && !body && !statement && closers.is_empty() => {
                return Some(start + token.end());
            }
            _ => {}
        }
    }
    None
}

fn dormant_activity_modules(source: &str) -> bool {
    let mut code = activity_code(source).into_bytes();
    let original = String::from_utf8(code.clone()).unwrap();
    let gates = regex::Regex::new(r"#\s*\[\s*cfg\s*\(([^\]]*)\)\s*\]").unwrap();
    for gate in gates.captures_iter(&original) {
        if code[gate.get(0).unwrap().start()] != b'#' {
            continue;
        }
        let condition: String = gate[1].chars().filter(|c| !c.is_whitespace()).collect();
        if condition != "test" && !condition.starts_with("all(test,") {
            continue;
        }
        let start = gate.get(0).unwrap().start();
        let Some(end) = test_activity_item_end(&original, gate.get(0).unwrap().end()) else {
            return false;
        };
        code[start..end].fill(b' ');
    }
    let production = String::from_utf8(code).unwrap();
    !regex::Regex::new(r"\bmod\s+(tail|unbound)\s*;|\b(tail|unbound)\s*::")
        .unwrap()
        .is_match(&production)
}

#[test]
fn s0o_limits_and_dormant_activity_source_are_unchanged() {
    let limits = Limits::default();
    assert_eq!(limits.tail, 32 * 1024 * 1024);
    assert_eq!(limits.tail_start(limits.full), None);
    assert_eq!(
        limits.tail_start(limits.full + 1),
        Some(limits.full + 1 - limits.tail)
    );
    assert!(dormant_activity_modules(include_str!("../activity.rs")));
    assert!(dormant_activity_modules(
        "#[cfg(all(test, unix))] mod tail; #[cfg(test)] mod unbound;"
    ));
    assert!(dormant_activity_modules(
        "fn live() { let _ = r###\"tail::x }\"###; } // unbound::x"
    ));
    assert!(dormant_activity_modules(
        "#[cfg(test)] fn t() { let _ = '{'; } fn live() {}"
    ));
    for source in [
        "mod tail;",
        "#[cfg(not(test))] mod unbound;",
        "fn live() { tail::x(); }",
        "#[cfg(test)] mod t {} fn live() { unbound::x(); }",
        "#[cfg(test)] mod t { #[cfg(test)] fn nested() {} } fn live() { tail::x(); }",
        "#[cfg(any(test, unix))] mod unbound;",
    ] {
        assert!(!dormant_activity_modules(source), "{source}");
    }
    for source in [
        "#[cfg(test)] const _: bool = 1 < 2; fn live() {}",
        "#[cfg(test)] const _: u32 = 1 << 2; fn live() {}",
        "#[cfg(test)] const _: u32 = 1 >> 2; fn live() {}",
        "#[cfg(test)] const _: bool = { 1 < 2 }; fn live() {}",
        "#[cfg(test)] fn generic<T, U>(_: T) -> Option<U> { tail::x(); } fn live() {}",
        "#[cfg(test)] type Generic<T, U> = Result<T, U>; fn live() {}",
        "#[cfg(test)] mod t { #[cfg(test)] const _: bool = 1 < 2; fn nested<T, U>() { unbound::x(); } } fn live() {}",
    ] {
        assert!(dormant_activity_modules(source), "{source}");
    }
    let unsafe_suffixes = [
        "#[cfg(test)] const _: bool = 1 < 2; mod tail;",
        "#[cfg(test)] const _: bool = 1 < 2; fn live() { tail::x(); }",
        "#[cfg(test)] const _: bool = 1 < 2; fn live() { unbound::x(); }",
        "#[cfg(test)] const _: u32 = 1 << 2; mod unbound;",
        "#[cfg(test)] const _: u32 = 1 << 2; fn live() { tail::x(); }",
        "#[cfg(test)] const _: u32 = 1 >> 2; fn live() { unbound::x(); }",
    ];
    let accepted: Vec<_> = unsafe_suffixes
        .into_iter()
        .filter(|source| dormant_activity_modules(source))
        .collect();
    assert!(
        accepted.is_empty(),
        "unsafe non-test suffix accepted: {accepted:?}"
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
