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
    scan(path, &mut file, &metadata, after_block).unwrap()
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
