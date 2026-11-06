use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::Write;

const LIMIT: u64 = 64 * 1024 * 1024;
const BLOCK: u64 = 64 * 1024;

fn event(payload: Value) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"event_msg","payload":payload})
    )
}
fn started(turn: &str) -> String {
    event(serde_json::json!({"type":"task_started","turn_id":turn,"root_turn_id":turn}))
}
fn item(turn: &str, kind: &str) -> String {
    event(serde_json::json!({"type":"item_completed","turn_id":turn,"item":{"type":kind}}))
}
fn model(turn: &str) -> String {
    started(turn)
        + &format!(
            "{}\n",
            serde_json::json!({"type":"turn_context","payload":{"turn_id":turn,"sandbox_policy":{"type":"danger-full-access"}}})
        )
        + &item(turn, "UserMessage")
}
fn ended(turn: &str) -> String {
    event(serde_json::json!({"type":"task_complete","turn_id":turn}))
}
fn temporary(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    fs::write(&path, contents).unwrap();
    (directory, path)
}
fn append(path: &Path, contents: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
}
fn pad_to(path: &Path, total: u64) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    let opening = b"{\"type\":\"padding\",\"payload\":{\"text\":\"";
    let closing = b"\"}}\n";
    let mut remaining =
        total - file.metadata().unwrap().len() - (opening.len() + closing.len()) as u64;
    file.write_all(opening).unwrap();
    let block = vec![b'x'; BLOCK as usize];
    while remaining > 0 {
        let len = remaining.min(BLOCK) as usize;
        file.write_all(&block[..len]).unwrap();
        remaining -= len as u64;
    }
    file.write_all(closing).unwrap();
}
fn snapshot(path: &Path, after_block: impl FnMut(u64)) -> TurnRead {
    let mut file = File::open(path).unwrap();
    let metadata = file.metadata().unwrap();
    scan(path, &mut file, &metadata, after_block, false).unwrap()
}

#[test]
fn nine_megabyte_open_turn_reads_past_the_old_fixed_tail() {
    let (_dir, path) = temporary(&model("long"));
    pad_to(&path, 9 * 1024 * 1024);
    let read = read_turn_detail(&path).unwrap();
    assert_eq!(read.verdict, TurnVerdict::KnownModelTurn("long".into()));
    assert_eq!(read.bytes_read, 9 * 1024 * 1024);
}

#[test]
fn a_complete_start_exactly_at_the_cap_boundary_is_not_skipped() {
    let (_dir, path) = temporary("{}\n");
    append(
        &path,
        &(model("edge")
            + &item("review", "EnteredReviewMode")
            + &item("review", "ExitedReviewMode")),
    );
    pad_to(&path, LIMIT + 3);
    let began = std::time::Instant::now();
    let read = read_turn_detail(&path).unwrap();
    eprintln!(
        "scan_cap bytes={} elapsed_ms={}",
        read.bytes_read,
        began.elapsed().as_millis()
    );
    assert_eq!(read.verdict, TurnVerdict::KnownModelTurn("edge".into()));
    assert_eq!(read.bytes_read, LIMIT);
}

#[test]
fn a_start_older_than_the_cap_is_unknown_without_unbounded_reading() {
    let (_dir, path) = temporary(&model("too-old"));
    pad_to(&path, LIMIT + 1);
    let read = read_turn_detail(&path).unwrap();
    assert_eq!(read.verdict, TurnVerdict::Unknown);
    assert_eq!(read.reason, "turn_scan_limit");
    assert_eq!(read.bytes_read, LIMIT);
}

#[test]
fn a_recent_start_without_a_review_seed_or_bof_stays_unknown() {
    let (_dir, path) = temporary("");
    pad_to(&path, 56 * 1024 * 1024);
    append(&path, &model("recent"));
    pad_to(&path, 65 * 1024 * 1024);
    let began = std::time::Instant::now();
    let read = read_turn_detail(&path).unwrap();
    eprintln!(
        "scan_review_seed_cap bytes={} elapsed_ms={}",
        read.bytes_read,
        began.elapsed().as_millis()
    );
    assert_eq!(read.verdict, TurnVerdict::Unknown);
    assert_eq!(read.reason, "review_seed_unknown");
    assert_eq!(read.bytes_read, LIMIT);
}

#[test]
fn a_bounded_review_seed_only_accepts_its_own_exit() {
    for exit in ["review", "foreign"] {
        let (_dir, path) = temporary("");
        pad_to(&path, 56 * 1024 * 1024);
        append(
            &path,
            &(item("review", "EnteredReviewMode")
                + &item(exit, "ExitedReviewMode")
                + &model("recent")),
        );
        pad_to(&path, 65 * 1024 * 1024);
        let read = read_turn_detail(&path).unwrap();
        let expected = if exit == "review" {
            TurnVerdict::KnownModelTurn("recent".into())
        } else {
            TurnVerdict::NonSteerable("recent".into())
        };
        assert_eq!(read.verdict, expected);
        assert!(read.bytes_read < LIMIT);
    }
}

#[test]
fn json_and_utf8_crossing_a_block_boundary_remain_complete() {
    let (_dir, path) = temporary(&model("경계한글"));
    let body = model("경계한글");
    let unicode = body.find('경').unwrap() as u64;
    for total in [
        BLOCK + unicode + 1,
        BLOCK + unicode + 2,
        2 * BLOCK + unicode + 1,
    ] {
        fs::write(&path, &body).unwrap();
        pad_to(&path, total);
        assert!(matches!((total - unicode) % BLOCK, 1 | 2));
        assert_eq!(
            read_turn(&path),
            Some(TurnVerdict::KnownModelTurn("경계한글".into()))
        );
    }
}

#[test]
fn growth_during_a_scan_is_read_only_by_the_next_snapshot() {
    let (_dir, path) = temporary(&model("growing"));
    pad_to(&path, 2 * BLOCK);
    let mut grown = false;
    let read = snapshot(&path, |_| {
        if !grown {
            append(&path, &ended("growing"));
            grown = true;
        }
    });
    assert_eq!(read.verdict, TurnVerdict::KnownModelTurn("growing".into()));
    assert_eq!(read.bytes_read, 2 * BLOCK);
    assert_eq!(read_turn(&path), Some(TurnVerdict::NotBusy));
}

#[test]
fn truncation_during_a_scan_does_not_poison_the_next_fresh_read() {
    let (_dir, path) = temporary(&model("old"));
    pad_to(&path, 2 * BLOCK);
    let mut changed = false;
    let read = snapshot(&path, |_| {
        if !changed {
            fs::write(&path, model("new")).unwrap();
            changed = true;
        }
    });
    assert_eq!(read.verdict, TurnVerdict::Unknown);
    assert_eq!(read.reason, "source_changed");
    assert_eq!(
        read_turn(&path),
        Some(TurnVerdict::KnownModelTurn("new".into()))
    );
}

#[test]
fn replacement_during_a_scan_does_not_reuse_the_old_file() {
    let (_dir, path) = temporary(&model("old"));
    pad_to(&path, 2 * BLOCK);
    let replacement = path.with_extension("replacement");
    fs::write(&replacement, model("replacement")).unwrap();
    let mut changed = false;
    let read = snapshot(&path, |_| {
        if !changed {
            fs::rename(&replacement, &path).unwrap();
            changed = true;
        }
    });
    assert_eq!(read.verdict, TurnVerdict::Unknown);
    assert_eq!(read.reason, "source_changed");
    assert_eq!(
        read_turn(&path),
        Some(TurnVerdict::KnownModelTurn("replacement".into()))
    );
}

#[test]
fn no_start_and_incomplete_eof_records_preserve_their_distinct_verdicts() {
    let (_dir, path) = temporary("");
    assert_eq!(read_turn(&path), Some(TurnVerdict::NotBusy));
    fs::write(&path, model("open") + ended("open").trim_end()).unwrap();
    assert_eq!(
        read_turn(&path),
        Some(TurnVerdict::KnownModelTurn("open".into()))
    );
    fs::write(&path, model("old") + &started("latest") + &ended("latest")).unwrap();
    assert_eq!(read_turn(&path), Some(TurnVerdict::NotBusy));
}

#[test]
fn manual_compaction_requires_positive_fixture_evidence_and_obeys_its_end() {
    let fixture = include_str!("../../../../tests/fixtures/codex_busy_inject/compact_turn.jsonl");
    let turn = "01a11dcc-3da7-7a23-9f47-5067834fa221";
    for (count, expected) in [
        (1, TurnVerdict::NonSteerable(turn.into())),
        (3, TurnVerdict::KnownModelTurn(turn.into())),
        (6, TurnVerdict::KnownModelTurn(turn.into())),
        (7, TurnVerdict::NotBusy),
    ] {
        let contents = fixture.lines().take(count).collect::<Vec<_>>().join("\n") + "\n";
        let (_dir, path) = temporary(&contents);
        assert_eq!(read_turn(&path), Some(expected), "fixture lines={count}");
    }
    let begun = started("compacting")
        + &event(
            serde_json::json!({"type":"item_started","turn_id":"compacting","item":{"type":"ContextCompaction"}}),
        );
    let (_dir, path) = temporary(&begun);
    assert_eq!(
        read_turn(&path),
        Some(TurnVerdict::KnownModelTurn("compacting".into()))
    );
    fs::write(&path, started("shell") + &item("shell", "CommandExecution")).unwrap();
    assert_eq!(
        read_turn(&path),
        Some(TurnVerdict::NonSteerable("shell".into()))
    );
}

/// Only the latest turn's own complete or abort is its end; another turn's end, an unnamed or
/// unreadable record, a torn last line or a rollout with no start proves none.
#[test]
fn a_native_end_is_only_the_latest_turns_own() {
    let abort = |turn: &str| event(serde_json::json!({"type":"turn_aborted","turn_id":turn}));
    let unnamed = event(serde_json::json!({"type":"task_complete"}));
    let unrooted = event(serde_json::json!({"type":"task_started","turn_id":"t"}));
    let meta = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n";
    let cases = [
        ("open", model("t"), NativeEnd::Open),
        ("own complete", model("t") + &ended("t"), NativeEnd::Ended),
        ("own abort", model("t") + &abort("t"), NativeEnd::Ended),
        (
            "others' ends",
            model("t") + &ended("u") + &abort("u"),
            NativeEnd::Open,
        ),
        ("unnamed end", model("t") + &unnamed, NativeEnd::Unknown),
        (
            "unnamed, then own",
            model("t") + &unnamed + &ended("t"),
            NativeEnd::Ended,
        ),
        (
            "malformed, then own",
            model("t") + "{torn\n" + &ended("t"),
            NativeEnd::Unknown,
        ),
        (
            "unrooted start",
            unrooted.clone() + &ended("t"),
            NativeEnd::Unknown,
        ),
        (
            "unrooted, then a start",
            unrooted + &model("u"),
            NativeEnd::Open,
        ),
        (
            "torn new start",
            model("t") + &ended("t") + &started("u")[..30],
            NativeEnd::Unknown,
        ),
        ("meta only", meta.to_string(), NativeEnd::NoStart),
        ("empty", String::new(), NativeEnd::NoStart),
        (
            "an end with no start",
            meta.to_string() + &ended("t"),
            NativeEnd::NoStart,
        ),
    ];
    for (case, contents, expected) in cases {
        let (_dir, path) = temporary(&contents);
        assert_eq!(read_native_end(&path), expected, "{case}");
    }
}

/// An end read stops at the latest turn's start, so a long rollout before it is not read again.
#[test]
fn an_end_read_stops_at_the_latest_start() {
    let (_dir, path) = temporary("{}\n");
    pad_to(&path, 4 * 1024 * 1024);
    let turn = model("t") + &ended("t");
    append(&path, &turn);
    let mut file = File::open(&path).unwrap();
    let metadata = file.metadata().unwrap();
    let read = scan(&path, &mut file, &metadata, |_| {}, true).unwrap();
    assert_eq!(read.end, NativeEnd::Ended);
    assert!(
        read.bytes_read <= turn.len() as u64 + BLOCK,
        "{}",
        read.bytes_read
    );
}

/// An unnamed end after the turn's own end leaves its end unproven until another own end, by
/// complete, abort or the root's id; the legacy verdict of the same records stays Unknown.
#[test]
fn an_unnamed_end_after_the_turns_own_is_resolved_only_by_a_later_own_end() {
    let rooted = event(serde_json::json!({"type":"task_started","turn_id":"t","root_turn_id":"r"}));
    let ends: [(&str, &str, fn(&str) -> String, String); 3] = [
        (
            "complete",
            "t",
            ended,
            event(serde_json::json!({"type":"task_complete"})),
        ),
        (
            "abort",
            "t",
            |turn| event(serde_json::json!({"type":"turn_aborted","turn_id":turn})),
            event(serde_json::json!({"type":"turn_aborted"})),
        ),
        (
            "root",
            "r",
            ended,
            event(serde_json::json!({"type":"task_complete"})),
        ),
    ];
    for (kind, id, own, unnamed) in ends {
        let begin = if kind == "root" {
            rooted.clone()
        } else {
            model("t")
        } + &own(id)
            + &unnamed;
        let cases = [
            ("own, unnamed", begin.clone(), NativeEnd::Unknown),
            (
                "own, unnamed, other",
                begin.clone() + &ended("u"),
                NativeEnd::Unknown,
            ),
            (
                "own, unnamed, own",
                begin.clone() + &own(id),
                NativeEnd::Ended,
            ),
        ];
        for (case, contents, expected) in cases {
            let (_dir, path) = temporary(&contents);
            assert_eq!(read_native_end(&path), expected, "{kind}: {case}");
        }
        let (_dir, path) = temporary(&(begin + &own(id)));
        assert_eq!(
            read_turn(&path),
            Some(TurnVerdict::Unknown),
            "{kind}: legacy"
        );
    }
}
