use super::*;
use std::cell::Cell;

thread_local! {
    static PARSES: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone)]
struct Counted(BindingEvent);

impl<'de> Deserialize<'de> for Counted {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        PARSES.with(|count| count.set(count.get() + 1));
        BindingEvent::deserialize(deserializer).map(Self)
    }
}

fn event(seq: u64) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: 7572,
        provider: "codex".into(),
        tmux_session: "cache-fixture".into(),
        execution_nonce: None,
        old: None,
        new: BindingTarget::Pending {
            payload_session_id: format!("session-{seq}"),
            payload_transcript_path: None,
        },
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: None,
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    }
}

fn append(writer: &mut Writer, path: &Path, seq: u64) {
    writer.append(path, &event(seq), false, None, None).unwrap();
}

#[test]
fn unchanged_log_is_not_reparsed_and_append_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("7572.log");
    let mut writer = Writer::default();
    append(&mut writer, &path, 1);
    assert_eq!(read_log::<Counted>(&path).unwrap().records[0].0.seq, 1);
    PARSES.with(|count| count.set(0));
    for _ in 0..10 {
        assert_eq!(read_log::<Counted>(&path).unwrap().records.len(), 1);
    }
    assert_eq!(
        PARSES.with(Cell::get),
        0,
        "unchanged logs must not be parsed"
    );
    append(&mut writer, &path, 2);
    let read = read_log::<Counted>(&path).unwrap();
    assert_eq!(read.records.last().unwrap().0.seq, 2);
    assert_eq!(PARSES.with(Cell::get), 2);
}

#[test]
fn failed_append_invalidates_even_when_rollback_restores_file_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("7572.log");
    let mut writer = Writer::default();
    append(&mut writer, &path, 1);
    read_log::<Counted>(&path).unwrap();
    let before = fs::metadata(&path).unwrap();
    APPEND_FAULT.with(|fault| fault.set(Some("sync")));
    let result = writer.append(&path, &event(2), false, None, None);
    APPEND_FAULT.with(|fault| fault.set(None));
    assert!(result.is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), before.len());
    filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .unwrap();
    PARSES.with(|count| count.set(0));
    assert_eq!(read_log::<Counted>(&path).unwrap().records.len(), 1);
    assert_eq!(
        PARSES.with(Cell::get),
        1,
        "every write attempt invalidates cached reads"
    );
}

#[test]
fn torn_tail_recovery_sees_new_event_with_identical_length_and_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("7572.log");
    append(&mut Writer::default(), &path, 1);
    let second = event(2);
    let mut tail = serde_json::to_vec(&LoggedRef {
        event: &second,
        verified: false,
        published_at: None,
        codex_ownership: None,
    })
    .unwrap();
    tail.push(b' ');
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&tail)
        .unwrap();
    let before = fs::metadata(&path).unwrap();
    assert_eq!(read_log::<Counted>(&path).unwrap().records.len(), 1);
    let mut writer = Writer::load(&path).unwrap();
    writer.append(&path, &second, false, None, None).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), before.len());
    filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .unwrap();
    let read = read_log::<Counted>(&path).unwrap();
    assert_eq!(
        read.records.len(),
        2,
        "recovery can reuse the old length within one timestamp tick"
    );
    assert_eq!(read.records[1].0.seq, 2);
}

#[test]
fn external_changes_missing_files_and_parser_types_are_observed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("7572.log");
    assert_eq!(read_log::<Counted>(&path).unwrap().total_len, 0);
    append(&mut Writer::default(), &path, 1);
    assert_eq!(read_log::<Counted>(&path).unwrap().records.len(), 1);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_within(..);
    fs::write(&path, bytes).unwrap();
    assert_eq!(read_log::<Counted>(&path).unwrap().records.len(), 2);
    let mut record = serde_json::to_value(event(3)).unwrap();
    record["codex_ownership"] = serde_json::json!("malformed");
    fs::write(&path, format!("{record}\ncorrupt\ntorn")).unwrap();
    let read = read_log::<BindingEvent>(&path).unwrap();
    assert_eq!((read.records.len(), read.lines), (1, 2));
    assert_eq!(read.total_len - read.complete_len, 4);
    let logged = read_log::<Logged>(&path).unwrap();
    assert_eq!((logged.records.len(), logged.lines), (0, 2));
    fs::remove_file(&path).unwrap();
    assert!(read_log::<Counted>(&path).unwrap().records.is_empty());
}

#[test]
fn equal_length_external_rewrite_is_detected_by_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("7572.log");
    append(&mut Writer::default(), &path, 1);
    let bytes = fs::read(&path).unwrap();
    assert_eq!(read_log::<Counted>(&path).unwrap().records[0].0.seq, 1);
    let rewritten = String::from_utf8(bytes)
        .unwrap()
        .replace("\"seq\":1", "\"seq\":2");
    let before = fs::metadata(&path).unwrap();
    fs::write(&path, rewritten).unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1, 0)).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), before.len());
    assert_eq!(read_log::<Counted>(&path).unwrap().records[0].0.seq, 2);
}
