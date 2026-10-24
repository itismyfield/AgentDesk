use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;

fn cursor(dir: &std::path::Path, bytes: &[u8]) -> Cursor {
    let path = dir.join("transcript.jsonl");
    std::fs::write(&path, bytes).unwrap();
    Cursor {
        source: source_id_for("session", &path).unwrap(),
        captured_through: bytes.len() as u64,
        prefix_hash: hex::encode(Sha256::digest(bytes)),
        retired: false,
        transition_cleanup: None,
    }
}

fn timestamp(raw: &str) -> SystemTime {
    SystemTime::from(
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc),
    )
}

#[test]
fn automatic_age_reads_captured_claude_rows_and_uses_the_latest_dialogue() {
    let fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hook_payload/claude-2.1.283.transcript-shapes.jsonl"
    ));
    let mut bytes = Vec::new();
    for line in fixture.lines() {
        let captured: serde_json::Value = serde_json::from_str(line).unwrap();
        serde_json::to_writer(&mut bytes, &captured["record"]).unwrap();
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(
        b"{\"type\":\"queue-operation\",\"timestamp\":\"2026-10-23T00:00:00Z\"}\n",
    );
    let dir = tempfile::tempdir().unwrap();
    let cursor = cursor(dir.path(), &bytes);
    assert_eq!(
        last_dialogue(&cursor),
        Some(timestamp("2026-09-27T12:06:35.524Z"))
    );
    assert_eq!(std::fs::read(&cursor.source.path).unwrap(), bytes);
}

#[test]
fn automatic_age_rejects_any_unreadable_dialogue_or_json_row() {
    let valid = b"{\"type\":\"assistant\",\"timestamp\":\"2026-09-27T00:00:00Z\"}\n";
    for invalid in [
        &b"{not-json}\n"[..],
        b"\n",
        b"{\"type\":\"user\"}\n",
        b"{\"type\":\"assistant\",\"timestamp\":\"bad\"}\n",
        b"{\"type\":\"user\",\"timestamp\":12}\n",
        b"{\"type\":\"user\",\"timestamp\":\"2026-09-28T00:00:00\"}\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let bytes = [valid.as_slice(), invalid].concat();
        assert_eq!(
            last_dialogue(&cursor(dir.path(), &bytes)),
            None,
            "{invalid:?}"
        );
    }
}

#[test]
fn automatic_age_keeps_sources_without_dialogue_or_a_complete_tail() {
    for bytes in [
        &b""[..],
        b"{\"type\":\"queue-operation\",\"timestamp\":\"2026-09-27T00:00:00Z\"}\n",
        b"{\"type\":\"assistant\",\"timestamp\":\"2026-09-27T00:00:00Z\"}",
    ] {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_dialogue(&cursor(dir.path(), bytes)), None);
    }
}

#[test]
fn automatic_age_requires_the_captured_file_identity_prefix_and_end_within_budget() {
    let bytes = b"{\"type\":\"user\",\"timestamp\":\"2026-09-27T00:00:00Z\"}\n";
    let dir = tempfile::tempdir().unwrap();
    let original = cursor(dir.path(), bytes);
    let mut changed = original.clone();
    changed.captured_through -= 1;
    assert_eq!(last_dialogue(&changed), None, "uncaptured tail");
    changed = original.clone();
    changed.prefix_hash = "wrong".into();
    assert_eq!(last_dialogue(&changed), None, "prefix changed");
    changed = original.clone();
    changed.source.ino = changed.source.ino.wrapping_add(1);
    assert_eq!(last_dialogue(&changed), None, "inode changed");
    changed = original;
    changed.captured_through = MAX_BYTES + 1;
    File::options()
        .write(true)
        .open(&changed.source.path)
        .unwrap()
        .set_len(changed.captured_through)
        .unwrap();
    assert_eq!(last_dialogue(&changed), None, "over the read budget");
}

#[test]
fn automatic_age_seven_days_is_inclusive_and_unknown_or_future_time_is_retained() {
    let last = timestamp("2026-09-27T00:00:00.123456789Z");
    assert!(!old_enough(None, last + MIN_AGE));
    assert!(!old_enough(Some(last), last - Duration::from_nanos(1)));
    assert!(!old_enough(
        Some(last),
        last + MIN_AGE - Duration::from_nanos(1)
    ));
    assert!(old_enough(Some(last), last + MIN_AGE));
    assert!(old_enough(
        Some(last),
        last + MIN_AGE + Duration::from_nanos(1)
    ));
}
